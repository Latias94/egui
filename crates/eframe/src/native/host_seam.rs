//! Narrow native event and renderer-settlement seam.
//!
//! This module deliberately does not expose renderer resources, platform handles, or an
//! application transaction protocol. A host observes immutable native events before egui input
//! translation and receives terminal presentation results for context-local output tokens.

use std::cell::RefCell;
use std::collections::{BTreeMap, VecDeque};
use std::num::NonZeroU64;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError, Weak};

use egui::{ViewportBuilder, ViewportId};
#[cfg(target_os = "linux")]
use raw_window_handle::{HasDisplayHandle as _, HasWindowHandle as _};
use winit::event::{DeviceId, PointerEventFacts, PointerWindowRoute};
use winit::event_loop::ActiveEventLoop;
#[cfg(all(target_os = "linux", feature = "x11"))]
use winit::platform::x11::{ActiveEventLoopExtX11 as _, WindowExtX11 as _, X11PointerObservation};
use winit::window::{Window, WindowAttributes, WindowId};

#[cfg(test)]
mod close_tests;
#[cfg(test)]
mod property_tests;
mod work_area;

use work_area::OwnedNativeWorkAreaRoster;
pub use work_area::{NativeDisplayId, NativeWorkAreaRecord, NativeWorkAreaRoster};

static NEXT_CONTEXT_ID: AtomicU64 = AtomicU64::new(1);
static NEXT_FOCUS_COMMAND: AtomicU64 = AtomicU64::new(1);
static NEXT_VISIBILITY_COMMAND: AtomicU64 = AtomicU64::new(1);
static NEXT_POINTER_PASSTHROUGH_COMMAND: AtomicU64 = AtomicU64::new(1);
static NEXT_POINTER_OBSERVATION_COMMAND: AtomicU64 = AtomicU64::new(1);

thread_local! {
    static ACTIVE_OUTPUTS: RefCell<Vec<ActiveNativeOutput>> = const { RefCell::new(Vec::new()) };
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ActiveNativeOutput {
    token: NativeOutputToken,
    render_mode: NativeOutputRenderMode,
    retain_eligible: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PresentedNativeOutputFrame {
    window_id: WindowId,
    inner_size: Option<(u32, u32)>,
    generation: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum NativeRetentionKind {
    Retainable,
    NoFrame,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct NativeRetentionRecord {
    /// Completion order protects a newer output fact from stale settlement.
    ordinal: NativeOutputOrdinal,
    frame: PresentedNativeOutputFrame,
    /// `NoFrame` is an exact tombstone for a fallback that replaced the framebuffer.
    kind: NativeRetentionKind,
}

impl NativeRetentionRecord {
    fn retains(self, frame: PresentedNativeOutputFrame) -> bool {
        self.kind == NativeRetentionKind::Retainable && self.frame == frame
    }
}

/// Renderer work allowed for one native output settlement.
///
/// This stays inside eframe: native hosts declare intent through the public
/// retain functions and never receive framebuffer or renderer authority.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum NativeOutputRenderMode {
    /// Replace the framebuffer with this generated output.
    Replace,
    /// Paint a fallback framebuffer without publishing presentation authority.
    NoFrame,
    /// Keep the current framebuffer and skip this output's paint and swap.
    Skip,
    /// Load the exact current framebuffer and paint this output over it.
    Overlay,
}

/// Monotonic order assigned to one native window event before egui translates it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NativeEventOrdinal(NonZeroU64);

impl NativeEventOrdinal {
    /// Returns the process-local ordinal.
    pub const fn get(self) -> u64 {
        self.0.get()
    }
}

/// Exact attachment-local native close request.
///
/// A host may hold this request before egui input translation and resolve it in
/// a later update without inventing a second close event or cancellation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct NativeViewportCloseRequest {
    attachment: NativeHostAttachment,
    event: NativeEventOrdinal,
    viewport_id: ViewportId,
    window_id: WindowId,
}

/// Host ownership of one immutable native event before input translation.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum NativeWindowEventDisposition {
    /// Preserve ordinary eframe processing of this event.
    #[default]
    Continue,
    /// Retain this exact close request without forwarding it as egui input.
    ///
    /// Only the request carried by this callback is accepted. A held request
    /// stays held until an exact decision or physical-window retirement.
    HoldClose(NativeViewportCloseRequest),
}

/// Final host decision for an exact held native close request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NativeViewportCloseDecision {
    /// Permit root shutdown or release the deferred child's close gate.
    ///
    /// The host remains responsible for omitting/retiring an allowed child.
    Allow,
    /// Keep the window and report cancellation of the original exact request.
    Veto,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum NativeCloseOwnership {
    Application,
    Held {
        decision: Option<NativeViewportCloseDecision>,
    },
}

#[derive(Clone, Copy, Debug)]
struct PendingNativeViewportClose {
    request: NativeViewportCloseRequest,
    ownership: NativeCloseOwnership,
}

#[derive(Clone)]
struct NativeCloseResolutionHandle(Weak<NativeHostStateInner>);

fn close_resolution_handle_id() -> egui::Id {
    egui::Id::unique("eframe::native_host::close_resolution_handle")
}

/// Schedules a final decision for a held request on its attached egui context.
///
/// Returns `false` for a foreign attachment, stale/retired request, unmanaged
/// close, or conflicting decision. Repeating the same decision before execution
/// is harmless. Acceptance wakes the root but is not physical-close proof.
/// Veto reports the original request through
/// [`NativeHostHandler::on_viewport_close_cancelled`]; a neutral hold never does.
pub fn resolve_native_viewport_close(
    ctx: &egui::Context,
    request: NativeViewportCloseRequest,
    decision: NativeViewportCloseDecision,
) -> bool {
    let handle =
        ctx.data(|data| data.get_temp::<NativeCloseResolutionHandle>(close_resolution_handle_id()));
    let Some(inner) = handle.and_then(|handle| handle.0.upgrade()) else {
        return false;
    };
    if inner.attachment != request.attachment {
        return false;
    }
    let accepted = {
        let mut pending = inner
            .pending_viewport_closes
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let Some(close) = pending.get_mut(&request.viewport_id) else {
            return false;
        };
        if close.request != request {
            return false;
        }
        match &mut close.ownership {
            NativeCloseOwnership::Application => false,
            NativeCloseOwnership::Held { decision: retained } => match *retained {
                Some(previous) => previous == decision,
                None => {
                    *retained = Some(decision);
                    true
                }
            },
        }
    };
    if accepted {
        ctx.request_repaint_of(ViewportId::ROOT);
    }
    accepted
}

impl NativeViewportCloseRequest {
    /// Returns the native event which introduced the close request.
    pub const fn event(self) -> NativeEventOrdinal {
        self.event
    }

    /// Returns the viewport which consumed the close request.
    pub const fn viewport_id(self) -> ViewportId {
        self.viewport_id
    }

    /// Returns the exact window which emitted the close request.
    pub const fn window_id(self) -> WindowId {
        self.window_id
    }
}

/// Opaque identity available while one viewport UI callback is producing an output.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NativeOutputToken {
    context: NonZeroU64,
    nonce: NonZeroU64,
    viewport_id: ViewportId,
    window_id: WindowId,
    create_attempt: Option<NativeViewportCreateAttempt>,
}

/// Opaque ownership identity for one eframe native-host attachment.
///
/// Eframe acquires this identity before it publishes any native callback and
/// releases it after every clone of the matching native host state is gone.
/// Handlers may use equality only to enforce exclusive attachment.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct NativeHostAttachment {
    context: NonZeroU64,
}

impl core::fmt::Debug for NativeHostAttachment {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter.write_str("NativeHostAttachment(..)")
    }
}

impl NativeOutputToken {
    /// Returns whether two output tokens were minted by the same native context.
    ///
    /// The context identity remains opaque; hosts may use only equality to
    /// validate a context-local output sequence.
    pub fn same_context(self, other: Self) -> bool {
        self.context == other.context
    }

    /// Returns the viewport whose UI callback owns this token.
    pub const fn viewport_id(self) -> ViewportId {
        self.viewport_id
    }

    /// Returns the native window whose callback generated this output.
    pub const fn window_id(self) -> WindowId {
        self.window_id
    }

    /// Returns the deferred-create attempt which produced this native window.
    ///
    /// Root outputs and ordinary upstream viewports have no attempt. The token
    /// is correlation only and does not prove that the host still accepts the
    /// corresponding logical viewport generation.
    pub const fn create_attempt(self) -> Option<NativeViewportCreateAttempt> {
        self.create_attempt
    }
}

/// Host admission for one candidate native viewport output.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum NativeOutputAdmission {
    /// The host reserved storage for this output and its exact terminal result.
    #[default]
    Proceed,
    /// Leave the viewport input buffered and retry after a root control pass.
    ///
    /// The candidate token never becomes active and receives no pass finalization
    /// or terminal callback. The host must not retain an output obligation for it.
    Defer,
}

/// Completion order assigned when one [`egui::FullOutput`] finishes generation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NativeOutputOrdinal(NonZeroU64);

impl NativeOutputOrdinal {
    /// Returns the context-local completion ordinal.
    pub const fn get(self) -> u64 {
        self.0.get()
    }
}

/// Terminal presentation-authority disposition for one generated output.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum NativeOutputStatus {
    /// The renderer presented this viewport output as semantic presentation authority.
    Presented,
    /// This viewport output did not become semantic presentation authority.
    ///
    /// The renderer may have skipped the output, painted a no-frame fallback, or composited a
    /// visual-only overlay. Texture commands remain owned by eframe's context-global texture batch
    /// and may be applied by a different viewport output.
    NotPresented,
}

/// Terminal result for one context-local output token.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct NativeOutputResult {
    token: NativeOutputToken,
    ordinal: NativeOutputOrdinal,
    status: NativeOutputStatus,
}

impl NativeOutputResult {
    /// Returns the opaque token observed by the producing UI callback.
    pub const fn token(self) -> NativeOutputToken {
        self.token
    }

    /// Returns the order in which this output finished generation.
    pub const fn ordinal(self) -> NativeOutputOrdinal {
        self.ordinal
    }

    /// Returns the output's terminal presentation-authority disposition.
    pub const fn status(self) -> NativeOutputStatus {
        self.status
    }
}

/// Exact egui pass output finalized by the native host which owns its output token.
///
/// Eframe creates this value only after every ordinary egui output hook has
/// returned successfully and the repeat/terminal disposition is frozen. The
/// borrowed output belongs to the active native output scope and cannot be
/// retained beyond this callback.
pub struct NativeOutputPassFinalization<'a> {
    context: &'a egui::Context,
    token: NativeOutputToken,
    ended_viewport: ViewportId,
    disposition: egui::OutputPassDisposition,
    output: &'a mut egui::FullOutput,
}

impl<'a> NativeOutputPassFinalization<'a> {
    /// Decomposes the finalization into its exact host-owned facts.
    pub fn into_parts(
        self,
    ) -> (
        &'a egui::Context,
        NativeOutputToken,
        ViewportId,
        egui::OutputPassDisposition,
        &'a mut egui::FullOutput,
    ) {
        (
            self.context,
            self.token,
            self.ended_viewport,
            self.disposition,
            self.output,
        )
    }
}

/// Host-owned callback installed before eframe attaches a native context.
pub trait NativeOutputPassFinalizer: Send + Sync + 'static {
    /// Settles one exact pass output at eframe's final output boundary.
    fn finalize(&self, finalization: NativeOutputPassFinalization<'_>) -> NativeHostWake;
}

/// Optional wake requested after a terminal host callback.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum NativeHostWake {
    /// Do not schedule an additional frame.
    #[default]
    Wait,
    /// Schedule the root viewport so queued host records can be reduced.
    RepaintRoot,
    /// Preserve one root repaint after a root pass which is already queued.
    ///
    /// Native output handlers should use this for lifecycle-bearing terminals
    /// whose callback record must remain observable after the current pass.
    RepaintRootAfterCurrent,
}

/// Backend disposition after eframe attempted to change native visibility.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum NativeViewportVisibilityStatus {
    /// Eframe invoked the corresponding native window operation.
    ///
    /// This does not prove that the window manager has applied the request.
    /// Observe a later [`NativeWindowSnapshot::visible`] value for that fact.
    Dispatched,
    /// The active window backend does not implement the operation.
    Unsupported,
    /// The command was retired before execution. This proves no native window state.
    Retired,
}

/// Opaque identity for one host-owned native visibility command.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NativeViewportVisibilityCommandToken(NonZeroU64);

/// Result of one host-owned native visibility dispatch.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct NativeViewportVisibilityResult {
    token: NativeViewportVisibilityCommandToken,
    viewport_id: ViewportId,
    window_id: WindowId,
    visible: bool,
    status: NativeViewportVisibilityStatus,
}

impl NativeViewportVisibilityResult {
    /// Returns the exact host command which produced this result.
    pub const fn token(self) -> NativeViewportVisibilityCommandToken {
        self.token
    }

    /// Returns the eframe viewport which received the command.
    pub const fn viewport_id(self) -> ViewportId {
        self.viewport_id
    }

    /// Returns the exact native window which received the command.
    pub const fn window_id(self) -> WindowId {
        self.window_id
    }

    /// Returns the requested native visibility.
    pub const fn visible(self) -> bool {
        self.visible
    }

    /// Returns the backend disposition.
    pub const fn status(self) -> NativeViewportVisibilityStatus {
        self.status
    }
}

/// Why eframe could not create one deferred native viewport.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum NativeViewportCreateFailureKind {
    /// Native window or renderer initialization failed.
    WindowUnavailable,
    /// The active backend cannot keep a host-staged viewport hidden.
    VisibilityUnsupported,
}

/// Terminal failure to create the native window for one deferred viewport.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct NativeViewportCreateFailure {
    create_attempt: NativeViewportCreateAttempt,
    kind: NativeViewportCreateFailureKind,
}

impl NativeViewportCreateFailure {
    /// Returns the exact deferred-create attempt which failed.
    pub const fn create_attempt(self) -> NativeViewportCreateAttempt {
        self.create_attempt
    }
}

/// Eframe-owned generation for one deferred native-window creation attempt.
///
/// The value is minted immediately before eframe asks the host whether the
/// physical create may proceed. A deferred attempt is discarded without an
/// output or failure callback. An admitted attempt is echoed through every
/// output produced by that native window, or through its terminal creation
/// failure. It is not a window, renderer, viewport-generation, or docking
/// authority.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NativeViewportCreateAttempt {
    context: NonZeroU64,
    nonce: NonZeroU64,
    viewport_id: ViewportId,
}

impl NativeViewportCreateAttempt {
    /// Returns the deferred viewport whose physical creation was attempted.
    pub const fn viewport_id(self) -> ViewportId {
        self.viewport_id
    }
}

/// Host admission for one exact deferred native-window creation attempt.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum NativeViewportCreateAdmission {
    /// Proceed with native creation and optionally request exact physical placement.
    Proceed {
        /// Undecorated physical outer rectangle requested before window creation.
        undecorated_outer_rect: Option<NativePhysicalRect>,
    },
    /// Keep the deferred viewport logical-only until a later host cycle.
    Defer,
}

/// A physical desktop rectangle used by the native host seam.
///
/// The rectangle is expressed in integer physical pixels and does not imply
/// that the window manager accepted a corresponding placement request.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct NativePhysicalRect {
    x: i32,
    y: i32,
    width: u32,
    height: u32,
}

impl NativePhysicalRect {
    /// Creates a physical desktop rectangle.
    pub const fn new(x: i32, y: i32, width: u32, height: u32) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    /// Returns the physical x coordinate.
    pub const fn x(self) -> i32 {
        self.x
    }

    /// Returns the physical y coordinate.
    pub const fn y(self) -> i32 {
        self.y
    }

    /// Returns the physical width.
    pub const fn width(self) -> u32 {
        self.width
    }

    /// Returns the physical height.
    pub const fn height(self) -> u32 {
        self.height
    }
}

/// Window-local geometry sampled at a native callback or output boundary.
///
/// Content size is expressed in physical pixels and is available independently
/// of desktop position. The presentation scale uses egui-winit's `f32` zoom and
/// native-scale multiplication, widened to `f64` after rounding.
///
/// These are current window facts, not the geometry which generated a queued
/// input event. Hosts must compare them with the event's own geometry before
/// interpreting event-local coordinates.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct NativeWindowLocalGeometry {
    content_size: [u32; 2],
    native_scale_factor: f64,
    presentation_scale_factor: f64,
}

impl NativeWindowLocalGeometry {
    /// Returns the physical content width and height, without a desktop origin.
    pub const fn content_size(self) -> [u32; 2] {
        self.content_size
    }

    /// Returns the platform-native scale sampled at this boundary.
    pub const fn native_scale_factor(self) -> f64 {
        self.native_scale_factor
    }

    /// Returns the egui presentation scale, including the current zoom factor.
    pub const fn presentation_scale_factor(self) -> f64 {
        self.presentation_scale_factor
    }

    pub(crate) fn capture(egui_ctx: &egui::Context, window: &Window) -> Self {
        let size = window.inner_size();
        let native_scale_factor = window.scale_factor();
        Self::from_window_facts(
            [size.width, size.height],
            native_scale_factor,
            egui_ctx.zoom_factor(),
        )
    }

    fn from_window_facts(
        content_size: [u32; 2],
        native_scale_factor: f64,
        zoom_factor: f32,
    ) -> Self {
        // Match egui_winit::pixels_per_point without reading the native scale again.
        let presentation_scale_factor = f64::from(zoom_factor * native_scale_factor as f32);
        Self {
            content_size,
            native_scale_factor,
            presentation_scale_factor,
        }
    }
}

/// Window facts captured for the exact callback that owns an output token.
///
/// Unsupported or unavailable facts remain [`None`]. In particular, this
/// snapshot does not infer desktop geometry from cached viewport information.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct NativeWindowSnapshot {
    content_size: [u32; 2],
    inner_rect: Option<NativePhysicalRect>,
    outer_rect: Option<NativePhysicalRect>,
    native_scale_factor: f64,
    presentation_scale_factor: f64,
    visible: Option<bool>,
    minimized: Option<bool>,
    input_state: NativeWindowInputState,
}

/// Exact cursor hit-test state of one live native window.
///
/// A newly observed window starts in [`Self::ReceivesInput`], matching winit's
/// default window configuration. Eframe records a change only after the
/// matching native window accepts a cursor hit-test operation. The fact is
/// scoped to the exact `(viewport, window)` incarnation and resets when that
/// viewport acquires a replacement window.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum NativeWindowInputState {
    /// The native window participates in pointer hit testing.
    ReceivesInput,
    /// Pointer hit testing passes through the native window.
    PassThrough,
}

/// One native window in a root-output roster captured from live backend windows.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct NativeViewportRecord {
    viewport_id: ViewportId,
    window_id: WindowId,
    window: NativeWindowSnapshot,
}

impl NativeViewportRecord {
    /// Returns the eframe viewport which owns the native window.
    pub const fn viewport_id(self) -> ViewportId {
        self.viewport_id
    }

    /// Returns the exact live native window identity.
    pub const fn window_id(self) -> WindowId {
        self.window_id
    }

    /// Returns the facts captured from this exact live window.
    pub const fn window(self) -> NativeWindowSnapshot {
        self.window
    }

    pub(crate) fn capture(
        viewport_id: ViewportId,
        egui_ctx: &egui::Context,
        window: &Window,
        input_state: NativeWindowInputState,
    ) -> Self {
        Self {
            viewport_id,
            window_id: window.id(),
            window: NativeWindowSnapshot::capture(egui_ctx, window, input_state),
        }
    }
}

/// Borrowed exact native-window roster attached to one root output callback.
#[derive(Clone, Copy, Debug)]
pub struct NativeViewportRoster<'a> {
    records: &'a [NativeViewportRecord],
    work_areas: NativeWorkAreaRoster<'a>,
    backend: NativeWindowingBackend,
}

/// Native window-system backend which produced one exact root roster.
///
/// The value identifies the active windowing contract only. It does not imply
/// that every optional geometry, input, or window-management operation is
/// available; hosts must still treat absent facts and terminal unsupported
/// results as authoritative.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum NativeWindowingBackend {
    /// Microsoft Windows desktop windowing.
    Windows,
    /// Apple macOS desktop windowing.
    MacOs,
    /// The X11 window system.
    X11,
    /// The Wayland window system.
    Wayland,
    /// A backend not classified by this host seam.
    Other,
}

impl<'a> NativeViewportRoster<'a> {
    /// Creates one complete root roster from exact native facts.
    pub(crate) const fn new(
        records: &'a [NativeViewportRecord],
        work_areas: NativeWorkAreaRoster<'a>,
        backend: NativeWindowingBackend,
    ) -> Self {
        Self {
            records,
            work_areas,
            backend,
        }
    }

    /// Returns every live native window captured for this root callback.
    pub const fn records(self) -> &'a [NativeViewportRecord] {
        self.records
    }

    /// Returns the complete work-area authority captured at the same boundary.
    pub const fn work_areas(self) -> NativeWorkAreaRoster<'a> {
        self.work_areas
    }

    /// Returns the active native windowing backend for this exact roster.
    pub const fn backend(self) -> NativeWindowingBackend {
        self.backend
    }
}

pub(crate) struct NativeViewportRosterCapture {
    records: Vec<NativeViewportRecord>,
    work_areas: OwnedNativeWorkAreaRoster,
    backend: NativeWindowingBackend,
}

impl NativeViewportRosterCapture {
    pub(crate) fn capture(
        event_loop: &ActiveEventLoop,
        records: Vec<NativeViewportRecord>,
    ) -> Self {
        Self {
            records,
            work_areas: OwnedNativeWorkAreaRoster::capture(event_loop),
            backend: native_windowing_backend(event_loop),
        }
    }

    pub(crate) fn as_borrowed(&self) -> NativeViewportRoster<'_> {
        NativeViewportRoster::new(&self.records, self.work_areas.as_borrowed(), self.backend)
    }
}

#[cfg(target_os = "windows")]
fn native_windowing_backend(_event_loop: &ActiveEventLoop) -> NativeWindowingBackend {
    NativeWindowingBackend::Windows
}

#[cfg(target_os = "macos")]
fn native_windowing_backend(_event_loop: &ActiveEventLoop) -> NativeWindowingBackend {
    NativeWindowingBackend::MacOs
}

#[cfg(target_os = "linux")]
fn native_windowing_backend(event_loop: &ActiveEventLoop) -> NativeWindowingBackend {
    event_loop
        .display_handle()
        .map_or(NativeWindowingBackend::Other, |handle| {
            match handle.as_raw() {
                raw_window_handle::RawDisplayHandle::Xlib(_)
                | raw_window_handle::RawDisplayHandle::Xcb(_) => NativeWindowingBackend::X11,
                raw_window_handle::RawDisplayHandle::Wayland(_) => NativeWindowingBackend::Wayland,
                _ => NativeWindowingBackend::Other,
            }
        })
}

#[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
fn native_windowing_backend(_event_loop: &ActiveEventLoop) -> NativeWindowingBackend {
    NativeWindowingBackend::Other
}

impl NativeWindowSnapshot {
    /// Returns the physical content width and height, even when the platform
    /// cannot report the content's desktop position.
    pub const fn content_size(self) -> [u32; 2] {
        self.content_size
    }

    /// Returns the current physical content rectangle, when the platform can
    /// report its desktop position.
    pub const fn inner_rect(self) -> Option<NativePhysicalRect> {
        self.inner_rect
    }

    /// Returns the current physical outer rectangle when the platform exposes
    /// it without a heuristic.
    pub const fn outer_rect(self) -> Option<NativePhysicalRect> {
        self.outer_rect
    }

    /// Returns the current platform-native scale factor.
    pub const fn native_scale_factor(self) -> f64 {
        self.native_scale_factor
    }

    /// Returns the current egui presentation scale factor.
    ///
    /// This includes the egui zoom factor and therefore must not be inferred
    /// from [`Self::native_scale_factor`].
    pub const fn presentation_scale_factor(self) -> f64 {
        self.presentation_scale_factor
    }

    /// Returns the platform visibility flag when it is available.
    ///
    /// This is not a compositor presentation acknowledgement.
    pub const fn visible(self) -> Option<bool> {
        self.visible
    }

    /// Returns whether the platform reports the window as minimized.
    pub const fn minimized(self) -> Option<bool> {
        self.minimized
    }

    /// Returns the exact cursor hit-test state for this window incarnation.
    pub const fn input_state(self) -> NativeWindowInputState {
        self.input_state
    }

    pub(crate) fn capture(
        egui_ctx: &egui::Context,
        window: &Window,
        input_state: NativeWindowInputState,
    ) -> Self {
        let local_geometry = NativeWindowLocalGeometry::capture(egui_ctx, window);
        let inner_position = window.inner_position().ok();

        #[cfg(any(target_os = "windows", target_os = "macos"))]
        let outer_rect = window
            .outer_position()
            .ok()
            .map(|position| physical_rect(position, window.outer_size()));
        #[cfg(all(target_os = "linux", feature = "x11"))]
        let outer_rect = window
            .exact_outer_position_and_size()
            .map(|(position, size)| physical_rect(position, size));
        #[cfg(all(target_os = "linux", not(feature = "x11")))]
        let outer_rect = None;
        #[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
        let outer_rect = None;

        #[cfg(target_os = "windows")]
        let minimized = window.is_minimized();
        #[cfg(not(target_os = "windows"))]
        let minimized = None;

        let visible = window.is_visible();

        Self::from_captured_geometry(
            local_geometry,
            inner_position,
            outer_rect,
            visible,
            minimized,
            input_state,
        )
    }

    fn from_captured_geometry(
        local_geometry: NativeWindowLocalGeometry,
        inner_position: Option<winit::dpi::PhysicalPosition<i32>>,
        outer_rect: Option<NativePhysicalRect>,
        visible: Option<bool>,
        minimized: Option<bool>,
        input_state: NativeWindowInputState,
    ) -> Self {
        let [width, height] = local_geometry.content_size();
        let inner_rect = inner_position
            .map(|position| physical_rect(position, winit::dpi::PhysicalSize::new(width, height)));
        Self {
            content_size: [width, height],
            inner_rect,
            outer_rect,
            native_scale_factor: local_geometry.native_scale_factor(),
            presentation_scale_factor: local_geometry.presentation_scale_factor(),
            visible,
            minimized,
            input_state,
        }
    }
}

fn physical_rect(
    position: winit::dpi::PhysicalPosition<i32>,
    size: winit::dpi::PhysicalSize<u32>,
) -> NativePhysicalRect {
    NativePhysicalRect::new(position.x, position.y, size.width, size.height)
}

impl NativeViewportCreateFailure {
    /// Returns the deferred viewport whose native window could not be created.
    pub const fn viewport_id(self) -> ViewportId {
        self.create_attempt.viewport_id
    }

    /// Returns why native viewport creation could not proceed.
    pub const fn kind(self) -> NativeViewportCreateFailureKind {
        self.kind
    }
}

/// One raw physical-device removal observed before eframe handles it.
///
/// Winit explicitly does not correlate raw-device identifiers with the
/// virtual identifiers carried by window input events, so this fact exposes
/// ordering only. A host must not use the raw callback identifier to target
/// one window-input stream.
#[derive(Clone, Copy, Debug)]
pub struct NativeDeviceRemoval {
    ordinal: NativeEventOrdinal,
}

impl NativeDeviceRemoval {
    /// Returns the exact order assigned by this eframe native context.
    pub const fn ordinal(&self) -> NativeEventOrdinal {
        self.ordinal
    }
}

/// One immutable native window event observed before egui input translation.
#[derive(Clone, Copy, Debug)]
pub struct NativeWindowEvent<'a> {
    ordinal: NativeEventOrdinal,
    window_id: WindowId,
    viewport_id: Option<ViewportId>,
    local_geometry: Option<NativeWindowLocalGeometry>,
    event: &'a winit::event::WindowEvent,
    close_request: Option<NativeViewportCloseRequest>,
}

impl NativeWindowEvent<'_> {
    /// Returns the exact order assigned by this eframe native context.
    pub const fn ordinal(&self) -> NativeEventOrdinal {
        self.ordinal
    }

    /// Returns the native window receiving the callback.
    pub const fn window_id(&self) -> WindowId {
        self.window_id
    }

    /// Returns the eframe viewport receiving the callback, when it belongs to eframe.
    pub const fn viewport_id(&self) -> Option<ViewportId> {
        self.viewport_id
    }

    /// Returns window-local geometry sampled before this callback, when both
    /// the exact live window and egui context are available.
    ///
    /// This does not describe the geometry which generated a queued event.
    pub const fn local_geometry(&self) -> Option<NativeWindowLocalGeometry> {
        self.local_geometry
    }

    /// Returns the immutable native event before egui-winit translation.
    pub const fn event(&self) -> &winit::event::WindowEvent {
        self.event
    }

    /// Returns the canonical close identity carried by this exact callback.
    /// Repeated OS close events for one still-pending window keep this identity.
    pub const fn close_request(&self) -> Option<NativeViewportCloseRequest> {
        self.close_request
    }
}

/// Globally consistent native focus after eframe applies one focus event.
///
/// `Unknown` means eframe cannot prove which native window owns focus. In
/// particular, losing focus to another process must not be reported as an
/// authoritative no-focus state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NativeGlobalFocus {
    /// One exact eframe viewport owns native focus.
    Viewport {
        /// Focused eframe viewport.
        viewport_id: ViewportId,
        /// Exact native window attached to the focused viewport.
        window_id: WindowId,
    },
    /// Eframe cannot prove the globally focused native window.
    Unknown,
}

/// One focus observation causally paired with a native focus event.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NativeGlobalFocusObservation {
    event: NativeEventOrdinal,
    focused: NativeGlobalFocus,
}

impl NativeGlobalFocusObservation {
    /// Returns the event ordinal which produced this observation.
    pub const fn event(self) -> NativeEventOrdinal {
        self.event
    }

    /// Returns the globally focused viewport fact.
    pub const fn focused(self) -> NativeGlobalFocus {
        self.focused
    }
}

/// Result of consuming one exact viewport focus command.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NativeViewportFocusStatus {
    /// The target window was already focused when the command was consumed.
    AlreadyFocused,
    /// Eframe dispatched the platform focus request and awaits a native event.
    Requested,
    /// The command was retired before execution. This proves no native focus state.
    Retired,
}

/// Opaque identity for one host-owned viewport focus command.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NativeViewportFocusCommandToken(NonZeroU64);

/// Exact host-owned viewport focus command consumed by eframe.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NativeViewportFocusResult {
    token: NativeViewportFocusCommandToken,
    viewport_id: ViewportId,
    window_id: WindowId,
    status: NativeViewportFocusStatus,
}

impl NativeViewportFocusResult {
    /// Returns the exact host command which produced this result.
    pub const fn token(self) -> NativeViewportFocusCommandToken {
        self.token
    }

    /// Returns the target eframe viewport.
    pub const fn viewport_id(self) -> ViewportId {
        self.viewport_id
    }

    /// Returns the exact target native window.
    pub const fn window_id(self) -> WindowId {
        self.window_id
    }

    /// Returns whether the command observed focus, requested it, or retired before execution.
    pub const fn status(self) -> NativeViewportFocusStatus {
        self.status
    }
}

/// Result of applying one exact viewport pointer pass-through command.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NativeViewportPointerPassthroughStatus {
    /// Winit applied the requested cursor hit-test state.
    Applied,
    /// The active platform backend does not support cursor hit-test control.
    Unsupported,
    /// The platform ignored or failed the cursor hit-test request.
    Failed,
    /// The command was retired before execution. This proves no cursor hit-test state.
    Retired,
}

/// Opaque identity for one host-owned pointer pass-through command.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NativeViewportPointerPassthroughCommandToken(NonZeroU64);

/// Exact pointer pass-through command result for one native viewport.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NativeViewportPointerPassthroughResult {
    token: NativeViewportPointerPassthroughCommandToken,
    viewport_id: ViewportId,
    window_id: WindowId,
    enabled: bool,
    status: NativeViewportPointerPassthroughStatus,
}

impl NativeViewportPointerPassthroughResult {
    /// Returns the exact host command which produced this result.
    pub const fn token(self) -> NativeViewportPointerPassthroughCommandToken {
        self.token
    }

    /// Returns the target eframe viewport.
    pub const fn viewport_id(self) -> ViewportId {
        self.viewport_id
    }

    /// Returns the exact target native window.
    pub const fn window_id(self) -> WindowId {
        self.window_id
    }

    /// Returns whether pointer input was requested to pass through the window.
    pub const fn enabled(self) -> bool {
        self.enabled
    }

    /// Returns the terminal platform result.
    pub const fn status(self) -> NativeViewportPointerPassthroughStatus {
        self.status
    }
}

/// A synchronous native pointer observation, without generating a window input event.
///
/// Observed facts belong to one backend observation. Eframe never fills missing facts from
/// cached input or later window queries. `Observed` does not imply complete capture facts,
/// continuity of an earlier press, or support for stationary pointer continuation. The current
/// X11 backend reports capture and delivery as [`PointerWindowRoute::Unknown`], even when it can
/// query the requested device's position and hover. Eframe forwards those unknown routes unchanged.
/// This does not grant application pointer ownership or acknowledge a continuation token; the
/// host must process the result through its ordered ingress.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum NativeViewportPointerObservation {
    /// The backend returned facts and a delivery classification from the same observation.
    ///
    /// Individual facts can be missing and routes can remain unknown.
    Observed {
        /// The immutable facts returned by the backend.
        facts: PointerEventFacts,
        /// The delivery classification from that same observation, possibly unknown.
        ///
        /// This is independent of hover, capture and the requested window identity.
        delivery: PointerWindowRoute,
    },
    /// No eligible observation was available at execution time.
    ///
    /// This includes a mismatched native window or an execution path without an active event loop.
    Unavailable,
    /// The active native backend does not support this observation.
    Unsupported,
}

/// Opaque identity for one host-owned viewport pointer observation command.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NativeViewportPointerObservationCommandToken(NonZeroU64);

/// The correlated terminal result of one native pointer observation command.
///
/// The viewport, expected window and device identify the request, even when no observation was
/// possible. They are not independently observed routing facts.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct NativeViewportPointerObservationResult {
    token: NativeViewportPointerObservationCommandToken,
    viewport_id: ViewportId,
    expected_window_id: WindowId,
    device_id: DeviceId,
    observation: NativeViewportPointerObservation,
}

impl NativeViewportPointerObservationResult {
    /// Returns the exact command which produced this result.
    pub const fn token(self) -> NativeViewportPointerObservationCommandToken {
        self.token
    }

    /// Returns the requested eframe viewport.
    pub const fn viewport_id(self) -> ViewportId {
        self.viewport_id
    }

    /// Returns the exact native window expected by the request.
    pub const fn expected_window_id(self) -> WindowId {
        self.expected_window_id
    }

    /// Returns the window-input device requested by the host.
    pub const fn device_id(self) -> DeviceId {
        self.device_id
    }

    /// Returns the unmodified backend observation or its explicit unavailable/unsupported result.
    pub const fn observation(self) -> NativeViewportPointerObservation {
        self.observation
    }

    fn observe_with(
        token: NativeViewportPointerObservationCommandToken,
        viewport_id: ViewportId,
        expected_window_id: WindowId,
        device_id: DeviceId,
        actual_window_id: WindowId,
        observe: impl FnOnce(WindowId, DeviceId) -> NativeViewportPointerObservation,
    ) -> Self {
        let observation = if actual_window_id == expected_window_id {
            observe(expected_window_id, device_id)
        } else {
            NativeViewportPointerObservation::Unavailable
        };
        Self {
            token,
            viewport_id,
            expected_window_id,
            device_id,
            observation,
        }
    }
}

/// Exact terminal identity of a pending property command retired before execution.
///
/// The viewport or its window was forgotten while the command remained queued. This reports
/// neither an attempted native operation nor an observed window state. No window identity is
/// invented: a host may correlate the token with its own pre-dispatch target reservation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NativeViewportPropertyCommandRetirement {
    /// One queued focus command was retired.
    Focus {
        /// Target viewport of the retired command.
        viewport_id: ViewportId,
        /// Exact token returned when the command was queued.
        token: NativeViewportFocusCommandToken,
    },
    /// One queued visibility command was retired.
    Visibility {
        /// Target viewport of the retired command.
        viewport_id: ViewportId,
        /// Exact token returned when the command was queued.
        token: NativeViewportVisibilityCommandToken,
        /// Requested visibility of the retired command.
        visible: bool,
    },
    /// One queued pointer pass-through command was retired.
    PointerPassthrough {
        /// Target viewport of the retired command.
        viewport_id: ViewportId,
        /// Exact token returned when the command was queued.
        token: NativeViewportPointerPassthroughCommandToken,
        /// Requested pass-through value of the retired command.
        enabled: bool,
    },
    /// One queued pointer observation was retired without querying the backend.
    PointerObservation {
        /// Target viewport of the retired command.
        viewport_id: ViewportId,
        /// Exact token returned when the command was prepared or queued.
        token: NativeViewportPointerObservationCommandToken,
        /// Expected native window from the request, not an observed window identity.
        expected_window_id: WindowId,
        /// Window-input device from the request.
        device_id: DeviceId,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum QueuedNativeViewportPropertyCommand {
    Focus(NativeViewportFocusCommandToken),
    Visibility {
        token: NativeViewportVisibilityCommandToken,
        visible: bool,
    },
    PointerPassthrough {
        token: NativeViewportPointerPassthroughCommandToken,
        enabled: bool,
    },
    PointerObservation {
        token: NativeViewportPointerObservationCommandToken,
        expected_window_id: WindowId,
        device_id: DeviceId,
    },
}

#[derive(Default)]
struct QueuedNativeViewportPropertyCommands(
    BTreeMap<ViewportId, VecDeque<QueuedNativeViewportPropertyCommand>>,
);

#[derive(Clone, Copy, Debug, PartialEq)]
enum NativeViewportPropertyResult {
    Focus(NativeViewportFocusResult),
    Visibility(NativeViewportVisibilityResult),
    PointerPassthrough(NativeViewportPointerPassthroughResult),
    PointerObservation(NativeViewportPointerObservationResult),
}

/// One prepared host command whose terminal owner can be reserved before enqueueing.
///
/// Preparation only spends an opaque identity. Dropping this value schedules no
/// native command or callback. Its viewport and requested value cannot change;
/// [`Self::enqueue`] consumes it and therefore cannot enqueue it twice.
#[must_use = "reserve the command's terminal owner, then enqueue it or drop it"]
pub struct PreparedNativeViewportCommand<Token> {
    token: Token,
    viewport_id: ViewportId,
    command: QueuedNativeViewportPropertyCommand,
}

impl<Token: Copy> PreparedNativeViewportCommand<Token> {
    /// Returns the exact token which the eventual terminal callback will echo.
    pub const fn token(&self) -> Token {
        self.token
    }
}

impl<Token> PreparedNativeViewportCommand<Token> {
    /// Enqueues the prepared command after its host terminal owner is reserved.
    ///
    /// The context attachment, missing-window and retirement rules match
    /// [`queue_native_viewport_focus`]. Without an active attachment the token
    /// remains unused and no operation or callback is scheduled.
    pub fn enqueue(self, ctx: &egui::Context) {
        queue_native_viewport_property(ctx, self.viewport_id, self.command);
    }
}

/// Prepares a focus command without enqueueing a native operation.
pub fn prepare_native_viewport_focus(
    viewport_id: ViewportId,
) -> PreparedNativeViewportCommand<NativeViewportFocusCommandToken> {
    let token = NativeViewportFocusCommandToken(next_non_zero(
        &NEXT_FOCUS_COMMAND,
        "native focus command identity exhausted",
    ));
    PreparedNativeViewportCommand {
        token,
        viewport_id,
        command: QueuedNativeViewportPropertyCommand::Focus(token),
    }
}

/// Prepares a visibility command without enqueueing a native operation.
pub fn prepare_native_viewport_visibility(
    viewport_id: ViewportId,
    visible: bool,
) -> PreparedNativeViewportCommand<NativeViewportVisibilityCommandToken> {
    let token = NativeViewportVisibilityCommandToken(next_non_zero(
        &NEXT_VISIBILITY_COMMAND,
        "native visibility command identity exhausted",
    ));
    PreparedNativeViewportCommand {
        token,
        viewport_id,
        command: QueuedNativeViewportPropertyCommand::Visibility { token, visible },
    }
}

/// Prepares a pointer pass-through command without enqueueing a native operation.
pub fn prepare_native_viewport_pointer_passthrough(
    viewport_id: ViewportId,
    enabled: bool,
) -> PreparedNativeViewportCommand<NativeViewportPointerPassthroughCommandToken> {
    let token = NativeViewportPointerPassthroughCommandToken(next_non_zero(
        &NEXT_POINTER_PASSTHROUGH_COMMAND,
        "native pointer pass-through command identity exhausted",
    ));
    PreparedNativeViewportCommand {
        token,
        viewport_id,
        command: QueuedNativeViewportPropertyCommand::PointerPassthrough { token, enabled },
    }
}

/// Prepares a pointer observation without querying the backend or scheduling a callback.
///
/// Reserve the token's terminal owner before enqueueing. The core continuation identity remains
/// owned by the host and is not sent to the native backend.
pub fn prepare_native_viewport_pointer_observation(
    viewport_id: ViewportId,
    expected_window_id: WindowId,
    device_id: DeviceId,
) -> PreparedNativeViewportCommand<NativeViewportPointerObservationCommandToken> {
    let token = NativeViewportPointerObservationCommandToken(next_non_zero(
        &NEXT_POINTER_OBSERVATION_COMMAND,
        "native pointer observation command identity exhausted",
    ));
    PreparedNativeViewportCommand {
        token,
        viewport_id,
        command: QueuedNativeViewportPropertyCommand::PointerObservation {
            token,
            expected_window_id,
            device_id,
        },
    }
}

/// Queues one synchronous native pointer observation for an exact viewport, window and device.
///
/// The backend is queried only when native commands execute, after application UI returns. A
/// replacement window is never queried on behalf of the expected window. The command consumes no
/// native events and produces no synthetic input event. Results are reported through
/// [`NativeHostHandler::on_viewport_pointer_observation`]; an unavailable result is terminal and
/// is not retried by eframe. An observed result can have unknown capture and delivery and does not
/// establish stationary continuation eligibility. The host must apply its own input ordering,
/// budget and continuation checks before using an observation; neither the request identity nor
/// hover may fill missing capture or delivery. Queue ordering, attachment and retirement rules
/// match [`queue_native_viewport_focus`].
pub fn queue_native_viewport_pointer_observation(
    ctx: &egui::Context,
    viewport_id: ViewportId,
    expected_window_id: WindowId,
    device_id: DeviceId,
) -> NativeViewportPointerObservationCommandToken {
    let command =
        prepare_native_viewport_pointer_observation(viewport_id, expected_window_id, device_id);
    let token = command.token();
    command.enqueue(ctx);
    token
}

/// Queues one host-owned focus command for an exact viewport.
///
/// Ordinary [`egui::ViewportCommand::Focus`] commands do not produce
/// [`NativeHostHandler::on_viewport_focus`] callbacks. The dedicated command performs the native
/// operation itself and echoes this opaque token. Its result is not proof of native focus.
///
/// Host property commands run after ordinary commands, in enqueue order within each viewport.
/// There is no ordering guarantee between different viewports. A viewport without a native window
/// cannot produce a result; queuing a command does not create a native window.
/// An active native-host attachment must already be bound to `ctx` (as it is during viewport UI).
/// Otherwise the token is unused and no operation or callback is scheduled. Forgetting a target
/// viewport or its current window retires pending commands through
/// [`NativeHostHandler::on_viewport_property_command_retired`], in their original queue order.
/// Releasing the attachment discards any remaining commands with that attachment.
pub fn queue_native_viewport_focus(
    ctx: &egui::Context,
    viewport_id: ViewportId,
) -> NativeViewportFocusCommandToken {
    let command = prepare_native_viewport_focus(viewport_id);
    let token = command.token();
    command.enqueue(ctx);
    token
}

/// Queues one host-owned visibility command for an exact viewport.
///
/// Ordinary [`egui::ViewportCommand::Visible`] commands and viewport builder changes do not produce
/// [`NativeHostHandler::on_viewport_visibility`] callbacks. The dedicated command performs the
/// native operation itself and echoes this opaque token. Dispatch is not visibility or presentation
/// proof. Queue ordering and missing-window behavior match [`queue_native_viewport_focus`].
pub fn queue_native_viewport_visibility(
    ctx: &egui::Context,
    viewport_id: ViewportId,
    visible: bool,
) -> NativeViewportVisibilityCommandToken {
    let command = prepare_native_viewport_visibility(viewport_id, visible);
    let token = command.token();
    command.enqueue(ctx);
    token
}

fn queue_native_viewport_property(
    ctx: &egui::Context,
    viewport_id: ViewportId,
    command: QueuedNativeViewportPropertyCommand,
) {
    let handle =
        ctx.data(|data| data.get_temp::<NativeCloseResolutionHandle>(close_resolution_handle_id()));
    let Some(inner) = handle.and_then(|handle| handle.0.upgrade()) else {
        return;
    };
    inner
        .viewport_property_commands
        .lock()
        .0
        .entry(viewport_id)
        .or_default()
        .push_back(command);
    // Keep property authority entirely on the attachment. An ordinary companion could survive
    // retirement in Context or a renderer's deferred commands and affect a successor window.
    ctx.request_repaint_of(viewport_id);
    if viewport_id != ViewportId::ROOT {
        // Hidden children do not run app logic. The root's logic-only boundary also dispatches
        // pending typed targets without forcing a hidden UI pass.
        ctx.request_repaint_of(ViewportId::ROOT);
    }
}

/// Queues one host-owned pointer pass-through command for an exact viewport.
///
/// Ordinary [`egui::ViewportCommand::MousePassthrough`] commands remain application-owned and do
/// not produce [`NativeHostHandler::on_viewport_pointer_passthrough`] callbacks. This dedicated
/// path returns an opaque token which is echoed by the terminal callback, so a native coordinator
/// cannot accidentally consume an unrelated same-value application command.
/// Queue ordering and attachment requirements match [`queue_native_viewport_focus`].
pub fn queue_native_viewport_pointer_passthrough(
    ctx: &egui::Context,
    viewport_id: ViewportId,
    enabled: bool,
) -> NativeViewportPointerPassthroughCommandToken {
    let command = prepare_native_viewport_pointer_passthrough(viewport_id, enabled);
    let token = command.token();
    command.enqueue(ctx);
    token
}

/// External native coordinator callbacks.
///
/// Implementations must copy any retained facts during the callback. Borrowed winit values remain
/// owned by eframe. Immediate viewports are rejected while a handler is installed; use deferred
/// viewports so native side effects remain outside application UI recursion. Callbacks run on the
/// native event-loop thread and must not re-enter eframe.
pub trait NativeHostHandler: Send + Sync + 'static {
    /// Installs the one finalizer which owns egui pass settlement for this host.
    ///
    /// The default rejects installation. Implementations which support this
    /// seam should accept at most one finalizer before [`Self::try_attach`].
    fn install_output_pass_finalizer(
        &self,
        _finalizer: Arc<dyn NativeOutputPassFinalizer>,
    ) -> bool {
        false
    }

    /// Acquires exclusive ownership for one eframe native context.
    ///
    /// This runs before any native event, viewport, or output callback for the
    /// context. Returning `false` rejects construction of that native host
    /// state before the handler can observe or mutate callback state.
    fn try_attach(&self, _attachment: NativeHostAttachment) -> bool {
        true
    }

    /// Releases an attachment after every clone of its native host state is gone.
    fn detach(&self, _attachment: NativeHostAttachment) {}

    /// Decides whether one exact deferred native-window attempt may proceed.
    ///
    /// The admission callback runs immediately before native window creation.
    /// An admitted attempt token is echoed through the first and all later
    /// outputs from that window, or through a terminal creation failure. A
    /// deferred token is discarded. The token is not an acknowledgement that
    /// the platform accepted creation or placement.
    /// Eframe ignores the request when the viewport builder also requests
    /// fullscreen, maximized, or monitor-targeted placement.
    fn begin_deferred_viewport_create(
        &self,
        _attempt: NativeViewportCreateAttempt,
    ) -> NativeViewportCreateAdmission {
        NativeViewportCreateAdmission::Proceed {
            undecorated_outer_rect: None,
        }
    }

    /// Returns whether one hidden deferred viewport must still run its UI and renderer output.
    ///
    /// This is intended for host-owned pre-show or post-show staging. Returning `true` does not
    /// make the window visible, and eframe will not run its first-frame auto-show hook for the
    /// hidden output. The default preserves ordinary eframe behavior and leaves hidden viewports
    /// dormant.
    fn render_hidden_deferred_viewport(&self, _viewport_id: ViewportId) -> bool {
        false
    }

    /// Observes one window event before egui-winit translates it.
    ///
    /// Retain the immutable event and its exact route before returning
    /// `HoldClose(event.close_request().unwrap())`. Only that callback's close
    /// identity can be claimed; other events always continue normally. Once
    /// held, repeated OS close callbacks cannot release the gate by returning
    /// `Continue`. Resolve the retained request with [`resolve_native_viewport_close`].
    fn on_window_event(&self, _event: NativeWindowEvent<'_>) -> NativeWindowEventDisposition {
        NativeWindowEventDisposition::Continue
    }

    /// Observes one physical-device removal before eframe handles it.
    fn on_device_removed(&self, _removal: NativeDeviceRemoval) {}

    /// Observes globally consistent focus after eframe applies one focus event.
    fn on_global_focus(&self, _observation: NativeGlobalFocusObservation) -> NativeHostWake {
        NativeHostWake::Wait
    }

    /// Reports consumption of one [`queue_native_viewport_focus`] command with its exact token.
    ///
    /// Ordinary focus commands never produce this callback. This is not native focus proof.
    fn on_viewport_focus(&self, _result: NativeViewportFocusResult) -> NativeHostWake {
        NativeHostWake::Wait
    }

    /// Reports the terminal result of one exact pointer pass-through command.
    fn on_viewport_pointer_passthrough(
        &self,
        _result: NativeViewportPointerPassthroughResult,
    ) -> NativeHostWake {
        NativeHostWake::Wait
    }

    /// Reports one exact pointer observation command's terminal result.
    ///
    /// Copy retained facts into host-owned ordered ingress and return a wake request. The result
    /// cannot change an already completed output. This callback must not re-enter eframe or query
    /// newer window state to complete the observation. An observed result does not establish
    /// capture continuity or stationary continuation support; unknown routes remain unknown.
    fn on_viewport_pointer_observation(
        &self,
        _result: NativeViewportPointerObservationResult,
    ) -> NativeHostWake {
        NativeHostWake::Wait
    }

    /// Reports one pending property command retired without executing a native operation.
    ///
    /// Forgetting a viewport or its current window removes that viewport's queue and reports
    /// each exact token in enqueue order, after releasing all internal mutex guards. Commands
    /// already consumed for dispatch do not also retire. Queueing a fresh command from this
    /// callback is permitted; it belongs to the next queue and survives this retirement.
    /// A late forget for an older window does not retire a replacement window's commands.
    /// Wake requests arm the existing root wake after the current pass.
    fn on_viewport_property_command_retired(
        &self,
        _retirement: NativeViewportPropertyCommandRetirement,
    ) -> NativeHostWake {
        NativeHostWake::Wait
    }

    /// Admits one candidate output before consuming input or running viewport UI.
    ///
    /// Return [`NativeOutputAdmission::Proceed`] only after reserving storage for
    /// the token and its terminal result. The host may attach affine painted
    /// output after the core frame commits. [`NativeOutputAdmission::Defer`]
    /// leaves input buffered, produces no output callbacks for the candidate,
    /// and schedules a root repaint after the current native event. The callback
    /// must not re-enter eframe; retained facts must be copied before returning.
    fn on_output_begin(
        &self,
        _token: NativeOutputToken,
        _window: NativeWindowSnapshot,
        _root_roster: Option<NativeViewportRoster<'_>>,
    ) -> NativeOutputAdmission {
        NativeOutputAdmission::Proceed
    }

    /// Finalizes one egui pass while its exact native output token is active.
    ///
    /// This runs after all ordinary egui output hooks completed successfully
    /// and before eframe exposes the accumulated [`egui::FullOutput`] to the
    /// renderer. No egui plugin runs after this callback. An unwind before or
    /// during this callback terminates the token through [`Self::on_output`]
    /// with [`NativeOutputStatus::NotPresented`].
    fn on_output_pass_finalized(
        &self,
        _finalization: NativeOutputPassFinalization<'_>,
    ) -> NativeHostWake {
        NativeHostWake::Wait
    }

    /// Receives one terminal output result and decides whether queued work needs another frame.
    fn on_output(&self, _result: NativeOutputResult) -> NativeHostWake {
        NativeHostWake::Wait
    }

    /// Reports that eframe could not create the native window for a deferred viewport.
    fn on_viewport_create_failed(&self, _failure: NativeViewportCreateFailure) -> NativeHostWake {
        NativeHostWake::Wait
    }

    /// Reports one [`queue_native_viewport_visibility`] dispatch with its exact token.
    ///
    /// Ordinary visibility commands and builder changes never produce this callback.
    fn on_viewport_visibility(&self, _result: NativeViewportVisibilityResult) -> NativeHostWake {
        NativeHostWake::Wait
    }

    /// Reports that eframe consumed `CancelClose` for one exact viewport close request.
    fn on_viewport_close_cancelled(&self, _request: NativeViewportCloseRequest) -> NativeHostWake {
        NativeHostWake::Wait
    }
}

/// Returns the output token for the currently executing viewport UI callback.
///
/// The token is available only while eframe is running root or deferred viewport UI. It is never
/// stored in [`egui::FullOutput`] or application-owned user data.
pub fn current_native_output_token() -> Option<NativeOutputToken> {
    ACTIVE_OUTPUTS.with(|outputs| outputs.borrow().last().map(|output| output.token))
}

/// Keeps a deferred viewport's currently presented framebuffer instead of presenting this output.
///
/// This is available only while a native-host viewport callback is running. Eframe still owns
/// and forwards the generated context-global texture commands, but it skips painting and swapping
/// this viewport and reports [`NativeOutputStatus::NotPresented`] to the host. The previous native
/// framebuffer therefore remains visible while the host waits for an ordered input boundary.
///
/// Returns `false` when no native output callback is active, or when the exact native viewport
/// has not successfully presented a framebuffer for this window identity yet.
pub fn retain_current_native_output() -> bool {
    request_current_native_output_mode(NativeOutputRenderMode::Skip)
}

/// Keeps a native viewport's exact current framebuffer and paints this output over it.
///
/// This is available only while a native-host viewport callback is running. Eframe accepts the
/// request only when both the semantic presentation ledger and the renderer ledger prove the same
/// viewport, native window, physical inner size, and presentation generation. The renderer loads
/// that framebuffer instead of clearing it, paints this output as a visual-only overlay, and
/// reports [`NativeOutputStatus::NotPresented`] so the overlay cannot become semantic
/// presentation authority.
///
/// Returns `false` when no output callback is active or no exact renderer presentation can be
/// retained. Callers should then paint their ordinary no-frame fallback.
pub fn retain_current_native_output_with_overlay() -> bool {
    request_current_native_output_mode(NativeOutputRenderMode::Overlay)
}

fn request_current_native_output_mode(requested: NativeOutputRenderMode) -> bool {
    ACTIVE_OUTPUTS.with(|outputs| {
        let mut outputs = outputs.borrow_mut();
        let Some(output) = outputs.last_mut() else {
            return false;
        };
        if !output.retain_eligible {
            if requested == NativeOutputRenderMode::Overlay {
                output.render_mode = NativeOutputRenderMode::NoFrame;
            }
            return false;
        }
        if requested == NativeOutputRenderMode::Overlay
            || output.render_mode == NativeOutputRenderMode::Replace
        {
            output.render_mode = requested;
        }
        true
    })
}

#[derive(Clone, Default)]
pub(crate) struct NativeHostState {
    inner: Option<Arc<NativeHostStateInner>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct NativeWindowInputRecord {
    window_id: WindowId,
    state: NativeWindowInputState,
}

#[derive(Debug, Default)]
struct NativeWindowInputLedger {
    records: BTreeMap<ViewportId, NativeWindowInputRecord>,
}

impl NativeWindowInputLedger {
    fn capture(&mut self, viewport_id: ViewportId, window_id: WindowId) -> NativeWindowInputState {
        let record = self
            .records
            .entry(viewport_id)
            .and_modify(|record| {
                if record.window_id != window_id {
                    *record = NativeWindowInputRecord {
                        window_id,
                        state: NativeWindowInputState::ReceivesInput,
                    };
                }
            })
            .or_insert(NativeWindowInputRecord {
                window_id,
                state: NativeWindowInputState::ReceivesInput,
            });
        record.state
    }

    fn record_applied(
        &mut self,
        viewport_id: ViewportId,
        window_id: WindowId,
        state: NativeWindowInputState,
    ) -> bool {
        if let Some(record) = self.records.get_mut(&viewport_id) {
            if record.window_id != window_id {
                return false;
            }
            record.state = state;
        } else {
            self.records
                .insert(viewport_id, NativeWindowInputRecord { window_id, state });
        }
        true
    }

    fn forget_window(&mut self, viewport_id: ViewportId, window_id: WindowId) {
        if self
            .records
            .get(&viewport_id)
            .is_some_and(|record| record.window_id == window_id)
        {
            self.records.remove(&viewport_id);
        }
    }

    fn forget_viewport(&mut self, viewport_id: ViewportId) {
        self.records.remove(&viewport_id);
    }
}

struct NativeHostStateInner {
    handler: Arc<dyn NativeHostHandler>,
    attachment: NativeHostAttachment,
    next_token: AtomicU64,
    next_output: AtomicU64,
    next_create_attempt: AtomicU64,
    pending_viewport_closes: Mutex<BTreeMap<ViewportId, PendingNativeViewportClose>>,
    viewport_property_commands: egui::mutex::Mutex<QueuedNativeViewportPropertyCommands>,
    presented_outputs: Mutex<BTreeMap<ViewportId, NativeRetentionRecord>>,
    renderer_outputs: Mutex<BTreeMap<ViewportId, NativeRetentionRecord>>,
    presentation_generations: Mutex<BTreeMap<ViewportId, u64>>,
    window_input: egui::mutex::Mutex<NativeWindowInputLedger>,
    root_wake_after_current: AtomicBool,
}

/// Single-threaded event ordinal source owned by the outer winit dispatcher.
#[derive(Default)]
pub(crate) struct NativeEventSequencer {
    next: u64,
}

impl NativeEventSequencer {
    pub(crate) fn next(&mut self) -> NativeEventOrdinal {
        let ordinal = NonZeroU64::new(
            self.next
                .checked_add(1)
                .unwrap_or_else(|| panic!("native event ordinal exhausted")),
        )
        .unwrap_or_else(|| panic!("native event ordinal exhausted"));
        self.next = ordinal.get();
        NativeEventOrdinal(ordinal)
    }
}

impl NativeHostState {
    pub(crate) fn from_options(options: &crate::NativeOptions) -> Self {
        Self::new(options.native_host.clone())
    }

    pub(crate) fn new(handler: Option<Arc<dyn NativeHostHandler>>) -> Self {
        let Some(handler) = handler else {
            return Self::default();
        };
        let attachment = NativeHostAttachment {
            context: next_non_zero(&NEXT_CONTEXT_ID, "native context identity exhausted"),
        };
        assert!(
            handler.try_attach(attachment),
            "the native host is already attached to another eframe context"
        );
        Self {
            inner: Some(Arc::new(NativeHostStateInner {
                handler,
                attachment,
                next_token: AtomicU64::new(1),
                next_output: AtomicU64::new(1),
                next_create_attempt: AtomicU64::new(1),
                pending_viewport_closes: Mutex::new(BTreeMap::new()),
                viewport_property_commands: egui::mutex::Mutex::new(
                    QueuedNativeViewportPropertyCommands::default(),
                ),
                presented_outputs: Mutex::new(BTreeMap::new()),
                renderer_outputs: Mutex::new(BTreeMap::new()),
                presentation_generations: Mutex::new(BTreeMap::new()),
                window_input: egui::mutex::Mutex::new(NativeWindowInputLedger::default()),
                root_wake_after_current: AtomicBool::new(false),
            })),
        }
    }

    pub(crate) fn is_enabled(&self) -> bool {
        self.inner.is_some()
    }

    pub(crate) fn take_root_wake_after_current(&self) -> bool {
        self.inner
            .as_ref()
            .is_some_and(|inner| inner.root_wake_after_current.swap(false, Ordering::AcqRel))
    }

    #[track_caller]
    pub(crate) fn assert_immediate_viewports_supported(&self) {
        assert!(
            !self.is_enabled(),
            "egui::show_viewport_immediate is unavailable while a native host is installed; use egui::Context::show_viewport_deferred"
        );
    }

    pub(crate) fn observe_window_event(
        &self,
        ctx: Option<&egui::Context>,
        ordinal: NativeEventOrdinal,
        window_id: WindowId,
        viewport_id: Option<ViewportId>,
        local_geometry: Option<NativeWindowLocalGeometry>,
        event: &winit::event::WindowEvent,
    ) -> NativeWindowEventDisposition {
        let Some(inner) = &self.inner else {
            return NativeWindowEventDisposition::Continue;
        };
        if matches!(event, winit::event::WindowEvent::CloseRequested)
            && let Some(ctx) = ctx
        {
            self.bind_close_context(ctx);
        }
        let close_request = if let Some(viewport_id) = viewport_id
            && matches!(
                event,
                winit::event::WindowEvent::CloseRequested | winit::event::WindowEvent::Destroyed
            ) {
            let mut pending = inner
                .pending_viewport_closes
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            if matches!(event, winit::event::WindowEvent::Destroyed) {
                if pending
                    .get(&viewport_id)
                    .is_some_and(|close| close.request.window_id == window_id)
                {
                    pending.remove(&viewport_id);
                }
                None
            } else if matches!(event, winit::event::WindowEvent::CloseRequested) {
                if pending
                    .get(&viewport_id)
                    .is_some_and(|close| close.request.window_id != window_id)
                {
                    pending.remove(&viewport_id);
                }
                Some(
                    pending
                        .entry(viewport_id)
                        .or_insert_with(|| PendingNativeViewportClose {
                            request: NativeViewportCloseRequest {
                                attachment: inner.attachment,
                                event: ordinal,
                                viewport_id,
                                window_id,
                            },
                            ownership: NativeCloseOwnership::Application,
                        })
                        .request,
                )
            } else {
                None
            }
        } else {
            None
        };
        let disposition = inner.handler.on_window_event(NativeWindowEvent {
            ordinal,
            window_id,
            viewport_id,
            local_geometry,
            event,
            close_request,
        });
        if let Some(ctx) = ctx {
            ctx.request_repaint_of(ViewportId::ROOT);
        }
        if let Some(request) = close_request {
            let mut pending = inner
                .pending_viewport_closes
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            if let Some(close) = pending.get_mut(&request.viewport_id)
                && close.request == request
            {
                if disposition == NativeWindowEventDisposition::HoldClose(request)
                    && close.ownership == NativeCloseOwnership::Application
                {
                    close.ownership = NativeCloseOwnership::Held { decision: None };
                }
                if matches!(close.ownership, NativeCloseOwnership::Held { .. }) {
                    return NativeWindowEventDisposition::HoldClose(request);
                }
            }
        }
        NativeWindowEventDisposition::Continue
    }

    pub(crate) fn bind_close_context(&self, ctx: &egui::Context) {
        if let Some(inner) = &self.inner {
            ctx.data_mut(|data| {
                data.insert_temp(
                    close_resolution_handle_id(),
                    NativeCloseResolutionHandle(Arc::downgrade(inner)),
                )
            });
        }
    }

    pub(crate) fn is_close_held(&self, viewport_id: ViewportId) -> bool {
        self.inner.as_ref().is_some_and(|inner| {
            inner
                .pending_viewport_closes
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .get(&viewport_id)
                .is_some_and(|close| matches!(close.ownership, NativeCloseOwnership::Held { .. }))
        })
    }

    pub(crate) fn prepare_close_input(&self, input: &mut egui::RawInput) {
        for (viewport, info) in &mut input.viewports {
            if self.is_close_held(*viewport) {
                info.events
                    .retain(|event| !matches!(event, egui::ViewportEvent::Close));
            }
        }
    }

    pub(crate) fn capture_viewport_info(
        &self,
        viewport: ViewportId,
        info: &mut egui::ViewportInfo,
    ) -> egui::ViewportInfo {
        // Consume the renderer-owned event, not just the input clone. Another
        // viewport may resolve this request before its own next update.
        if self.is_close_held(viewport) {
            info.events
                .retain(|event| !matches!(event, egui::ViewportEvent::Close));
        }
        info.clone()
    }

    /// Consumes only final decisions from the existing close ledger. Child
    /// Allow releases the host's gate; the host still owns child retirement.
    pub(crate) fn settle_close_resolutions(&self, ctx: &egui::Context) -> bool {
        self.bind_close_context(ctx);
        let Some(inner) = &self.inner else {
            return false;
        };
        let decided = {
            let mut pending = inner
                .pending_viewport_closes
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            let mut decided = Vec::new();
            pending.retain(|_, close| {
                if let NativeCloseOwnership::Held {
                    decision: Some(decision),
                } = close.ownership
                {
                    decided.push((close.request, decision));
                    false
                } else {
                    true
                }
            });
            decided
        };
        let mut root_allowed = false;
        for (request, decision) in decided {
            match decision {
                NativeViewportCloseDecision::Allow => {
                    root_allowed |= request.viewport_id == ViewportId::ROOT
                }
                NativeViewportCloseDecision::Veto => {
                    if inner.handler.on_viewport_close_cancelled(request)
                        == NativeHostWake::RepaintRoot
                    {
                        ctx.request_repaint_of(ViewportId::ROOT);
                    }
                }
            }
        }
        root_allowed
    }

    pub(crate) fn observe_device_removal(
        &self,
        ctx: Option<&egui::Context>,
        ordinal: NativeEventOrdinal,
    ) {
        let Some(inner) = &self.inner else {
            return;
        };
        inner
            .handler
            .on_device_removed(NativeDeviceRemoval { ordinal });
        if let Some(ctx) = ctx {
            ctx.request_repaint_of(ViewportId::ROOT);
        }
    }

    pub(crate) fn observe_global_focus(
        &self,
        ctx: Option<&egui::Context>,
        event: NativeEventOrdinal,
        focused: Option<(ViewportId, WindowId)>,
    ) {
        let Some(inner) = &self.inner else {
            return;
        };
        let focused = focused.map_or(NativeGlobalFocus::Unknown, |(viewport_id, window_id)| {
            NativeGlobalFocus::Viewport {
                viewport_id,
                window_id,
            }
        });
        if inner
            .handler
            .on_global_focus(NativeGlobalFocusObservation { event, focused })
            == NativeHostWake::RepaintRoot
            && let Some(ctx) = ctx
        {
            ctx.request_repaint_of(ViewportId::ROOT);
        }
    }

    pub(crate) fn finish_viewport_close_request(
        &self,
        ctx: &egui::Context,
        viewport_id: ViewportId,
        window_id: WindowId,
        cancelled: bool,
    ) {
        let Some(inner) = &self.inner else {
            return;
        };
        let request = {
            let mut pending = inner
                .pending_viewport_closes
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            if !pending.get(&viewport_id).is_some_and(|close| {
                close.request.window_id == window_id
                    && close.ownership == NativeCloseOwnership::Application
            }) {
                return;
            }
            pending.remove(&viewport_id).map(|close| close.request)
        };
        if !cancelled {
            return;
        }
        let Some(request) = request else {
            return;
        };
        if inner.handler.on_viewport_close_cancelled(request) == NativeHostWake::RepaintRoot {
            ctx.request_repaint_of(ViewportId::ROOT);
        }
    }

    pub(crate) fn capture_window_snapshot(
        &self,
        egui_ctx: &egui::Context,
        viewport_id: ViewportId,
        window: &Window,
    ) -> NativeWindowSnapshot {
        let input_state = self
            .inner
            .as_ref()
            .map_or(NativeWindowInputState::ReceivesInput, |inner| {
                inner.capture_window_input_state(viewport_id, window.id())
            });
        NativeWindowSnapshot::capture(egui_ctx, window, input_state)
    }

    pub(crate) fn capture_viewport_record(
        &self,
        viewport_id: ViewportId,
        egui_ctx: &egui::Context,
        window: &Window,
    ) -> NativeViewportRecord {
        let input_state = self
            .inner
            .as_ref()
            .map_or(NativeWindowInputState::ReceivesInput, |inner| {
                inner.capture_window_input_state(viewport_id, window.id())
            });
        NativeViewportRecord::capture(viewport_id, egui_ctx, window, input_state)
    }

    pub(crate) fn apply_viewport_builder_to_window(
        &self,
        egui_ctx: &egui::Context,
        viewport_id: ViewportId,
        window: &Window,
        builder: &ViewportBuilder,
    ) {
        if let Some(inner) = &self.inner {
            let _ = inner.capture_window_input_state(viewport_id, window.id());
        }
        let Some(pointer_passthrough) = builder.mouse_passthrough else {
            egui_winit::apply_viewport_builder_to_window(egui_ctx, window, builder);
            return;
        };
        if self.inner.is_none() {
            egui_winit::apply_viewport_builder_to_window(egui_ctx, window, builder);
            return;
        }

        let mut remaining = builder.clone();
        remaining.mouse_passthrough = None;
        egui_winit::apply_viewport_builder_to_window(egui_ctx, window, &remaining);
        if let Err(error) = self.apply_window_input_state(viewport_id, window, pointer_passthrough)
        {
            log::warn!("set_cursor_hittest failed: {error}");
        }
    }

    pub(crate) fn apply_window_input_state(
        &self,
        viewport_id: ViewportId,
        window: &Window,
        pointer_passthrough: bool,
    ) -> Result<NativeWindowInputState, winit::error::ExternalError> {
        let state = apply_window_input_state(window, pointer_passthrough)?;
        if let Some(inner) = &self.inner
            && !inner.record_applied_window_input_state(viewport_id, window.id(), state)
        {
            log::warn!(
                "discarding native input state from stale window {:?} for viewport {viewport_id:?}",
                window.id()
            );
        }
        Ok(state)
    }

    pub(crate) fn begin_output(
        &self,
        ctx: &egui::Context,
        viewport_id: ViewportId,
        window_id: WindowId,
        create_attempt: Option<NativeViewportCreateAttempt>,
        snapshot: NativeWindowSnapshot,
        root_roster: Option<NativeViewportRoster<'_>>,
    ) -> NativeOutputStart {
        self.bind_close_context(ctx);
        let Some(inner) = &self.inner else {
            return NativeOutputStart::Disabled;
        };
        let inner = Arc::clone(inner);
        debug_assert!(
            create_attempt.is_none_or(|attempt| {
                attempt.context == inner.attachment.context && attempt.viewport_id == viewport_id
            }),
            "native output attempt must belong to the producing context and viewport"
        );
        let token = NativeOutputToken {
            context: inner.attachment.context,
            nonce: next_non_zero(&inner.next_token, "native output token exhausted"),
            viewport_id,
            window_id,
            create_attempt,
        };
        // The native dispatcher consumed this wake even if admission runs no
        // egui pass. Re-arm before application callbacks can request a retry.
        ctx.acknowledge_repaint_request(viewport_id);
        if inner.handler.on_output_begin(token, snapshot, root_roster)
            == NativeOutputAdmission::Defer
        {
            apply_host_wake(&inner, ctx, NativeHostWake::RepaintRootAfterCurrent);
            return NativeOutputStart::Deferred;
        }
        let frame = inner.output_frame(viewport_id, window_id, snapshot);
        let retain_eligible = inner.has_presented_output(viewport_id, frame)
            && inner.has_renderer_output(viewport_id, frame);
        ACTIVE_OUTPUTS.with(|outputs| {
            outputs.borrow_mut().push(ActiveNativeOutput {
                token,
                render_mode: NativeOutputRenderMode::Replace,
                retain_eligible,
            });
        });
        NativeOutputStart::Accepted(NativeOutputScope {
            inner,
            ctx: ctx.clone(),
            token,
            frame,
            active: true,
        })
    }

    pub(crate) fn finalize_output_pass(
        &self,
        context: &egui::Context,
        ended_viewport: ViewportId,
        disposition: egui::OutputPassDisposition,
        output: &mut egui::FullOutput,
    ) {
        let Some(inner) = &self.inner else {
            return;
        };
        let token =
            ACTIVE_OUTPUTS.with(|outputs| outputs.borrow().last().map(|active| active.token));
        let Some(token) = token else {
            return;
        };
        let wake = inner
            .handler
            .on_output_pass_finalized(NativeOutputPassFinalization {
                context,
                token,
                ended_viewport,
                disposition,
                output,
            });
        apply_host_wake(inner, context, wake);
    }

    #[cfg(test)]
    fn begin_output_for_test(
        &self,
        ctx: &egui::Context,
        viewport_id: ViewportId,
        window_id: WindowId,
    ) -> Option<NativeOutputScope> {
        self.begin_output(
            ctx,
            viewport_id,
            window_id,
            None,
            NativeWindowSnapshot {
                content_size: [0, 0],
                inner_rect: None,
                outer_rect: None,
                native_scale_factor: 1.0,
                presentation_scale_factor: 1.0,
                visible: None,
                minimized: None,
                input_state: NativeWindowInputState::ReceivesInput,
            },
            None,
        )
        .accepted()
    }

    pub(crate) fn forget_presented_window(&self, viewport_id: ViewportId, window_id: WindowId) {
        let Some(inner) = &self.inner else {
            return;
        };
        inner.forget_presented_window(viewport_id, window_id);
    }

    pub(crate) fn forget_presented_viewport(&self, viewport_id: ViewportId) {
        let Some(inner) = &self.inner else {
            return;
        };
        inner.forget_presented_viewport(viewport_id);
    }

    pub(crate) fn invalidate_presented_viewport(&self, viewport_id: ViewportId) {
        let Some(inner) = &self.inner else {
            return;
        };
        inner.invalidate_presented_viewport(viewport_id);
    }

    pub(crate) fn prepare_deferred_window(
        &self,
        ctx: &egui::Context,
        visibility_status: NativeViewportVisibilityStatus,
        viewport_id: ViewportId,
        builder: &ViewportBuilder,
    ) -> NativeDeferredWindowPreparation {
        let Some(inner) = &self.inner else {
            return NativeDeferredWindowPreparation::Unmanaged;
        };
        if viewport_id == ViewportId::ROOT {
            return NativeDeferredWindowPreparation::Unmanaged;
        }

        let create_attempt = NativeViewportCreateAttempt {
            context: inner.attachment.context,
            nonce: next_non_zero(
                &inner.next_create_attempt,
                "native viewport create attempt exhausted",
            ),
            viewport_id,
        };
        let NativeViewportCreateAdmission::Proceed {
            undecorated_outer_rect,
        } = inner.handler.begin_deferred_viewport_create(create_attempt)
        else {
            return NativeDeferredWindowPreparation::Defer;
        };

        if self.render_hidden_deferred_viewport(viewport_id)
            && visibility_status == NativeViewportVisibilityStatus::Unsupported
        {
            self.notify_viewport_create_failed(
                ctx,
                create_attempt,
                NativeViewportCreateFailureKind::VisibilityUnsupported,
            );
            return NativeDeferredWindowPreparation::Failed;
        }

        let window_override = undecorated_outer_rect.and_then(|rect| {
            if builder.fullscreen == Some(true)
                || builder.maximized == Some(true)
                || builder.monitor.is_some()
            {
                log::warn!(
                    "ignoring native host geometry for viewport {viewport_id:?}: fullscreen, maximized, and monitor-targeted builders are incompatible with an exact outer-rectangle request"
                );
                return None;
            }
            if rect.width == 0 || rect.height == 0 {
                log::warn!(
                    "ignoring native host geometry for viewport {viewport_id:?}: physical size must be non-zero"
                );
                return None;
            }
            Some(NativeDeferredWindowOverride { rect })
        });

        NativeDeferredWindowPreparation::Admitted {
            create_attempt,
            window_override,
        }
    }

    pub(crate) fn render_hidden_deferred_viewport(&self, viewport_id: ViewportId) -> bool {
        viewport_id != ViewportId::ROOT
            && self
                .inner
                .as_ref()
                .is_some_and(|inner| inner.handler.render_hidden_deferred_viewport(viewport_id))
    }

    pub(crate) fn notify_viewport_create_failed(
        &self,
        ctx: &egui::Context,
        create_attempt: NativeViewportCreateAttempt,
        kind: NativeViewportCreateFailureKind,
    ) {
        let Some(inner) = &self.inner else {
            return;
        };
        if inner
            .handler
            .on_viewport_create_failed(NativeViewportCreateFailure {
                create_attempt,
                kind,
            })
            == NativeHostWake::RepaintRoot
        {
            ctx.request_repaint_of(ViewportId::ROOT);
        }
    }

    fn notify_viewport_visibility_result(
        &self,
        ctx: &egui::Context,
        result: NativeViewportVisibilityResult,
    ) {
        let Some(inner) = &self.inner else {
            return;
        };
        apply_host_wake(inner, ctx, inner.handler.on_viewport_visibility(result));
    }

    pub(crate) fn deferred_visibility_status(
        &self,
        event_loop: &ActiveEventLoop,
    ) -> NativeViewportVisibilityStatus {
        visibility_control_for_event_loop(event_loop)
    }

    #[expect(clippy::too_many_arguments)]
    pub(crate) fn process_viewport_commands(
        &self,
        ctx: &egui::Context,
        viewport_id: ViewportId,
        info: &mut egui::ViewportInfo,
        commands: impl IntoIterator<Item = egui::ViewportCommand>,
        window: &Window,
        actions_requested: &mut Vec<egui_winit::ActionRequested>,
        event_loop: Option<&ActiveEventLoop>,
    ) {
        self.process_viewport_commands_with(
            ctx,
            viewport_id,
            commands,
            |command| {
                if self.inner.is_some()
                    && let egui::ViewportCommand::MousePassthrough(enabled) = &command
                {
                    if let Err(error) = self.apply_window_input_state(viewport_id, window, *enabled)
                    {
                        log::warn!("{command:?}: {error}");
                    }
                } else {
                    egui_winit::process_viewport_commands(
                        ctx,
                        info,
                        std::iter::once(command),
                        window,
                        actions_requested,
                    );
                }
            },
            |command| {
                self.apply_viewport_property_command(viewport_id, window, command, event_loop)
            },
        );
    }

    /// Includes typed targets in logic-only dispatch without queuing ordinary property commands.
    pub(crate) fn with_pending_viewport_property_targets(
        &self,
        mut commands: egui::OrderedViewportIdMap<Vec<egui::ViewportCommand>>,
    ) -> egui::OrderedViewportIdMap<Vec<egui::ViewportCommand>> {
        if let Some(inner) = &self.inner {
            for &viewport_id in inner.viewport_property_commands.lock().0.keys() {
                commands.entry(viewport_id).or_default();
            }
        }
        commands
    }

    // Keep ordinary dispatch and typed queue consumption at the same boundary in both backends.
    // The closures also let tests exercise that boundary without creating an OS window.
    fn process_viewport_commands_with(
        &self,
        ctx: &egui::Context,
        viewport_id: ViewportId,
        commands: impl IntoIterator<Item = egui::ViewportCommand>,
        mut apply_ordinary: impl FnMut(egui::ViewportCommand),
        mut apply_property: impl FnMut(
            QueuedNativeViewportPropertyCommand,
        ) -> NativeViewportPropertyResult,
    ) {
        for command in commands {
            if matches!(
                command,
                egui::ViewportCommand::Close | egui::ViewportCommand::CancelClose
            ) && self.is_close_held(viewport_id)
            {
                continue;
            }
            apply_ordinary(command);
        }

        for command in self.take_viewport_property_commands(viewport_id) {
            match apply_property(command) {
                NativeViewportPropertyResult::Focus(result) => {
                    self.notify_viewport_focus(ctx, result);
                }
                NativeViewportPropertyResult::Visibility(result) => {
                    self.notify_viewport_visibility_result(ctx, result);
                }
                NativeViewportPropertyResult::PointerPassthrough(result) => {
                    self.notify_viewport_pointer_passthrough(ctx, result);
                }
                NativeViewportPropertyResult::PointerObservation(result) => {
                    self.notify_viewport_pointer_observation(ctx, result);
                }
            }
        }
    }

    fn take_viewport_property_commands(
        &self,
        viewport_id: ViewportId,
    ) -> VecDeque<QueuedNativeViewportPropertyCommand> {
        self.inner.as_ref().map_or_else(VecDeque::new, |inner| {
            inner
                .viewport_property_commands
                .lock()
                .0
                .remove(&viewport_id)
                .unwrap_or_default()
        })
    }

    fn apply_viewport_property_command(
        &self,
        viewport_id: ViewportId,
        window: &Window,
        command: QueuedNativeViewportPropertyCommand,
        event_loop: Option<&ActiveEventLoop>,
    ) -> NativeViewportPropertyResult {
        match command {
            QueuedNativeViewportPropertyCommand::Focus(token) => {
                let status = if window.has_focus() {
                    NativeViewportFocusStatus::AlreadyFocused
                } else {
                    window.focus_window();
                    NativeViewportFocusStatus::Requested
                };
                NativeViewportPropertyResult::Focus(NativeViewportFocusResult {
                    token,
                    viewport_id,
                    window_id: window.id(),
                    status,
                })
            }
            QueuedNativeViewportPropertyCommand::Visibility { token, visible } => {
                window.set_visible(visible);
                NativeViewportPropertyResult::Visibility(NativeViewportVisibilityResult {
                    token,
                    viewport_id,
                    window_id: window.id(),
                    visible,
                    status: visibility_control_for_window(window),
                })
            }
            QueuedNativeViewportPropertyCommand::PointerPassthrough { token, enabled } => {
                let status = match self.apply_window_input_state(viewport_id, window, enabled) {
                    Ok(_) => NativeViewportPointerPassthroughStatus::Applied,
                    Err(winit::error::ExternalError::NotSupported(_)) => {
                        NativeViewportPointerPassthroughStatus::Unsupported
                    }
                    Err(
                        winit::error::ExternalError::Ignored | winit::error::ExternalError::Os(_),
                    ) => NativeViewportPointerPassthroughStatus::Failed,
                };
                NativeViewportPropertyResult::PointerPassthrough(
                    NativeViewportPointerPassthroughResult {
                        token,
                        viewport_id,
                        window_id: window.id(),
                        enabled,
                        status,
                    },
                )
            }
            QueuedNativeViewportPropertyCommand::PointerObservation {
                token,
                expected_window_id,
                device_id,
            } => NativeViewportPropertyResult::PointerObservation(
                NativeViewportPointerObservationResult::observe_with(
                    token,
                    viewport_id,
                    expected_window_id,
                    device_id,
                    window.id(),
                    |expected_window_id, device_id| {
                        observe_native_pointer(event_loop, expected_window_id, device_id)
                    },
                ),
            ),
        }
    }

    fn notify_viewport_pointer_observation(
        &self,
        ctx: &egui::Context,
        result: NativeViewportPointerObservationResult,
    ) {
        let Some(inner) = &self.inner else {
            return;
        };
        apply_host_wake(
            inner,
            ctx,
            inner.handler.on_viewport_pointer_observation(result),
        );
    }

    fn notify_viewport_focus(&self, ctx: &egui::Context, result: NativeViewportFocusResult) {
        let Some(inner) = &self.inner else {
            return;
        };
        apply_host_wake(inner, ctx, inner.handler.on_viewport_focus(result));
    }

    fn notify_viewport_pointer_passthrough(
        &self,
        ctx: &egui::Context,
        result: NativeViewportPointerPassthroughResult,
    ) {
        let Some(inner) = &self.inner else {
            return;
        };
        apply_host_wake(
            inner,
            ctx,
            inner.handler.on_viewport_pointer_passthrough(result),
        );
    }
}

impl Drop for NativeHostStateInner {
    fn drop(&mut self) {
        self.handler.detach(self.attachment);
    }
}

fn apply_window_input_state(
    window: &Window,
    pointer_passthrough: bool,
) -> Result<NativeWindowInputState, winit::error::ExternalError> {
    window.set_cursor_hittest(!pointer_passthrough)?;
    Ok(if pointer_passthrough {
        NativeWindowInputState::PassThrough
    } else {
        NativeWindowInputState::ReceivesInput
    })
}

#[cfg(target_os = "linux")]
fn visibility_control_for_window(window: &Window) -> NativeViewportVisibilityStatus {
    window.window_handle().map_or(
        NativeViewportVisibilityStatus::Unsupported,
        |handle| match handle.as_raw() {
            raw_window_handle::RawWindowHandle::Xlib(_)
            | raw_window_handle::RawWindowHandle::Xcb(_) => {
                NativeViewportVisibilityStatus::Dispatched
            }
            raw_window_handle::RawWindowHandle::Wayland(_) => {
                NativeViewportVisibilityStatus::Unsupported
            }
            _ => NativeViewportVisibilityStatus::Unsupported,
        },
    )
}

#[cfg(any(target_os = "windows", target_os = "macos"))]
fn visibility_control_for_window(_window: &Window) -> NativeViewportVisibilityStatus {
    NativeViewportVisibilityStatus::Dispatched
}

#[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
fn visibility_control_for_window(_window: &Window) -> NativeViewportVisibilityStatus {
    NativeViewportVisibilityStatus::Unsupported
}

#[cfg(target_os = "linux")]
fn visibility_control_for_event_loop(
    event_loop: &ActiveEventLoop,
) -> NativeViewportVisibilityStatus {
    event_loop
        .display_handle()
        .map_or(
            NativeViewportVisibilityStatus::Unsupported,
            |handle| match handle.as_raw() {
                raw_window_handle::RawDisplayHandle::Xlib(_)
                | raw_window_handle::RawDisplayHandle::Xcb(_) => {
                    NativeViewportVisibilityStatus::Dispatched
                }
                raw_window_handle::RawDisplayHandle::Wayland(_) => {
                    NativeViewportVisibilityStatus::Unsupported
                }
                _ => NativeViewportVisibilityStatus::Unsupported,
            },
        )
}

#[cfg(any(target_os = "windows", target_os = "macos"))]
fn visibility_control_for_event_loop(
    _event_loop: &ActiveEventLoop,
) -> NativeViewportVisibilityStatus {
    NativeViewportVisibilityStatus::Dispatched
}

#[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
fn visibility_control_for_event_loop(
    _event_loop: &ActiveEventLoop,
) -> NativeViewportVisibilityStatus {
    NativeViewportVisibilityStatus::Unsupported
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum NativeDeferredWindowPreparation {
    Unmanaged,
    Admitted {
        create_attempt: NativeViewportCreateAttempt,
        window_override: Option<NativeDeferredWindowOverride>,
    },
    Defer,
    Failed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct NativeDeferredWindowOverride {
    rect: NativePhysicalRect,
}

impl NativeDeferredWindowOverride {
    pub(crate) fn apply_to_attributes(self, attributes: WindowAttributes) -> WindowAttributes {
        let rect = self.rect;
        attributes
            .with_decorations(false)
            .with_position(winit::dpi::PhysicalPosition::new(rect.x, rect.y))
            .with_inner_size(winit::dpi::PhysicalSize::new(rect.width, rect.height))
    }

    pub(crate) fn reapply_to_window(self, window: &Window) {
        let rect = self.rect;
        window.set_decorations(false);
        window.set_outer_position(winit::dpi::PhysicalPosition::new(rect.x, rect.y));
        let _ = window.request_inner_size(winit::dpi::PhysicalSize::new(rect.width, rect.height));
    }
}

/// Distinguishes ordinary rendering without a host from deferred host admission.
#[must_use]
pub(crate) enum NativeOutputStart {
    Disabled,
    Deferred,
    Accepted(NativeOutputScope),
}

#[cfg(test)]
impl NativeOutputStart {
    fn accepted(self) -> Option<NativeOutputScope> {
        match self {
            Self::Accepted(scope) => Some(scope),
            Self::Disabled | Self::Deferred => None,
        }
    }
}

pub(crate) struct NativeOutputScope {
    inner: Arc<NativeHostStateInner>,
    ctx: egui::Context,
    token: NativeOutputToken,
    frame: PresentedNativeOutputFrame,
    active: bool,
}

impl NativeOutputScope {
    pub(crate) fn finish(mut self) -> NativeOutputSettlement {
        let render_mode = self.leave();
        NativeOutputSettlement {
            inner: Arc::clone(&self.inner),
            ctx: self.ctx.clone(),
            token: self.token,
            frame: self.frame,
            ordinal: NativeOutputOrdinal(next_non_zero(
                &self.inner.next_output,
                "native output ordinal exhausted",
            )),
            settled: false,
            render_mode,
        }
    }

    fn leave(&mut self) -> NativeOutputRenderMode {
        if !self.active {
            return NativeOutputRenderMode::Replace;
        }
        let render_mode = ACTIVE_OUTPUTS.with(|outputs| {
            let popped = outputs.borrow_mut().pop();
            debug_assert_eq!(
                popped.map(|output| output.token),
                Some(self.token),
                "native output scopes must leave in stack order"
            );
            popped.map_or(NativeOutputRenderMode::Replace, |output| output.render_mode)
        });
        self.active = false;
        render_mode
    }
}

impl Drop for NativeOutputScope {
    fn drop(&mut self) {
        if !self.active {
            return;
        }
        self.leave();
        let ordinal = NativeOutputOrdinal(next_non_zero(
            &self.inner.next_output,
            "native output ordinal exhausted",
        ));
        let notify = || {
            notify_output(
                &self.inner,
                &self.ctx,
                NativeOutputResult {
                    token: self.token,
                    ordinal,
                    status: NativeOutputStatus::NotPresented,
                },
                self.frame,
                None,
            );
        };
        if std::thread::panicking() {
            if std::panic::catch_unwind(std::panic::AssertUnwindSafe(notify)).is_err() {
                log::error!(
                    "native output abandonment callback also unwound; suppressing the secondary panic"
                );
            }
        } else {
            notify();
        }
    }
}

pub(crate) struct NativeOutputSettlement {
    inner: Arc<NativeHostStateInner>,
    ctx: egui::Context,
    token: NativeOutputToken,
    frame: PresentedNativeOutputFrame,
    ordinal: NativeOutputOrdinal,
    settled: bool,
    render_mode: NativeOutputRenderMode,
}

impl NativeOutputSettlement {
    pub(crate) const fn should_present(&self) -> bool {
        !matches!(self.render_mode, NativeOutputRenderMode::Skip)
    }

    pub(crate) const fn render_mode(&self) -> NativeOutputRenderMode {
        self.render_mode
    }

    pub(crate) fn present(mut self) {
        let status = match self.render_mode {
            NativeOutputRenderMode::Replace => NativeOutputStatus::Presented,
            NativeOutputRenderMode::NoFrame
            | NativeOutputRenderMode::Skip
            | NativeOutputRenderMode::Overlay => NativeOutputStatus::NotPresented,
        };
        let renderer_output = match self.render_mode {
            NativeOutputRenderMode::Replace | NativeOutputRenderMode::Overlay => {
                Some(NativeRetentionKind::Retainable)
            }
            NativeOutputRenderMode::NoFrame => Some(NativeRetentionKind::NoFrame),
            NativeOutputRenderMode::Skip => None,
        };
        self.settle(status, renderer_output);
    }

    fn settle(&mut self, status: NativeOutputStatus, renderer_output: Option<NativeRetentionKind>) {
        if self.settled {
            return;
        }
        self.settled = true;
        notify_output(
            &self.inner,
            &self.ctx,
            NativeOutputResult {
                token: self.token,
                ordinal: self.ordinal,
                status,
            },
            self.frame,
            renderer_output,
        );
    }
}

impl Drop for NativeOutputSettlement {
    fn drop(&mut self) {
        self.settle(NativeOutputStatus::NotPresented, None);
    }
}

impl NativeHostStateInner {
    fn capture_window_input_state(
        &self,
        viewport_id: ViewportId,
        window_id: WindowId,
    ) -> NativeWindowInputState {
        self.window_input.lock().capture(viewport_id, window_id)
    }

    fn record_applied_window_input_state(
        &self,
        viewport_id: ViewportId,
        window_id: WindowId,
        state: NativeWindowInputState,
    ) -> bool {
        self.window_input
            .lock()
            .record_applied(viewport_id, window_id, state)
    }

    fn output_frame(
        &self,
        viewport_id: ViewportId,
        window_id: WindowId,
        snapshot: NativeWindowSnapshot,
    ) -> PresentedNativeOutputFrame {
        let [width, height] = snapshot.content_size();
        PresentedNativeOutputFrame {
            window_id,
            inner_size: Some((width, height)),
            generation: self.current_presentation_generation(viewport_id),
        }
    }

    fn current_presentation_generation(&self, viewport_id: ViewportId) -> u64 {
        *self
            .presentation_generations
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&viewport_id)
            .unwrap_or(&0)
    }

    fn has_presented_output(
        &self,
        viewport_id: ViewportId,
        frame: PresentedNativeOutputFrame,
    ) -> bool {
        self.presented_outputs
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&viewport_id)
            .is_some_and(|output| output.retains(frame))
    }

    fn has_renderer_output(
        &self,
        viewport_id: ViewportId,
        frame: PresentedNativeOutputFrame,
    ) -> bool {
        self.renderer_outputs
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&viewport_id)
            .is_some_and(|output| output.retains(frame))
    }

    fn record_renderer_output(
        &self,
        viewport_id: ViewportId,
        ordinal: NativeOutputOrdinal,
        frame: PresentedNativeOutputFrame,
        kind: NativeRetentionKind,
    ) -> bool {
        record_newer_retention(&self.renderer_outputs, viewport_id, ordinal, frame, kind)
    }

    fn record_output_result(
        &self,
        result: NativeOutputResult,
        frame: PresentedNativeOutputFrame,
        submitted_no_frame: bool,
    ) {
        let kind = match result.status {
            NativeOutputStatus::Presented => NativeRetentionKind::Retainable,
            NativeOutputStatus::NotPresented if submitted_no_frame => NativeRetentionKind::NoFrame,
            NativeOutputStatus::NotPresented => return,
        };
        let _ = record_newer_retention(
            &self.presented_outputs,
            result.token.viewport_id,
            result.ordinal,
            frame,
            kind,
        );
    }

    fn forget_presented_window(&self, viewport_id: ViewportId, window_id: WindowId) {
        let retired_commands = {
            let window_input = self.window_input.lock();
            if window_input
                .records
                .get(&viewport_id)
                .is_none_or(|record| record.window_id == window_id)
            {
                self.viewport_property_commands
                    .lock()
                    .0
                    .remove(&viewport_id)
                    .unwrap_or_default()
            } else {
                VecDeque::new()
            }
        };
        self.pending_viewport_closes
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .retain(|viewport, close| {
                *viewport != viewport_id || close.request.window_id != window_id
            });
        let mut presented_outputs = self
            .presented_outputs
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if presented_outputs
            .get(&viewport_id)
            .is_some_and(|output| output.frame.window_id == window_id)
        {
            presented_outputs.remove(&viewport_id);
        }
        drop(presented_outputs);
        let mut renderer_outputs = self
            .renderer_outputs
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if renderer_outputs
            .get(&viewport_id)
            .is_some_and(|output| output.frame.window_id == window_id)
        {
            renderer_outputs.remove(&viewport_id);
        }
        drop(renderer_outputs);
        self.window_input
            .lock()
            .forget_window(viewport_id, window_id);
        self.bump_presentation_generation(viewport_id);
        self.notify_retired_property_commands(viewport_id, retired_commands);
    }

    fn forget_presented_viewport(&self, viewport_id: ViewportId) {
        let retired_commands = self
            .viewport_property_commands
            .lock()
            .0
            .remove(&viewport_id)
            .unwrap_or_default();
        self.pending_viewport_closes
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&viewport_id);
        self.presented_outputs
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&viewport_id);
        self.renderer_outputs
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&viewport_id);
        self.window_input.lock().forget_viewport(viewport_id);
        self.bump_presentation_generation(viewport_id);
        self.notify_retired_property_commands(viewport_id, retired_commands);
    }

    fn notify_retired_property_commands(
        &self,
        viewport_id: ViewportId,
        commands: VecDeque<QueuedNativeViewportPropertyCommand>,
    ) {
        // The queue is detached and all ledger cleanup is complete before host code runs.
        // Reentrant queueing creates a successor queue which this loop cannot remove.
        for command in commands {
            let retirement = match command {
                QueuedNativeViewportPropertyCommand::Focus(token) => {
                    NativeViewportPropertyCommandRetirement::Focus { viewport_id, token }
                }
                QueuedNativeViewportPropertyCommand::Visibility { token, visible } => {
                    NativeViewportPropertyCommandRetirement::Visibility {
                        viewport_id,
                        token,
                        visible,
                    }
                }
                QueuedNativeViewportPropertyCommand::PointerPassthrough { token, enabled } => {
                    NativeViewportPropertyCommandRetirement::PointerPassthrough {
                        viewport_id,
                        token,
                        enabled,
                    }
                }
                QueuedNativeViewportPropertyCommand::PointerObservation {
                    token,
                    expected_window_id,
                    device_id,
                } => NativeViewportPropertyCommandRetirement::PointerObservation {
                    viewport_id,
                    token,
                    expected_window_id,
                    device_id,
                },
            };
            if self
                .handler
                .on_viewport_property_command_retired(retirement)
                != NativeHostWake::Wait
            {
                // Forget callbacks have no Context. Both repaint variants preserve a root
                // wake through the existing event-loop latch without retaining that Context.
                self.root_wake_after_current.store(true, Ordering::Release);
            }
        }
    }

    fn invalidate_presented_viewport(&self, viewport_id: ViewportId) {
        self.presented_outputs
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&viewport_id);
        self.renderer_outputs
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&viewport_id);
        self.bump_presentation_generation(viewport_id);
    }

    fn bump_presentation_generation(&self, viewport_id: ViewportId) {
        let mut generations = self
            .presentation_generations
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let next_generation = generations
            .get(&viewport_id)
            .copied()
            .unwrap_or(0)
            .saturating_add(1);
        generations.insert(viewport_id, next_generation);
    }
}

fn record_newer_retention(
    outputs: &Mutex<BTreeMap<ViewportId, NativeRetentionRecord>>,
    viewport_id: ViewportId,
    ordinal: NativeOutputOrdinal,
    frame: PresentedNativeOutputFrame,
    kind: NativeRetentionKind,
) -> bool {
    let mut outputs = outputs.lock().unwrap_or_else(PoisonError::into_inner);
    if outputs
        .get(&viewport_id)
        .is_some_and(|current| current.ordinal >= ordinal)
    {
        return false;
    }
    outputs.insert(
        viewport_id,
        NativeRetentionRecord {
            ordinal,
            frame,
            kind,
        },
    );
    true
}

fn notify_output(
    inner: &NativeHostStateInner,
    ctx: &egui::Context,
    result: NativeOutputResult,
    frame: PresentedNativeOutputFrame,
    renderer_output: Option<NativeRetentionKind>,
) {
    let submitted_no_frame = match renderer_output {
        Some(kind) => {
            let recorded =
                inner.record_renderer_output(result.token.viewport_id, result.ordinal, frame, kind);
            recorded && kind == NativeRetentionKind::NoFrame
        }
        None => false,
    };
    inner.record_output_result(result, frame, submitted_no_frame);
    apply_host_wake(inner, ctx, inner.handler.on_output(result));
}

fn apply_host_wake(inner: &NativeHostStateInner, ctx: &egui::Context, wake: NativeHostWake) {
    match wake {
        NativeHostWake::Wait => {}
        NativeHostWake::RepaintRoot => ctx.request_repaint_once_of(ViewportId::ROOT),
        NativeHostWake::RepaintRootAfterCurrent => {
            inner.root_wake_after_current.store(true, Ordering::Release);
        }
    }
}

fn observe_native_pointer(
    event_loop: Option<&ActiveEventLoop>,
    expected_window_id: WindowId,
    device_id: DeviceId,
) -> NativeViewportPointerObservation {
    let Some(event_loop) = event_loop else {
        return NativeViewportPointerObservation::Unavailable;
    };
    #[cfg(all(target_os = "linux", feature = "x11"))]
    {
        match event_loop.observe_pointer(expected_window_id, device_id) {
            X11PointerObservation::Observed { facts, delivery } => {
                NativeViewportPointerObservation::Observed { facts, delivery }
            }
            X11PointerObservation::Unavailable => NativeViewportPointerObservation::Unavailable,
            X11PointerObservation::Unsupported => NativeViewportPointerObservation::Unsupported,
        }
    }
    #[cfg(not(all(target_os = "linux", feature = "x11")))]
    {
        let _ = (event_loop, expected_window_id, device_id);
        NativeViewportPointerObservation::Unsupported
    }
}

fn next_non_zero(counter: &AtomicU64, exhausted: &str) -> NonZeroU64 {
    let value = counter
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
            current.checked_add(1)
        })
        .unwrap_or_else(|_| panic!("{exhausted}"));
    NonZeroU64::new(value).unwrap_or_else(|| panic!("{exhausted}"))
}

#[cfg(test)]
mod pointer_observation_tests {
    use super::*;

    #[derive(Default)]
    struct ObservationHost {
        results: egui::mutex::Mutex<Vec<NativeViewportPointerObservationResult>>,
        retirements: egui::mutex::Mutex<Vec<NativeViewportPropertyCommandRetirement>>,
        requeue: egui::mutex::Mutex<Option<(egui::Context, ViewportId, WindowId, DeviceId)>>,
        successor: egui::mutex::Mutex<Option<NativeViewportPointerObservationCommandToken>>,
    }

    impl NativeHostHandler for ObservationHost {
        fn on_viewport_pointer_observation(
            &self,
            result: NativeViewportPointerObservationResult,
        ) -> NativeHostWake {
            self.results.lock().push(result);
            NativeHostWake::RepaintRootAfterCurrent
        }

        fn on_viewport_property_command_retired(
            &self,
            retirement: NativeViewportPropertyCommandRetirement,
        ) -> NativeHostWake {
            self.retirements.lock().push(retirement);
            if let Some((ctx, viewport, window, device)) = self.requeue.lock().take() {
                *self.successor.lock() = Some(queue_native_viewport_pointer_observation(
                    &ctx, viewport, window, device,
                ));
            }
            NativeHostWake::RepaintRootAfterCurrent
        }
    }

    fn fixture() -> (Arc<ObservationHost>, NativeHostState, egui::Context) {
        let host = Arc::new(ObservationHost::default());
        let handler: Arc<dyn NativeHostHandler> = Arc::<ObservationHost>::clone(&host);
        let state = NativeHostState::new(Some(handler));
        let ctx = egui::Context::default();
        state.bind_close_context(&ctx);
        (host, state, ctx)
    }

    // Only native sampling is stubbed. Queue consumption, exact-window gating, callbacks and
    // retirement use the same production boundary as both renderers.
    fn dispatch(
        state: &NativeHostState,
        ctx: &egui::Context,
        viewport: ViewportId,
        actual_window: WindowId,
        mut observe: impl FnMut(WindowId, DeviceId) -> NativeViewportPointerObservation,
    ) {
        state.process_viewport_commands_with(
            ctx,
            viewport,
            [],
            |_| panic!("no ordinary command was queued"),
            |command| {
                let QueuedNativeViewportPropertyCommand::PointerObservation {
                    token,
                    expected_window_id,
                    device_id,
                } = command
                else {
                    panic!("only pointer observations may execute in this fixture");
                };
                NativeViewportPropertyResult::PointerObservation(
                    NativeViewportPointerObservationResult::observe_with(
                        token,
                        viewport,
                        expected_window_id,
                        device_id,
                        actual_window,
                        &mut observe,
                    ),
                )
            },
        );
    }

    #[test]
    fn pointer_observation_preparation_reserves_identity_before_enqueue() {
        let (host, state, ctx) = fixture();
        let viewport = ViewportId::ROOT;
        let window = WindowId::from(141);
        let device = DeviceId::dummy();
        let unattached = queue_native_viewport_pointer_observation(
            &egui::Context::default(),
            viewport,
            window,
            device,
        );
        let prepared = prepare_native_viewport_pointer_observation(viewport, window, device);
        let token = prepared.token();
        let rejected = prepare_native_viewport_pointer_observation(viewport, window, device);
        assert_ne!(token, unattached);
        assert_ne!(rejected.token(), token);
        drop(rejected);
        assert!(state.take_viewport_property_commands(viewport).is_empty());
        assert!(host.results.lock().is_empty());
        assert!(host.retirements.lock().is_empty());

        prepared.enqueue(&ctx);
        dispatch(
            &state,
            &ctx,
            viewport,
            window,
            |expected, requested_device| {
                assert_eq!((expected, requested_device), (window, device));
                assert!(host.results.lock().is_empty());
                NativeViewportPointerObservation::Unsupported
            },
        );
        assert_eq!(host.results.lock()[0].token(), token);
        assert!(host.retirements.lock().is_empty());
    }

    #[test]
    fn pointer_observation_queue_preserves_same_observation_and_viewport_order() {
        let (host, state, ctx) = fixture();
        let viewport = ViewportId::ROOT;
        let other = ViewportId::from_hash_of("other-pointer-observation");
        let window = WindowId::from(141);
        let other_window = WindowId::from(142);
        let device = DeviceId::dummy();
        let observations = [
            NativeViewportPointerObservation::Observed {
                facts: PointerEventFacts {
                    surface_position: Some(winit::dpi::PhysicalPosition::new(10.25, 20.5)),
                    geometry: Some(winit::event::PointerEventGeometry {
                        surface_size: winit::dpi::PhysicalSize::new(801, 603),
                        scale_factor: 1.25,
                    }),
                    desktop_position: Some(winit::dpi::PhysicalPosition::new(-200.75, 300.5)),
                    modifiers: Some(winit::keyboard::ModifiersState::SHIFT),
                    hover: PointerWindowRoute::Window(other_window),
                    capture: PointerWindowRoute::Window(window),
                },
                delivery: PointerWindowRoute::Window(window),
            },
            NativeViewportPointerObservation::Unavailable,
            NativeViewportPointerObservation::Unsupported,
            NativeViewportPointerObservation::Observed {
                facts: PointerEventFacts::default(),
                delivery: PointerWindowRoute::Foreign,
            },
        ];
        let tokens = observations
            .map(|_| queue_native_viewport_pointer_observation(&ctx, viewport, window, device));
        let other_token =
            queue_native_viewport_pointer_observation(&ctx, other, other_window, device);
        let mut sampled = 0;
        dispatch(
            &state,
            &ctx,
            viewport,
            window,
            |expected, requested_device| {
                assert_eq!((expected, requested_device), (window, device));
                assert_eq!(host.results.lock().len(), sampled);
                let observation = observations[sampled];
                sampled += 1;
                observation
            },
        );
        assert_eq!(sampled, observations.len());
        {
            let results = host.results.lock();
            assert_eq!(results.len(), observations.len());
            for ((result, token), observation) in results.iter().zip(tokens).zip(observations) {
                assert_eq!(result.token(), token);
                assert_eq!(result.viewport_id(), viewport);
                assert_eq!(result.expected_window_id(), window);
                assert_eq!(result.device_id(), device);
                assert_eq!(result.observation(), observation);
            }
        }
        state.forget_presented_viewport(viewport);
        assert!(host.retirements.lock().is_empty());
        dispatch(
            &state,
            &ctx,
            other,
            other_window,
            |expected, requested_device| {
                assert_eq!((expected, requested_device), (other_window, device));
                NativeViewportPointerObservation::Unavailable
            },
        );
        assert_eq!(host.results.lock().last().unwrap().token(), other_token);
    }

    #[test]
    fn pointer_observation_keeps_unknown_capture_and_delivery_with_known_hover() {
        let (host, state, ctx) = fixture();
        let viewport = ViewportId::ROOT;
        let window = WindowId::from(141);
        let device = DeviceId::dummy();
        let facts = PointerEventFacts {
            surface_position: Some(winit::dpi::PhysicalPosition::new(10.25, 20.5)),
            desktop_position: Some(winit::dpi::PhysicalPosition::new(110.25, 220.5)),
            hover: PointerWindowRoute::Window(window),
            capture: PointerWindowRoute::Unknown,
            ..Default::default()
        };
        let observation = NativeViewportPointerObservation::Observed {
            facts,
            delivery: PointerWindowRoute::Unknown,
        };
        let token = queue_native_viewport_pointer_observation(&ctx, viewport, window, device);

        dispatch(
            &state,
            &ctx,
            viewport,
            window,
            |expected, requested_device| {
                assert_eq!((expected, requested_device), (window, device));
                observation
            },
        );

        let results = host.results.lock();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].token(), token);
        assert_eq!(results[0].expected_window_id(), window);
        assert_eq!(results[0].device_id(), device);
        assert_eq!(results[0].observation(), observation);
        let NativeViewportPointerObservation::Observed {
            facts: observed,
            delivery,
        } = results[0].observation()
        else {
            panic!("partial pointer facts must remain an observed result");
        };
        assert_eq!(observed, facts);
        assert_eq!(observed.capture, PointerWindowRoute::Unknown);
        assert_eq!(delivery, PointerWindowRoute::Unknown);
        assert_eq!(observed.geometry, None);
    }

    #[test]
    fn pointer_observation_never_queries_a_replacement_window() {
        let (host, state, ctx) = fixture();
        let viewport = ViewportId::ROOT;
        let original = WindowId::from(141);
        let replacement = WindowId::from(142);
        let device = DeviceId::dummy();
        let inner = state.inner.as_ref().unwrap();
        inner.capture_window_input_state(viewport, original);
        inner.capture_window_input_state(viewport, replacement);
        let token = queue_native_viewport_pointer_observation(&ctx, viewport, original, device);

        state.forget_presented_window(viewport, original);
        assert!(host.retirements.lock().is_empty());
        dispatch(&state, &ctx, viewport, replacement, |_, _| {
            panic!("the successor must not be queried for an older expected window")
        });
        let results = host.results.lock();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].token(), token);
        assert_eq!(results[0].expected_window_id(), original);
        assert_eq!(results[0].device_id(), device);
        assert_eq!(
            results[0].observation(),
            NativeViewportPointerObservation::Unavailable
        );
    }

    #[test]
    fn pointer_observation_without_event_loop_finishes_unavailable_once() {
        let (host, state, ctx) = fixture();
        let viewport = ViewportId::ROOT;
        let window = WindowId::from(141);
        let token =
            queue_native_viewport_pointer_observation(&ctx, viewport, window, DeviceId::dummy());
        dispatch(&state, &ctx, viewport, window, |expected, device| {
            observe_native_pointer(None, expected, device)
        });
        dispatch(&state, &ctx, viewport, window, |_, _| {
            panic!("an unavailable observation must not retry itself")
        });
        state.forget_presented_viewport(viewport);
        let results = host.results.lock();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].token(), token);
        assert_eq!(
            results[0].observation(),
            NativeViewportPointerObservation::Unavailable
        );
        assert!(host.retirements.lock().is_empty());
        assert!(state.take_root_wake_after_current());
        assert!(!state.take_root_wake_after_current());
    }

    #[test]
    fn pending_pointer_observations_retire_once_in_original_queue_order() {
        for forget_window in [false, true] {
            let (host, state, ctx) = fixture();
            let viewport = ViewportId::ROOT;
            let other = ViewportId::from_hash_of("surviving-pointer-observation");
            let window = WindowId::from(141);
            let device = DeviceId::dummy();
            state
                .inner
                .as_ref()
                .unwrap()
                .capture_window_input_state(viewport, window);
            let first = queue_native_viewport_pointer_observation(&ctx, viewport, window, device);
            let focus = queue_native_viewport_focus(&ctx, viewport);
            let second = queue_native_viewport_pointer_observation(&ctx, viewport, window, device);
            let surviving = queue_native_viewport_pointer_observation(&ctx, other, window, device);
            let pending = state.with_pending_viewport_property_targets(Default::default());
            assert!(pending.contains_key(&viewport));
            assert!(pending.contains_key(&other));
            assert!(host.results.lock().is_empty());

            if forget_window {
                state.forget_presented_window(viewport, window);
            } else {
                state.forget_presented_viewport(viewport);
            }
            let expected = [
                NativeViewportPropertyCommandRetirement::PointerObservation {
                    viewport_id: viewport,
                    token: first,
                    expected_window_id: window,
                    device_id: device,
                },
                NativeViewportPropertyCommandRetirement::Focus {
                    viewport_id: viewport,
                    token: focus,
                },
                NativeViewportPropertyCommandRetirement::PointerObservation {
                    viewport_id: viewport,
                    token: second,
                    expected_window_id: window,
                    device_id: device,
                },
            ];
            assert_eq!(*host.retirements.lock(), expected);
            state.forget_presented_window(viewport, window);
            state.forget_presented_viewport(viewport);
            dispatch(&state, &ctx, viewport, window, |_, _| {
                panic!("a retired observation must not query the backend")
            });
            assert_eq!(*host.retirements.lock(), expected);
            dispatch(&state, &ctx, other, window, |_, _| {
                NativeViewportPointerObservation::Unsupported
            });
            let results = host.results.lock();
            assert_eq!(results.len(), 1);
            assert_eq!(results[0].token(), surviving);
            assert_eq!(results[0].viewport_id(), other);
        }
    }

    #[test]
    fn pointer_observation_retirement_requeue_preserves_successor_and_root_wake() {
        let (host, state, ctx) = fixture();
        let viewport = ViewportId::ROOT;
        let window = WindowId::from(141);
        let device = DeviceId::dummy();
        let retired = queue_native_viewport_pointer_observation(&ctx, viewport, window, device);
        *host.requeue.lock() = Some((ctx.clone(), viewport, window, device));

        state.forget_presented_viewport(viewport);
        let successor = host.successor.lock().unwrap();
        assert_ne!(successor, retired);
        assert_eq!(host.retirements.lock().len(), 1);
        assert!(
            state
                .with_pending_viewport_property_targets(Default::default())
                .contains_key(&viewport)
        );
        assert!(state.take_root_wake_after_current());
        assert!(!state.take_root_wake_after_current());
        dispatch(&state, &ctx, viewport, window, |_, _| {
            NativeViewportPointerObservation::Unsupported
        });
        state.forget_presented_viewport(viewport);
        let results = host.results.lock();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].token(), successor);
        assert_eq!(host.retirements.lock().len(), 1);
    }
}

#[cfg(test)]
mod tests {
    use egui::mutex::Mutex;
    use std::sync::atomic::AtomicUsize;

    use super::*;

    #[derive(Debug, Clone, PartialEq)]
    struct RecordedNativeViewportRoster {
        records: Vec<NativeViewportRecord>,
        work_areas: Option<Vec<NativeWorkAreaRecord>>,
        backend: NativeWindowingBackend,
    }

    impl From<NativeViewportRoster<'_>> for RecordedNativeViewportRoster {
        fn from(roster: NativeViewportRoster<'_>) -> Self {
            let work_areas = match roster.work_areas() {
                NativeWorkAreaRoster::Exact(records) => Some(records.to_vec()),
                NativeWorkAreaRoster::Unknown => None,
            };
            Self {
                records: roster.records().to_vec(),
                work_areas,
                backend: roster.backend(),
            }
        }
    }

    #[derive(Default)]
    struct RecordingHost {
        events: Mutex<
            Vec<(
                u64,
                WindowId,
                Option<ViewportId>,
                winit::event::PointerEventFacts,
                Option<NativeWindowLocalGeometry>,
            )>,
        >,
        device_removals: Mutex<Vec<u64>>,
        output_begins: Mutex<
            Vec<(
                NativeOutputToken,
                NativeWindowSnapshot,
                Option<RecordedNativeViewportRoster>,
            )>,
        >,
        outputs: Mutex<Vec<NativeOutputResult>>,
        create_failures: Mutex<Vec<NativeViewportCreateFailure>>,
        visibility_results: Mutex<Vec<NativeViewportVisibilityResult>>,
        viewport_close_cancellations: Mutex<Vec<NativeViewportCloseRequest>>,
        global_focus_observations: Mutex<Vec<NativeGlobalFocusObservation>>,
        viewport_focus_results: Mutex<Vec<NativeViewportFocusResult>>,
        viewport_pointer_passthrough_results: Mutex<Vec<NativeViewportPointerPassthroughResult>>,
        create_attempts: Mutex<Vec<NativeViewportCreateAttempt>>,
        deferred_rect: Mutex<Option<(ViewportId, NativePhysicalRect)>>,
        deferred_viewport: Mutex<Option<ViewportId>>,
        deferred_output: Mutex<Option<ViewportId>>,
        hidden_viewport: Mutex<Option<ViewportId>>,
        wake: NativeHostWake,
    }

    #[derive(Default)]
    struct ExclusiveAttachmentHost {
        attachment: Mutex<Option<NativeHostAttachment>>,
        detachments: AtomicUsize,
    }

    impl NativeHostHandler for ExclusiveAttachmentHost {
        fn try_attach(&self, attachment: NativeHostAttachment) -> bool {
            let mut current = self.attachment.lock();
            if current.is_some() {
                return false;
            }
            *current = Some(attachment);
            true
        }

        fn detach(&self, attachment: NativeHostAttachment) {
            let mut current = self.attachment.lock();
            if *current != Some(attachment) {
                return;
            }
            *current = None;
            self.detachments
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
    }

    impl NativeHostHandler for RecordingHost {
        fn begin_deferred_viewport_create(
            &self,
            attempt: NativeViewportCreateAttempt,
        ) -> NativeViewportCreateAdmission {
            self.create_attempts.lock().push(attempt);
            if *self.deferred_viewport.lock() == Some(attempt.viewport_id()) {
                return NativeViewportCreateAdmission::Defer;
            }
            NativeViewportCreateAdmission::Proceed {
                undecorated_outer_rect: self
                    .deferred_rect
                    .lock()
                    .filter(|(requested_viewport, _)| *requested_viewport == attempt.viewport_id())
                    .map(|(_, rect)| rect),
            }
        }

        fn render_hidden_deferred_viewport(&self, viewport_id: ViewportId) -> bool {
            *self.hidden_viewport.lock() == Some(viewport_id)
        }

        fn on_window_event(&self, event: NativeWindowEvent<'_>) -> NativeWindowEventDisposition {
            let facts = match event.event() {
                winit::event::WindowEvent::MouseInput { facts, .. }
                | winit::event::WindowEvent::MouseWheel { facts, .. } => *facts,
                winit::event::WindowEvent::CloseRequested => {
                    return NativeWindowEventDisposition::Continue;
                }
                other => panic!("expected pointer event, got {other:?}"),
            };
            self.events.lock().push((
                event.ordinal().get(),
                event.window_id(),
                event.viewport_id(),
                facts,
                event.local_geometry(),
            ));
            NativeWindowEventDisposition::Continue
        }

        fn on_device_removed(&self, removal: NativeDeviceRemoval) {
            self.device_removals.lock().push(removal.ordinal().get());
        }

        fn on_global_focus(&self, observation: NativeGlobalFocusObservation) -> NativeHostWake {
            self.global_focus_observations.lock().push(observation);
            self.wake
        }

        fn on_viewport_focus(&self, result: NativeViewportFocusResult) -> NativeHostWake {
            self.viewport_focus_results.lock().push(result);
            self.wake
        }

        fn on_viewport_pointer_passthrough(
            &self,
            result: NativeViewportPointerPassthroughResult,
        ) -> NativeHostWake {
            self.viewport_pointer_passthrough_results
                .lock()
                .push(result);
            self.wake
        }

        fn on_output(&self, result: NativeOutputResult) -> NativeHostWake {
            self.outputs.lock().push(result);
            self.wake
        }

        fn on_output_begin(
            &self,
            token: NativeOutputToken,
            window: NativeWindowSnapshot,
            root_roster: Option<NativeViewportRoster<'_>>,
        ) -> NativeOutputAdmission {
            if *self.deferred_output.lock() == Some(token.viewport_id()) {
                return NativeOutputAdmission::Defer;
            }
            self.output_begins.lock().push((
                token,
                window,
                root_roster.map(RecordedNativeViewportRoster::from),
            ));
            NativeOutputAdmission::Proceed
        }

        fn on_viewport_create_failed(
            &self,
            failure: NativeViewportCreateFailure,
        ) -> NativeHostWake {
            self.create_failures.lock().push(failure);
            self.wake
        }

        fn on_viewport_visibility(&self, result: NativeViewportVisibilityResult) -> NativeHostWake {
            self.visibility_results.lock().push(result);
            self.wake
        }

        fn on_viewport_close_cancelled(
            &self,
            request: NativeViewportCloseRequest,
        ) -> NativeHostWake {
            self.viewport_close_cancellations.lock().push(request);
            self.wake
        }
    }

    #[test]
    fn local_geometry_preserves_native_precision_and_egui_scale_rounding() {
        for (native_scale, zoom) in [(1.2, 1.1_f32), (1.000_000_000_1, 0.9), (2.0, 1.25)] {
            let geometry =
                NativeWindowLocalGeometry::from_window_facts([801, 603], native_scale, zoom);
            assert_eq!(geometry.content_size(), [801, 603]);
            assert_eq!(geometry.native_scale_factor(), native_scale);
            assert_eq!(
                geometry.presentation_scale_factor(),
                f64::from(zoom * native_scale as f32)
            );
        }
        let geometry = NativeWindowLocalGeometry::from_window_facts([800, 600], 1.2, 1.1);
        assert_ne!(
            geometry.presentation_scale_factor(),
            f64::from(1.1_f32) * 1.2,
            "presentation scale must round as f32 before widening"
        );
    }

    #[test]
    fn snapshot_keeps_content_size_without_inventing_desktop_position() {
        let geometry = NativeWindowLocalGeometry::from_window_facts([801, 603], 1.2, 1.1);
        let snapshot = NativeWindowSnapshot::from_captured_geometry(
            geometry,
            None,
            None,
            Some(true),
            None,
            NativeWindowInputState::ReceivesInput,
        );
        assert_eq!(snapshot.content_size(), [801, 603]);
        assert_eq!(snapshot.inner_rect(), None);
        assert_eq!(snapshot.outer_rect(), None);
        assert_eq!(
            snapshot.native_scale_factor(),
            geometry.native_scale_factor()
        );
        assert_eq!(
            snapshot.presentation_scale_factor(),
            geometry.presentation_scale_factor()
        );

        let positioned = NativeWindowSnapshot::from_captured_geometry(
            geometry,
            Some(winit::dpi::PhysicalPosition::new(-120, 45)),
            None,
            Some(true),
            None,
            NativeWindowInputState::ReceivesInput,
        );
        assert_eq!(positioned.content_size(), [801, 603]);
        assert_eq!(
            positioned.inner_rect(),
            Some(NativePhysicalRect::new(-120, 45, 801, 603))
        );
    }

    #[test]
    fn callback_preserves_optional_local_geometry_separately_from_event_facts() {
        use winit::event::{DeviceId, ElementState, MouseButton, PointerEventFacts, WindowEvent};

        let host = Arc::new(RecordingHost::default());
        let handler: Arc<dyn NativeHostHandler> = host.clone();
        let state = NativeHostState::new(Some(handler));
        let mut sequence = NativeEventSequencer::default();
        let window = WindowId::from(29);
        let facts = PointerEventFacts {
            surface_position: Some(winit::dpi::PhysicalPosition::new(11.0, 12.0)),
            ..PointerEventFacts::default()
        };
        let event = WindowEvent::MouseInput {
            device_id: DeviceId::dummy(),
            state: ElementState::Released,
            button: MouseButton::Left,
            facts,
        };
        let geometry = NativeWindowLocalGeometry::from_window_facts([801, 603], 1.2, 1.1);
        for local_geometry in [Some(geometry), None] {
            state.observe_window_event(
                None,
                sequence.next(),
                window,
                Some(ViewportId::ROOT),
                local_geometry,
                &event,
            );
        }
        assert_eq!(
            *host.events.lock(),
            vec![
                (1, window, Some(ViewportId::ROOT), facts, Some(geometry)),
                (2, window, Some(ViewportId::ROOT), facts, None),
            ]
        );
    }

    #[test]
    fn device_removal_callback_preserves_global_native_event_order() {
        use winit::event::{DeviceId, ElementState, MouseButton, PointerEventFacts};

        let host = Arc::new(RecordingHost::default());
        let handler: Arc<dyn NativeHostHandler> = Arc::<RecordingHost>::clone(&host);
        let state = NativeHostState::new(Some(handler));
        let mut sequencer = NativeEventSequencer::default();
        let window = WindowId::from(31);
        let device = DeviceId::dummy();
        let press = winit::event::WindowEvent::MouseInput {
            device_id: device,
            state: ElementState::Pressed,
            button: MouseButton::Left,
            facts: PointerEventFacts::default(),
        };
        let release = winit::event::WindowEvent::MouseInput {
            device_id: device,
            state: ElementState::Released,
            button: MouseButton::Left,
            facts: PointerEventFacts::default(),
        };

        state.observe_window_event(
            None,
            sequencer.next(),
            window,
            Some(ViewportId::ROOT),
            None,
            &press,
        );
        state.observe_device_removal(None, sequencer.next());
        state.observe_window_event(
            None,
            sequencer.next(),
            window,
            Some(ViewportId::ROOT),
            None,
            &release,
        );

        assert_eq!(
            host.events
                .lock()
                .iter()
                .map(|(ordinal, ..)| *ordinal)
                .collect::<Vec<_>>(),
            vec![1, 3]
        );
        assert_eq!(
            *host.device_removals.lock(),
            vec![2],
            "the typed removal keeps exact order without claiming raw/window device correlation"
        );
    }

    #[test]
    fn native_host_attachment_is_exclusive_and_released_on_drop() {
        let host = Arc::new(ExclusiveAttachmentHost::default());
        let first_handler: Arc<dyn NativeHostHandler> =
            Arc::<ExclusiveAttachmentHost>::clone(&host);
        let first = NativeHostState::new(Some(first_handler));

        let second_handler: Arc<dyn NativeHostHandler> =
            Arc::<ExclusiveAttachmentHost>::clone(&host);
        let rejected = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = NativeHostState::new(Some(second_handler));
        }));
        assert!(
            rejected.is_err(),
            "a second live attachment must be rejected"
        );
        assert_eq!(host.detachments.load(Ordering::Relaxed), 0);

        drop(first);
        assert_eq!(host.detachments.load(Ordering::Relaxed), 1);

        let third_handler: Arc<dyn NativeHostHandler> =
            Arc::<ExclusiveAttachmentHost>::clone(&host);
        let third = NativeHostState::new(Some(third_handler));
        drop(third);
        assert_eq!(host.detachments.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn viewport_close_callback_reports_only_the_exact_cancelled_request() {
        use winit::event::WindowEvent;

        let host = Arc::new(RecordingHost::default());
        let handler: Arc<dyn NativeHostHandler> = Arc::<RecordingHost>::clone(&host);
        let state = NativeHostState::new(Some(handler));
        let ctx = egui::Context::default();
        let mut sequencer = NativeEventSequencer::default();
        let root_window = WindowId::from(11);
        let child_viewport = ViewportId::from_hash_of("child-close");
        let child_window = WindowId::from(12);

        state.observe_window_event(
            None,
            sequencer.next(),
            root_window,
            Some(ViewportId::ROOT),
            None,
            &WindowEvent::CloseRequested,
        );
        state.finish_viewport_close_request(&ctx, ViewportId::ROOT, root_window, false);
        assert!(host.viewport_close_cancellations.lock().is_empty());

        state.observe_window_event(
            None,
            sequencer.next(),
            child_window,
            Some(child_viewport),
            None,
            &WindowEvent::CloseRequested,
        );
        state.finish_viewport_close_request(&ctx, child_viewport, child_window, true);

        let requests = host.viewport_close_cancellations.lock();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].event().get(), 2);
        assert_eq!(requests[0].viewport_id(), child_viewport);
        assert_eq!(requests[0].window_id(), child_window);
    }

    #[test]
    fn global_focus_callback_preserves_event_and_exact_window() {
        let host = Arc::new(RecordingHost::default());
        let handler: Arc<dyn NativeHostHandler> = Arc::<RecordingHost>::clone(&host);
        let state = NativeHostState::new(Some(handler));
        let child_viewport = ViewportId::from_hash_of("focused-child");
        let child_window = WindowId::from(17);
        let ctx = egui::Context::default();

        state.observe_global_focus(
            Some(&ctx),
            NativeEventOrdinal(NonZeroU64::new(4).expect("test ordinal is non-zero")),
            Some((child_viewport, child_window)),
        );
        state.observe_global_focus(
            Some(&ctx),
            NativeEventOrdinal(NonZeroU64::new(5).expect("test ordinal is non-zero")),
            None,
        );

        let observations = host.global_focus_observations.lock();
        assert_eq!(observations.len(), 2);
        assert_eq!(observations[0].event().get(), 4);
        assert_eq!(
            observations[0].focused(),
            NativeGlobalFocus::Viewport {
                viewport_id: child_viewport,
                window_id: child_window,
            }
        );
        assert_eq!(observations[1].event().get(), 5);
        assert_eq!(observations[1].focused(), NativeGlobalFocus::Unknown);
    }

    #[test]
    fn viewport_focus_callback_reports_command_consumption() {
        let host = Arc::new(RecordingHost::default());
        let handler: Arc<dyn NativeHostHandler> = Arc::<RecordingHost>::clone(&host);
        let state = NativeHostState::new(Some(handler));
        let child_viewport = ViewportId::from_hash_of("focus-command-child");
        let child_window = WindowId::from(19);
        let context = egui::Context::default();
        state.bind_close_context(&context);
        let token = queue_native_viewport_focus(&context, child_viewport);

        state.notify_viewport_focus(
            &context,
            NativeViewportFocusResult {
                token,
                viewport_id: child_viewport,
                window_id: child_window,
                status: NativeViewportFocusStatus::AlreadyFocused,
            },
        );

        let results = host.viewport_focus_results.lock();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].token(), token);
        assert_eq!(results[0].viewport_id(), child_viewport);
        assert_eq!(results[0].window_id(), child_window);
        assert_eq!(
            results[0].status(),
            NativeViewportFocusStatus::AlreadyFocused
        );
    }

    #[test]
    fn viewport_pointer_passthrough_callback_reports_exact_command_result() {
        let host = Arc::new(RecordingHost::default());
        let handler: Arc<dyn NativeHostHandler> = Arc::<RecordingHost>::clone(&host);
        let state = NativeHostState::new(Some(handler));
        let context = egui::Context::default();
        state.bind_close_context(&context);
        let child_viewport = ViewportId::from_hash_of("pointer-passthrough-child");
        let child_window = WindowId::from(23);
        let token = queue_native_viewport_pointer_passthrough(&context, child_viewport, true);

        state.notify_viewport_pointer_passthrough(
            &context,
            NativeViewportPointerPassthroughResult {
                token,
                viewport_id: child_viewport,
                window_id: child_window,
                enabled: true,
                status: NativeViewportPointerPassthroughStatus::Applied,
            },
        );

        let results = host.viewport_pointer_passthrough_results.lock();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].token(), token);
        assert_eq!(results[0].viewport_id(), child_viewport);
        assert_eq!(results[0].window_id(), child_window);
        assert!(results[0].enabled());
        assert_eq!(
            results[0].status(),
            NativeViewportPointerPassthroughStatus::Applied
        );
    }

    #[test]
    fn pointer_passthrough_host_commands_keep_exact_tokens_and_viewport_order() {
        let context = egui::Context::default();
        let state = NativeHostState::new(Some(Arc::new(RecordingHost::default())));
        state.bind_close_context(&context);
        let first_viewport = ViewportId::from_hash_of("pointer-passthrough-first");
        let second_viewport = ViewportId::from_hash_of("pointer-passthrough-second");
        let first = queue_native_viewport_pointer_passthrough(&context, first_viewport, true);
        let other = queue_native_viewport_pointer_passthrough(&context, second_viewport, true);
        let second = queue_native_viewport_pointer_passthrough(&context, first_viewport, false);

        let first_commands = state.take_viewport_property_commands(first_viewport);
        assert_eq!(
            first_commands.into_iter().collect::<Vec<_>>(),
            [
                QueuedNativeViewportPropertyCommand::PointerPassthrough {
                    token: first,
                    enabled: true,
                },
                QueuedNativeViewportPropertyCommand::PointerPassthrough {
                    token: second,
                    enabled: false,
                },
            ]
        );
        assert_eq!(
            state
                .take_viewport_property_commands(second_viewport)
                .into_iter()
                .collect::<Vec<_>>(),
            [QueuedNativeViewportPropertyCommand::PointerPassthrough {
                token: other,
                enabled: true,
            }]
        );
        assert!(
            state
                .take_viewport_property_commands(first_viewport)
                .is_empty()
        );
    }

    #[test]
    fn first_window_incarnation_defaults_to_receives_input() {
        let mut ledger = NativeWindowInputLedger::default();
        let viewport = ViewportId::from_hash_of("initial-input-window");
        let window = WindowId::from(31);

        assert_eq!(
            ledger.capture(viewport, window),
            NativeWindowInputState::ReceivesInput
        );
        assert_eq!(
            ledger.capture(viewport, window),
            NativeWindowInputState::ReceivesInput
        );
    }

    #[test]
    fn applied_pointer_passthrough_command_updates_the_next_snapshot() {
        let mut ledger = NativeWindowInputLedger::default();
        let viewport = ViewportId::from_hash_of("input-command-window");
        let window = WindowId::from(32);

        assert_eq!(
            ledger.capture(viewport, window),
            NativeWindowInputState::ReceivesInput
        );
        assert!(ledger.record_applied(viewport, window, NativeWindowInputState::PassThrough));
        assert_eq!(
            ledger.capture(viewport, window),
            NativeWindowInputState::PassThrough
        );
        assert!(ledger.record_applied(viewport, window, NativeWindowInputState::ReceivesInput));
        assert_eq!(
            ledger.capture(viewport, window),
            NativeWindowInputState::ReceivesInput
        );
    }

    #[test]
    fn replacement_window_resets_input_and_rejects_stale_command_state() {
        let mut ledger = NativeWindowInputLedger::default();
        let viewport = ViewportId::from_hash_of("replacement-input-window");
        let first_window = WindowId::from(33);
        let replacement_window = WindowId::from(34);

        assert_eq!(
            ledger.capture(viewport, first_window),
            NativeWindowInputState::ReceivesInput
        );
        assert!(ledger.record_applied(viewport, first_window, NativeWindowInputState::PassThrough));
        assert_eq!(
            ledger.capture(viewport, replacement_window),
            NativeWindowInputState::ReceivesInput
        );
        assert!(!ledger.record_applied(
            viewport,
            first_window,
            NativeWindowInputState::PassThrough
        ));
        assert_eq!(
            ledger.capture(viewport, replacement_window),
            NativeWindowInputState::ReceivesInput
        );
        assert!(ledger.record_applied(
            viewport,
            replacement_window,
            NativeWindowInputState::PassThrough
        ));
        ledger.forget_window(viewport, replacement_window);
        assert_eq!(
            ledger.capture(viewport, replacement_window),
            NativeWindowInputState::ReceivesInput
        );
    }

    #[test]
    fn hidden_rendering_is_opt_in_and_never_applies_to_root() {
        let child = ViewportId::from_hash_of("hidden-staging");
        let host = Arc::new(RecordingHost {
            hidden_viewport: Mutex::new(Some(child)),
            ..Default::default()
        });
        let handler: Arc<dyn NativeHostHandler> = Arc::<RecordingHost>::clone(&host);
        let state = NativeHostState::new(Some(handler));

        assert!(state.render_hidden_deferred_viewport(child));
        assert!(!state.render_hidden_deferred_viewport(ViewportId::ROOT));
        assert!(!state.render_hidden_deferred_viewport(ViewportId::from_hash_of("other")));
        assert!(!NativeHostState::default().render_hidden_deferred_viewport(child));
    }

    #[test]
    fn event_ordinals_preserve_cross_window_facts() {
        use winit::dpi::PhysicalPosition;
        use winit::event::{
            DeviceId, ElementState, MouseButton, PointerEventFacts, PointerWindowRoute, WindowEvent,
        };
        use winit::keyboard::ModifiersState;

        let host = Arc::new(RecordingHost::default());
        let handler: Arc<dyn NativeHostHandler> = Arc::<RecordingHost>::clone(&host);
        let state = NativeHostState::new(Some(handler));
        let mut sequencer = NativeEventSequencer::default();
        let first_window = WindowId::from(11);
        let second_window = WindowId::from(22);
        let second_viewport = ViewportId::from_hash_of("second");
        let first_facts = PointerEventFacts {
            surface_position: Some(PhysicalPosition::new(10.0, 20.0)),
            desktop_position: Some(PhysicalPosition::new(110.0, 220.0)),
            modifiers: Some(ModifiersState::SHIFT),
            hover: PointerWindowRoute::Window(first_window),
            capture: PointerWindowRoute::Unknown,
            ..PointerEventFacts::default()
        };
        let second_facts = PointerEventFacts {
            surface_position: Some(PhysicalPosition::new(30.0, 40.0)),
            desktop_position: Some(PhysicalPosition::new(330.0, 440.0)),
            modifiers: None,
            hover: PointerWindowRoute::Foreign,
            capture: PointerWindowRoute::Window(second_window),
            ..PointerEventFacts::default()
        };

        let release = WindowEvent::MouseInput {
            device_id: DeviceId::dummy(),
            state: ElementState::Released,
            button: MouseButton::Left,
            facts: first_facts,
        };
        state.observe_window_event(None, sequencer.next(), first_window, None, None, &release);

        let press = WindowEvent::MouseInput {
            device_id: DeviceId::dummy(),
            state: ElementState::Pressed,
            button: MouseButton::Left,
            facts: second_facts,
        };
        state.observe_window_event(
            None,
            sequencer.next(),
            second_window,
            Some(second_viewport),
            None,
            &press,
        );

        assert_eq!(
            *host.events.lock(),
            vec![
                (1, first_window, None, first_facts, None),
                (2, second_window, Some(second_viewport), second_facts, None),
            ]
        );
    }

    #[test]
    fn secondary_window_event_requests_root_repaint() {
        use winit::event::{DeviceId, ElementState, MouseButton, PointerEventFacts, WindowEvent};

        let host = Arc::new(RecordingHost::default());
        let handler: Arc<dyn NativeHostHandler> = Arc::<RecordingHost>::clone(&host);
        let state = NativeHostState::new(Some(handler));
        let ctx = egui::Context::default();
        let repaint_count = Arc::new(AtomicUsize::new(0));
        ctx.set_request_repaint_callback({
            let repaint_count = Arc::clone(&repaint_count);
            move |request| {
                assert_eq!(request.viewport_id, ViewportId::ROOT);
                repaint_count.fetch_add(1, Ordering::Relaxed);
            }
        });
        let mut sequencer = NativeEventSequencer::default();
        let child_window = WindowId::from(22);
        let child_viewport = ViewportId::from_hash_of("secondary-wake");
        let facts = PointerEventFacts::default();
        let event = WindowEvent::MouseInput {
            device_id: DeviceId::dummy(),
            state: ElementState::Pressed,
            button: MouseButton::Left,
            facts,
        };

        state.observe_window_event(
            Some(&ctx),
            sequencer.next(),
            child_window,
            Some(child_viewport),
            None,
            &event,
        );

        assert_eq!(repaint_count.load(Ordering::Relaxed), 1);
        assert_eq!(
            *host.events.lock(),
            vec![(1, child_window, Some(child_viewport), facts, None)]
        );
    }

    #[test]
    fn event_ordinals_preserve_cross_window_wheel_facts() {
        use winit::dpi::PhysicalPosition;
        use winit::event::{
            DeviceId, MouseScrollDelta, PointerEventFacts, PointerWindowRoute, TouchPhase,
            WindowEvent,
        };
        use winit::keyboard::ModifiersState;

        let host = Arc::new(RecordingHost::default());
        let handler: Arc<dyn NativeHostHandler> = Arc::<RecordingHost>::clone(&host);
        let state = NativeHostState::new(Some(handler));
        let mut sequencer = NativeEventSequencer::default();
        let first_window = WindowId::from(31);
        let second_window = WindowId::from(32);
        let second_viewport = ViewportId::from_hash_of("second-wheel");
        let first_facts = PointerEventFacts {
            surface_position: Some(PhysicalPosition::new(4.0, 5.0)),
            desktop_position: Some(PhysicalPosition::new(104.0, 205.0)),
            modifiers: Some(ModifiersState::CONTROL),
            hover: PointerWindowRoute::Window(second_window),
            capture: PointerWindowRoute::Window(first_window),
            ..PointerEventFacts::default()
        };
        let second_facts = PointerEventFacts {
            surface_position: Some(PhysicalPosition::new(6.0, 7.0)),
            desktop_position: Some(PhysicalPosition::new(306.0, 407.0)),
            modifiers: None,
            hover: PointerWindowRoute::Foreign,
            capture: PointerWindowRoute::Unknown,
            ..PointerEventFacts::default()
        };

        let first = WindowEvent::MouseWheel {
            device_id: DeviceId::dummy(),
            delta: MouseScrollDelta::LineDelta(1.0, -2.0),
            phase: TouchPhase::Moved,
            facts: first_facts,
        };
        state.observe_window_event(None, sequencer.next(), first_window, None, None, &first);

        let second = WindowEvent::MouseWheel {
            device_id: DeviceId::dummy(),
            delta: MouseScrollDelta::PixelDelta(PhysicalPosition::new(3.0, 4.0)),
            phase: TouchPhase::Ended,
            facts: second_facts,
        };
        state.observe_window_event(
            None,
            sequencer.next(),
            second_window,
            Some(second_viewport),
            None,
            &second,
        );

        assert_eq!(
            *host.events.lock(),
            vec![
                (1, first_window, None, first_facts, None),
                (2, second_window, Some(second_viewport), second_facts, None),
            ]
        );
    }

    #[test]
    fn nested_outputs_use_completion_order_and_terminal_drop() {
        let host = Arc::new(RecordingHost {
            wake: NativeHostWake::RepaintRoot,
            ..Default::default()
        });
        let handler: Arc<dyn NativeHostHandler> = Arc::<RecordingHost>::clone(&host);
        let state = NativeHostState::new(Some(handler));
        let ctx = egui::Context::default();
        let repaint_count = Arc::new(AtomicUsize::new(0));
        ctx.set_request_repaint_callback({
            let repaint_count = Arc::clone(&repaint_count);
            move |_| {
                repaint_count.fetch_add(1, Ordering::Relaxed);
            }
        });

        let outer = state
            .begin_output_for_test(&ctx, ViewportId::ROOT, WindowId::from(11))
            .unwrap();
        let outer_token = current_native_output_token().unwrap();
        let child_id = ViewportId::from_hash_of("child");
        let child = state
            .begin_output_for_test(&ctx, child_id, WindowId::from(22))
            .unwrap();
        let child_token = current_native_output_token().unwrap();

        child.finish().present();
        assert_eq!(current_native_output_token(), Some(outer_token));
        drop(outer.finish());
        assert_eq!(current_native_output_token(), None);

        let outputs = host.outputs.lock();
        let output_begins = host.output_begins.lock();
        assert_eq!(
            output_begins
                .iter()
                .map(|(token, _, _)| *token)
                .collect::<Vec<_>>(),
            vec![outer_token, child_token]
        );
        assert!(output_begins.iter().all(|(_, snapshot, roster)| {
            roster.is_none()
                && snapshot.inner_rect().is_none()
                && snapshot.outer_rect().is_none()
                && snapshot.native_scale_factor() == 1.0
                && snapshot.presentation_scale_factor() == 1.0
                && snapshot.visible().is_none()
                && snapshot.minimized().is_none()
                && snapshot.input_state() == NativeWindowInputState::ReceivesInput
        }));
        assert_eq!(outputs.len(), 2);
        assert_eq!(outputs[0].token(), child_token);
        assert_eq!(outputs[0].ordinal().get(), 1);
        assert_eq!(outputs[0].status(), NativeOutputStatus::Presented);
        assert_eq!(outputs[1].token(), outer_token);
        assert_eq!(outputs[1].ordinal().get(), 2);
        assert_eq!(outputs[1].status(), NativeOutputStatus::NotPresented);
        assert_eq!(repaint_count.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn deferred_nested_outputs_preserve_the_outer_token_and_contiguous_retry_completion() {
        let host = Arc::new(RecordingHost::default());
        let handler: Arc<dyn NativeHostHandler> = host.clone();
        let state = NativeHostState::new(Some(handler));
        let ctx = egui::Context::default();
        let root_window = WindowId::from(11);
        let child = ViewportId::from_hash_of("deferred-output-child");
        let child_window = WindowId::from(22);
        let outer = state
            .begin_output_for_test(&ctx, ViewportId::ROOT, root_window)
            .expect("the outer output is admitted");
        let outer_token = current_native_output_token().unwrap();

        for (viewport, window) in [(child, child_window), (ViewportId::ROOT, root_window)] {
            *host.deferred_output.lock() = Some(viewport);
            assert!(
                state
                    .begin_output_for_test(&ctx, viewport, window)
                    .is_none()
            );
            assert_eq!(current_native_output_token(), Some(outer_token));
            assert_eq!(host.output_begins.lock().len(), 1);
            assert!(host.outputs.lock().is_empty());
            assert_eq!(
                state
                    .inner
                    .as_ref()
                    .unwrap()
                    .next_output
                    .load(Ordering::Relaxed),
                1,
                "deferred candidates cannot consume a completion ordinal",
            );
            assert!(state.take_root_wake_after_current());
            assert!(!state.take_root_wake_after_current());
        }

        *host.deferred_output.lock() = None;
        let retried_child = state
            .begin_output_for_test(&ctx, child, child_window)
            .expect("the child retry is admitted");
        let child_token = current_native_output_token().unwrap();
        retried_child.finish().present();
        assert_eq!(current_native_output_token(), Some(outer_token));
        drop(outer);
        assert_eq!(current_native_output_token(), None);

        let next_root = state
            .begin_output_for_test(&ctx, ViewportId::ROOT, root_window)
            .expect("ordinary root output resumes");
        let next_token = current_native_output_token().unwrap();
        next_root.finish().present();

        assert_eq!(
            host.output_begins
                .lock()
                .iter()
                .map(|(token, _, _)| *token)
                .collect::<Vec<_>>(),
            [outer_token, child_token, next_token],
        );
        assert_eq!(
            host.outputs
                .lock()
                .iter()
                .map(|result| (result.token(), result.ordinal().get(), result.status()))
                .collect::<Vec<_>>(),
            [
                (child_token, 1, NativeOutputStatus::Presented),
                (outer_token, 2, NativeOutputStatus::NotPresented),
                (next_token, 3, NativeOutputStatus::Presented),
            ],
        );
        assert_eq!(current_native_output_token(), None);
    }

    #[test]
    fn deferred_child_rearms_a_consumed_wake_without_starting_a_pass() {
        let host = Arc::new(RecordingHost::default());
        let child = ViewportId::from_hash_of("deferred-child-repaint");
        *host.deferred_output.lock() = Some(child);
        let handler: Arc<dyn NativeHostHandler> = host.clone();
        let state = NativeHostState::new(Some(handler));
        let ctx = egui::Context::default();
        let requests = Arc::new(Mutex::new(Vec::new()));
        ctx.set_request_repaint_callback({
            let requests = Arc::clone(&requests);
            move |info| requests.lock().push(info.viewport_id)
        });
        ctx.request_repaint_once_of(child);
        assert_eq!(*requests.lock(), [child]);
        assert!(
            state
                .begin_output_for_test(&ctx, child, WindowId::from(22))
                .is_none()
        );
        assert_eq!(ctx.cumulative_pass_nr_for(child), 0);
        assert!(host.outputs.lock().is_empty());
        assert_eq!(current_native_output_token(), None);
        ctx.request_repaint_once_of(child);
        assert_eq!(*requests.lock(), [child, child]);
        assert_eq!(
            state
                .inner
                .as_ref()
                .unwrap()
                .next_output
                .load(Ordering::Relaxed),
            1
        );
    }

    #[test]
    fn deferred_output_preserves_the_exact_retained_frame_until_retry() {
        let host = Arc::new(RecordingHost::default());
        let handler: Arc<dyn NativeHostHandler> = host.clone();
        let state = NativeHostState::new(Some(handler));
        let ctx = egui::Context::default();
        let window = WindowId::from(23);
        state
            .begin_output_for_test(&ctx, ViewportId::ROOT, window)
            .expect("the initial output is admitted")
            .finish()
            .present();

        *host.deferred_output.lock() = Some(ViewportId::ROOT);
        assert!(
            state
                .begin_output_for_test(&ctx, ViewportId::ROOT, window)
                .is_none()
        );
        assert_eq!(current_native_output_token(), None);
        assert_eq!(host.outputs.lock().len(), 1);

        *host.deferred_output.lock() = None;
        let retry = state
            .begin_output_for_test(&ctx, ViewportId::ROOT, window)
            .expect("the retried output is admitted");
        assert!(retain_current_native_output());
        let settlement = retry.finish();
        assert_eq!(settlement.render_mode(), NativeOutputRenderMode::Skip);
        settlement.present();
        let outputs = host.outputs.lock();
        assert_eq!(outputs.len(), 2);
        assert_eq!(outputs[1].ordinal().get(), 2);
        assert_eq!(outputs[1].status(), NativeOutputStatus::NotPresented);
    }

    #[test]
    fn disabled_output_host_leaves_ordinary_ui_available() {
        let state = NativeHostState::default();
        let ctx = egui::Context::default();
        let start = state.begin_output(
            &ctx,
            ViewportId::ROOT,
            WindowId::from(24),
            None,
            NativeWindowSnapshot {
                content_size: [0, 0],
                inner_rect: None,
                outer_rect: None,
                native_scale_factor: 1.0,
                presentation_scale_factor: 1.0,
                visible: None,
                minimized: None,
                input_state: NativeWindowInputState::ReceivesInput,
            },
            None,
        );
        assert!(matches!(start, NativeOutputStart::Disabled));
        assert_eq!(current_native_output_token(), None);
        assert!(!state.take_root_wake_after_current());

        let mut ran_ui = false;
        let mut output = ctx.run_ui(egui::RawInput::default(), |_| {
            ran_ui = true;
            assert_eq!(current_native_output_token(), None);
        });
        output.textures_delta.clear();
        assert!(ran_ui);
    }

    #[test]
    fn retained_output_requires_a_previous_exact_presentation() {
        let host = Arc::new(RecordingHost::default());
        let handler: Arc<dyn NativeHostHandler> = Arc::<RecordingHost>::clone(&host);
        let state = NativeHostState::new(Some(handler));
        let ctx = egui::Context::default();
        let child = ViewportId::from_hash_of("retained-output-child");
        let root_window = WindowId::from(11);
        let next_root_window = WindowId::from(12);
        let child_window = WindowId::from(22);

        assert!(!retain_current_native_output());
        let root = state
            .begin_output_for_test(&ctx, ViewportId::ROOT, root_window)
            .expect("the root output scope exists");
        assert!(!retain_current_native_output());
        root.finish().present();

        let retained_root = state
            .begin_output_for_test(&ctx, ViewportId::ROOT, root_window)
            .expect("the retained root output scope exists");
        assert!(retain_current_native_output());
        let retained_root = retained_root.finish();
        assert!(!retained_root.should_present());
        retained_root.present();

        let replaced_root = state
            .begin_output_for_test(&ctx, ViewportId::ROOT, next_root_window)
            .expect("the replacement root output scope exists");
        assert!(!retain_current_native_output());
        replaced_root.finish().present();

        let first_child = state
            .begin_output_for_test(&ctx, child, child_window)
            .expect("the first child output scope exists");
        assert!(!retain_current_native_output());
        first_child.finish().present();

        let retained_child = state
            .begin_output_for_test(&ctx, child, child_window)
            .expect("the retained child output scope exists");
        let child_token = current_native_output_token().expect("the child token is active");
        assert!(retain_current_native_output());
        let retained_child = retained_child.finish();
        assert!(!retained_child.should_present());
        retained_child.present();

        let outputs = host.outputs.lock();
        assert_eq!(outputs.len(), 5);
        assert_eq!(outputs[1].status(), NativeOutputStatus::NotPresented);
        assert_eq!(outputs[4].token(), child_token);
        assert_eq!(outputs[4].status(), NativeOutputStatus::NotPresented);
    }

    #[test]
    fn forgetting_presented_frames_clears_retain_eligibility() {
        let host = Arc::new(RecordingHost::default());
        let handler: Arc<dyn NativeHostHandler> = Arc::<RecordingHost>::clone(&host);
        let state = NativeHostState::new(Some(handler));
        let ctx = egui::Context::default();
        let child = ViewportId::from_hash_of("forget-retained-output-child");
        let child_window = WindowId::from(42);

        state
            .begin_output_for_test(&ctx, child, child_window)
            .expect("the child output scope exists")
            .finish()
            .present();

        let retained_child = state
            .begin_output_for_test(&ctx, child, child_window)
            .expect("the retained child output scope exists");
        assert!(retain_current_native_output());
        retained_child.finish().present();

        let retained_child_again = state
            .begin_output_for_test(&ctx, child, child_window)
            .expect("not presented does not clear the child framebuffer ledger");
        assert!(retain_current_native_output());
        drop(retained_child_again);

        state.forget_presented_window(child, child_window);

        let forgotten_window = state
            .begin_output_for_test(&ctx, child, child_window)
            .expect("the forgotten child output scope exists");
        assert!(!retain_current_native_output());
        forgotten_window.finish().present();

        state
            .begin_output_for_test(&ctx, child, child_window)
            .expect("the child output scope exists again")
            .finish()
            .present();
        state.forget_presented_viewport(child);

        let forgotten_viewport = state
            .begin_output_for_test(&ctx, child, child_window)
            .expect("the forgotten viewport output scope exists");
        assert!(!retain_current_native_output());
        forgotten_viewport.finish().present();
    }

    #[test]
    fn retained_output_requires_matching_inner_size_and_generation() {
        let host = Arc::new(RecordingHost::default());
        let handler: Arc<dyn NativeHostHandler> = Arc::<RecordingHost>::clone(&host);
        let state = NativeHostState::new(Some(handler));
        let ctx = egui::Context::default();
        let viewport_id = ViewportId::ROOT;
        let window_id = WindowId::from(77);
        let initial_snapshot = NativeWindowSnapshot {
            content_size: [800, 600],
            inner_rect: Some(NativePhysicalRect::new(0, 0, 800, 600)),
            outer_rect: None,
            native_scale_factor: 1.0,
            presentation_scale_factor: 1.0,
            visible: Some(true),
            minimized: Some(false),
            input_state: NativeWindowInputState::ReceivesInput,
        };
        let resized_snapshot = NativeWindowSnapshot {
            content_size: [801, 600],
            inner_rect: Some(NativePhysicalRect::new(0, 0, 801, 600)),
            ..initial_snapshot
        };

        state
            .begin_output(&ctx, viewport_id, window_id, None, initial_snapshot, None)
            .accepted()
            .expect("the initial output scope exists")
            .finish()
            .present();

        let retained_same_size = state
            .begin_output(&ctx, viewport_id, window_id, None, initial_snapshot, None)
            .accepted()
            .expect("the retained output scope exists");
        assert!(retain_current_native_output());
        retained_same_size.finish().present();

        let resized_output = state
            .begin_output(&ctx, viewport_id, window_id, None, resized_snapshot, None)
            .accepted()
            .expect("the resized output scope exists");
        assert!(!retain_current_native_output());
        resized_output.finish().present();

        state.invalidate_presented_viewport(viewport_id);

        let invalidated_generation = state
            .begin_output(&ctx, viewport_id, window_id, None, resized_snapshot, None)
            .accepted()
            .expect("the invalidated output scope exists");
        assert!(!retain_current_native_output());
        invalidated_generation.finish().present();
    }

    #[test]
    fn retained_output_checks_content_size_without_desktop_position() {
        let host = Arc::new(RecordingHost::default());
        let handler: Arc<dyn NativeHostHandler> = host;
        let state = NativeHostState::new(Some(handler));
        let ctx = egui::Context::default();
        let viewport_id = ViewportId::ROOT;
        let window_id = WindowId::from(78);
        let initial_snapshot = NativeWindowSnapshot::from_captured_geometry(
            NativeWindowLocalGeometry::from_window_facts([800, 600], 1.0, 1.0),
            None,
            None,
            Some(true),
            None,
            NativeWindowInputState::ReceivesInput,
        );
        state
            .begin_output(&ctx, viewport_id, window_id, None, initial_snapshot, None)
            .accepted()
            .expect("the size-only initial output scope exists")
            .finish()
            .present();

        let unchanged = state
            .begin_output(&ctx, viewport_id, window_id, None, initial_snapshot, None)
            .accepted()
            .expect("the unchanged size-only output scope exists");
        assert!(retain_current_native_output());
        drop(unchanged);

        let resized_snapshot = NativeWindowSnapshot {
            content_size: [801, 600],
            ..initial_snapshot
        };
        let resized = state
            .begin_output(&ctx, viewport_id, window_id, None, resized_snapshot, None)
            .accepted()
            .expect("the resized size-only output scope exists");
        assert!(!retain_current_native_output());
        assert!(!retain_current_native_output_with_overlay());
        drop(resized);
    }

    #[test]
    fn retained_overlay_requires_an_exact_renderer_presentation() {
        let host = Arc::new(RecordingHost::default());
        let handler: Arc<dyn NativeHostHandler> = Arc::<RecordingHost>::clone(&host);
        let state = NativeHostState::new(Some(handler));
        let ctx = egui::Context::default();
        let viewport_id = ViewportId::ROOT;
        let window_id = WindowId::from(81);
        let replacement_window_id = WindowId::from(82);
        let initial_snapshot = NativeWindowSnapshot {
            content_size: [800, 600],
            inner_rect: Some(NativePhysicalRect::new(0, 0, 800, 600)),
            outer_rect: None,
            native_scale_factor: 1.0,
            presentation_scale_factor: 1.0,
            visible: Some(true),
            minimized: Some(false),
            input_state: NativeWindowInputState::ReceivesInput,
        };
        let resized_snapshot = NativeWindowSnapshot {
            content_size: [801, 600],
            inner_rect: Some(NativePhysicalRect::new(0, 0, 801, 600)),
            ..initial_snapshot
        };

        let first_frame = state
            .begin_output(&ctx, viewport_id, window_id, None, initial_snapshot, None)
            .accepted()
            .expect("the first output scope exists");
        assert!(!retain_current_native_output_with_overlay());
        let first_frame = first_frame.finish();
        assert_eq!(first_frame.render_mode(), NativeOutputRenderMode::NoFrame);
        first_frame.present();

        let still_unavailable = state
            .begin_output(&ctx, viewport_id, window_id, None, initial_snapshot, None)
            .accepted()
            .expect("the next no-frame output scope exists");
        assert!(!retain_current_native_output_with_overlay());
        still_unavailable.finish().present();

        state
            .begin_output(&ctx, viewport_id, window_id, None, initial_snapshot, None)
            .accepted()
            .expect("the semantic output scope exists")
            .finish()
            .present();

        let overlay = state
            .begin_output(&ctx, viewport_id, window_id, None, initial_snapshot, None)
            .accepted()
            .expect("the overlay output scope exists");
        assert!(retain_current_native_output_with_overlay());
        assert!(retain_current_native_output());
        let overlay = overlay.finish();
        assert_eq!(overlay.render_mode(), NativeOutputRenderMode::Overlay);
        overlay.present();

        let resized = state
            .begin_output(&ctx, viewport_id, window_id, None, resized_snapshot, None)
            .accepted()
            .expect("the resized output scope exists");
        assert!(!retain_current_native_output_with_overlay());
        let resized = resized.finish();
        assert_eq!(resized.render_mode(), NativeOutputRenderMode::NoFrame);
        resized.present();

        let replacement = state
            .begin_output(
                &ctx,
                viewport_id,
                replacement_window_id,
                None,
                resized_snapshot,
                None,
            )
            .accepted()
            .expect("the replacement-window output scope exists");
        assert!(!retain_current_native_output_with_overlay());
        let replacement = replacement.finish();
        assert_eq!(replacement.render_mode(), NativeOutputRenderMode::NoFrame);
        replacement.present();

        state.invalidate_presented_viewport(viewport_id);
        let invalidated = state
            .begin_output(
                &ctx,
                viewport_id,
                replacement_window_id,
                None,
                resized_snapshot,
                None,
            )
            .accepted()
            .expect("the invalidated-generation output scope exists");
        assert!(!retain_current_native_output_with_overlay());
        let invalidated = invalidated.finish();
        assert_eq!(invalidated.render_mode(), NativeOutputRenderMode::NoFrame);
        invalidated.present();

        let outputs = host.outputs.lock();
        let statuses = outputs
            .iter()
            .map(|output| output.status())
            .collect::<Vec<_>>();
        assert_eq!(
            statuses,
            vec![
                NativeOutputStatus::NotPresented,
                NativeOutputStatus::NotPresented,
                NativeOutputStatus::Presented,
                NativeOutputStatus::NotPresented,
                NativeOutputStatus::NotPresented,
                NativeOutputStatus::NotPresented,
                NativeOutputStatus::NotPresented,
            ]
        );
    }

    #[test]
    fn presented_no_frame_revokes_a_prior_renderer_frame() {
        let host = Arc::new(RecordingHost::default());
        let handler: Arc<dyn NativeHostHandler> = Arc::<RecordingHost>::clone(&host);
        let state = NativeHostState::new(Some(handler));
        let ctx = egui::Context::default();
        let viewport_id = ViewportId::ROOT;
        let window_id = WindowId::from(86);
        let initial_snapshot = NativeWindowSnapshot {
            content_size: [800, 600],
            inner_rect: Some(NativePhysicalRect::new(0, 0, 800, 600)),
            outer_rect: None,
            native_scale_factor: 1.0,
            presentation_scale_factor: 1.0,
            visible: Some(true),
            minimized: Some(false),
            input_state: NativeWindowInputState::ReceivesInput,
        };
        let resized_snapshot = NativeWindowSnapshot {
            content_size: [801, 600],
            inner_rect: Some(NativePhysicalRect::new(0, 0, 801, 600)),
            ..initial_snapshot
        };

        state
            .begin_output(&ctx, viewport_id, window_id, None, initial_snapshot, None)
            .accepted()
            .expect("the semantic output scope exists")
            .finish()
            .present();

        let no_frame = state
            .begin_output(&ctx, viewport_id, window_id, None, resized_snapshot, None)
            .accepted()
            .expect("the resized output scope exists");
        assert!(!retain_current_native_output_with_overlay());
        let no_frame = no_frame.finish();
        assert_eq!(no_frame.render_mode(), NativeOutputRenderMode::NoFrame);
        no_frame.present();

        let restored_size = state
            .begin_output(&ctx, viewport_id, window_id, None, initial_snapshot, None)
            .accepted()
            .expect("the restored-size output scope exists");
        assert!(
            !retain_current_native_output_with_overlay(),
            "the submitted no-frame fallback replaced the prior framebuffer"
        );
        drop(restored_size);
    }

    #[test]
    fn unsubmitted_no_frame_does_not_revoke_a_prior_renderer_frame() {
        let host = Arc::new(RecordingHost::default());
        let handler: Arc<dyn NativeHostHandler> = Arc::<RecordingHost>::clone(&host);
        let state = NativeHostState::new(Some(handler));
        let ctx = egui::Context::default();
        let viewport_id = ViewportId::ROOT;
        let window_id = WindowId::from(88);
        let initial_snapshot = NativeWindowSnapshot {
            content_size: [800, 600],
            inner_rect: Some(NativePhysicalRect::new(0, 0, 800, 600)),
            outer_rect: None,
            native_scale_factor: 1.0,
            presentation_scale_factor: 1.0,
            visible: Some(true),
            minimized: Some(false),
            input_state: NativeWindowInputState::ReceivesInput,
        };
        let resized_snapshot = NativeWindowSnapshot {
            content_size: [801, 600],
            inner_rect: Some(NativePhysicalRect::new(0, 0, 801, 600)),
            ..initial_snapshot
        };

        state
            .begin_output(&ctx, viewport_id, window_id, None, initial_snapshot, None)
            .accepted()
            .expect("the semantic output scope exists")
            .finish()
            .present();

        let no_frame = state
            .begin_output(&ctx, viewport_id, window_id, None, resized_snapshot, None)
            .accepted()
            .expect("the resized output scope exists");
        assert!(!retain_current_native_output_with_overlay());
        let no_frame = no_frame.finish();
        assert_eq!(no_frame.render_mode(), NativeOutputRenderMode::NoFrame);
        drop(no_frame);

        let restored_size = state
            .begin_output(&ctx, viewport_id, window_id, None, initial_snapshot, None)
            .accepted()
            .expect("the restored-size output scope exists");
        assert!(
            retain_current_native_output_with_overlay(),
            "an unsubmitted fallback never replaced the retained framebuffer"
        );
        drop(restored_size);
    }

    #[test]
    fn late_no_frame_predecessor_cannot_revoke_a_presented_successor() {
        let host = Arc::new(RecordingHost::default());
        let handler: Arc<dyn NativeHostHandler> = Arc::<RecordingHost>::clone(&host);
        let state = NativeHostState::new(Some(handler));
        let ctx = egui::Context::default();
        let viewport_id = ViewportId::ROOT;
        let window_id = WindowId::from(87);
        let initial_snapshot = NativeWindowSnapshot {
            content_size: [800, 600],
            inner_rect: Some(NativePhysicalRect::new(0, 0, 800, 600)),
            outer_rect: None,
            native_scale_factor: 1.0,
            presentation_scale_factor: 1.0,
            visible: Some(true),
            minimized: Some(false),
            input_state: NativeWindowInputState::ReceivesInput,
        };
        let resized_snapshot = NativeWindowSnapshot {
            content_size: [801, 600],
            inner_rect: Some(NativePhysicalRect::new(0, 0, 801, 600)),
            ..initial_snapshot
        };

        state
            .begin_output(&ctx, viewport_id, window_id, None, initial_snapshot, None)
            .accepted()
            .expect("the initial semantic output scope exists")
            .finish()
            .present();

        let predecessor = state
            .begin_output(&ctx, viewport_id, window_id, None, resized_snapshot, None)
            .accepted()
            .expect("the no-frame predecessor scope exists");
        assert!(!retain_current_native_output_with_overlay());
        let predecessor = predecessor.finish();
        assert_eq!(predecessor.render_mode(), NativeOutputRenderMode::NoFrame);

        state
            .begin_output(&ctx, viewport_id, window_id, None, initial_snapshot, None)
            .accepted()
            .expect("the semantic successor scope exists")
            .finish()
            .present();
        predecessor.present();

        let after_late_predecessor = state
            .begin_output(&ctx, viewport_id, window_id, None, initial_snapshot, None)
            .accepted()
            .expect("the post-settlement output scope exists");
        assert!(
            retain_current_native_output_with_overlay(),
            "a late no-frame predecessor must not revoke the newer submitted framebuffer"
        );
        drop(after_late_predecessor);
    }

    #[test]
    fn renderer_only_successor_cannot_resurrect_no_frame_semantics() {
        let host = Arc::new(RecordingHost::default());
        let handler: Arc<dyn NativeHostHandler> = Arc::<RecordingHost>::clone(&host);
        let state = NativeHostState::new(Some(handler));
        let ctx = egui::Context::default();
        let viewport_id = ViewportId::ROOT;
        let window_id = WindowId::from(89);
        let initial_snapshot = NativeWindowSnapshot {
            content_size: [800, 600],
            inner_rect: Some(NativePhysicalRect::new(0, 0, 800, 600)),
            outer_rect: None,
            native_scale_factor: 1.0,
            presentation_scale_factor: 1.0,
            visible: Some(true),
            minimized: Some(false),
            input_state: NativeWindowInputState::ReceivesInput,
        };
        let resized_snapshot = NativeWindowSnapshot {
            content_size: [801, 600],
            inner_rect: Some(NativePhysicalRect::new(0, 0, 801, 600)),
            ..initial_snapshot
        };

        state
            .begin_output(&ctx, viewport_id, window_id, None, initial_snapshot, None)
            .accepted()
            .expect("the initial semantic output scope exists")
            .finish()
            .present();

        let no_frame = state
            .begin_output(&ctx, viewport_id, window_id, None, resized_snapshot, None)
            .accepted()
            .expect("the no-frame output scope exists");
        assert!(!retain_current_native_output_with_overlay());
        let no_frame = no_frame.finish();

        let overlay = state
            .begin_output(&ctx, viewport_id, window_id, None, initial_snapshot, None)
            .accepted()
            .expect("the renderer-only successor scope exists");
        assert!(retain_current_native_output_with_overlay());
        let overlay = overlay.finish();

        no_frame.present();
        overlay.present();

        let after_overlay = state
            .begin_output(&ctx, viewport_id, window_id, None, initial_snapshot, None)
            .accepted()
            .expect("the output after the renderer-only successor exists");
        assert!(
            !retain_current_native_output_with_overlay(),
            "a visual-only overlay cannot restore semantic presentation authority"
        );
        drop(after_overlay);
    }

    #[test]
    fn semantic_result_without_renderer_submission_cannot_enable_retention() {
        let host = Arc::new(RecordingHost::default());
        let handler: Arc<dyn NativeHostHandler> = Arc::<RecordingHost>::clone(&host);
        let state = NativeHostState::new(Some(handler));
        let ctx = egui::Context::default();
        let window_id = WindowId::from(84);

        let mut settlement = state
            .begin_output_for_test(&ctx, ViewportId::ROOT, window_id)
            .expect("the output scope exists")
            .finish();
        settlement.settle(NativeOutputStatus::Presented, None);

        let next = state
            .begin_output_for_test(&ctx, ViewportId::ROOT, window_id)
            .expect("the next output scope exists");
        assert!(!retain_current_native_output());
        assert!(!retain_current_native_output_with_overlay());
        next.finish().present();
    }

    #[test]
    fn ordinary_retain_skips_renderer_paint() {
        let host = Arc::new(RecordingHost::default());
        let handler: Arc<dyn NativeHostHandler> = Arc::<RecordingHost>::clone(&host);
        let state = NativeHostState::new(Some(handler));
        let ctx = egui::Context::default();
        let window_id = WindowId::from(83);

        state
            .begin_output_for_test(&ctx, ViewportId::ROOT, window_id)
            .expect("the first output scope exists")
            .finish()
            .present();

        let retained = state
            .begin_output_for_test(&ctx, ViewportId::ROOT, window_id)
            .expect("the retained output scope exists");
        assert!(retain_current_native_output());
        assert_eq!(
            retained.finish().render_mode(),
            NativeOutputRenderMode::Skip
        );
    }

    #[test]
    fn root_output_binds_one_complete_roster_to_the_active_token() {
        let host = Arc::new(RecordingHost::default());
        let handler: Arc<dyn NativeHostHandler> = Arc::<RecordingHost>::clone(&host);
        let state = NativeHostState::new(Some(handler));
        let ctx = egui::Context::default();
        let root_window = WindowId::from(11);
        let child_window = WindowId::from(22);
        let child_viewport = ViewportId::from_hash_of("child-roster");
        let root_snapshot = NativeWindowSnapshot {
            content_size: [800, 600],
            inner_rect: Some(NativePhysicalRect::new(10, 20, 800, 600)),
            outer_rect: Some(NativePhysicalRect::new(2, -10, 816, 638)),
            native_scale_factor: 2.0,
            presentation_scale_factor: 2.5,
            visible: Some(true),
            minimized: Some(false),
            input_state: NativeWindowInputState::ReceivesInput,
        };
        let child_snapshot = NativeWindowSnapshot {
            content_size: [640, 480],
            inner_rect: Some(NativePhysicalRect::new(900, 20, 640, 480)),
            outer_rect: None,
            native_scale_factor: 1.5,
            presentation_scale_factor: 1.25,
            visible: None,
            minimized: None,
            input_state: NativeWindowInputState::PassThrough,
        };
        let roster = [
            NativeViewportRecord {
                viewport_id: ViewportId::ROOT,
                window_id: root_window,
                window: root_snapshot,
            },
            NativeViewportRecord {
                viewport_id: child_viewport,
                window_id: child_window,
                window: child_snapshot,
            },
        ];
        let work_areas = [NativeWorkAreaRecord::new(
            7,
            NativePhysicalRect::new(0, 0, 1920, 1080),
            NativePhysicalRect::new(0, 40, 1920, 1040),
            2.0,
        )];

        let scope = state
            .begin_output(
                &ctx,
                ViewportId::ROOT,
                root_window,
                None,
                root_snapshot,
                Some(NativeViewportRoster::new(
                    &roster,
                    NativeWorkAreaRoster::Exact(&work_areas),
                    NativeWindowingBackend::Windows,
                )),
            )
            .accepted()
            .expect("root output scope exists");
        let token = current_native_output_token().expect("root token is active");

        let output_begins = host.output_begins.lock();
        assert_eq!(output_begins.len(), 1);
        assert_eq!(output_begins[0].0, token);
        assert_eq!(output_begins[0].1, root_snapshot);
        let recorded_roster = output_begins[0]
            .2
            .as_ref()
            .expect("the root callback includes its native roster");
        assert_eq!(
            recorded_roster.records[0].window().input_state(),
            NativeWindowInputState::ReceivesInput
        );
        assert_eq!(
            recorded_roster.records[1].window().input_state(),
            NativeWindowInputState::PassThrough
        );
        assert_eq!(
            output_begins[0].2,
            Some(RecordedNativeViewportRoster {
                records: roster.to_vec(),
                work_areas: Some(work_areas.to_vec()),
                backend: NativeWindowingBackend::Windows,
            })
        );
        drop(output_begins);
        scope.finish().present();
    }

    #[test]
    fn deferred_output_never_claims_a_root_roster() {
        let host = Arc::new(RecordingHost::default());
        let handler: Arc<dyn NativeHostHandler> = Arc::<RecordingHost>::clone(&host);
        let state = NativeHostState::new(Some(handler));
        let child = ViewportId::from_hash_of("child-without-roster");

        state
            .begin_output_for_test(&egui::Context::default(), child, WindowId::from(22))
            .expect("child scope exists")
            .finish()
            .present();

        assert!(host.output_begins.lock()[0].2.is_none());
    }

    #[test]
    fn host_pass_finalizer_receives_the_active_output_token() {
        #[derive(Default)]
        struct LateDiscard {
            remaining: usize,
        }

        impl egui::plugin::Plugin for LateDiscard {
            fn debug_name(&self) -> &'static str {
                "eframe::test::late_output_discard"
            }

            fn output_hook(&mut self, _context: &egui::Context, output: &mut egui::FullOutput) {
                if self.remaining == 0 {
                    return;
                }
                self.remaining -= 1;
                output
                    .platform_output
                    .request_discard_reasons
                    .push(egui::RepaintCause::new_reason("late host-seam discard"));
            }
        }

        #[derive(Default)]
        struct FinalizingHost {
            finalizations: Mutex<Vec<(NativeOutputToken, ViewportId, egui::OutputPassDisposition)>>,
        }

        impl NativeHostHandler for FinalizingHost {
            fn on_output_pass_finalized(
                &self,
                finalization: NativeOutputPassFinalization<'_>,
            ) -> NativeHostWake {
                let (_context, token, ended_viewport, disposition, _output) =
                    finalization.into_parts();
                self.finalizations
                    .lock()
                    .push((token, ended_viewport, disposition));
                NativeHostWake::Wait
            }
        }

        let host = Arc::new(FinalizingHost::default());
        let handler: Arc<dyn NativeHostHandler> = Arc::<FinalizingHost>::clone(&host);
        let state = NativeHostState::new(Some(handler));
        let context = egui::Context::default();
        context.options_mut(|options| {
            options.max_passes = 2.try_into().expect("two is non-zero");
        });
        context.plugin_or_default::<LateDiscard>().lock().remaining = 1;
        let scope = state
            .begin_output_for_test(&context, ViewportId::ROOT, WindowId::from(17))
            .expect("the native output scope exists");
        let token = current_native_output_token().expect("the active scope publishes its token");

        let mut output = context.run_ui_with_pass_finalizer(
            egui::RawInput::default(),
            |_| {},
            |context, ended_viewport, disposition, output| {
                state.finalize_output_pass(context, ended_viewport, disposition, output);
            },
        );
        output.textures_delta.clear();
        scope.finish().present();

        assert_eq!(
            *host.finalizations.lock(),
            [
                (token, ViewportId::ROOT, egui::OutputPassDisposition::Repeat,),
                (
                    token,
                    ViewportId::ROOT,
                    egui::OutputPassDisposition::Terminal,
                ),
            ]
        );
    }

    #[test]
    fn unwinding_output_scope_reports_an_exact_not_presented_tombstone() {
        let host = Arc::new(RecordingHost::default());
        let handler: Arc<dyn NativeHostHandler> = Arc::<RecordingHost>::clone(&host);
        let state = NativeHostState::new(Some(handler));
        let context = egui::Context::default();
        let token = Arc::new(Mutex::new(None));
        let observed_token = Arc::clone(&token);

        let interrupted = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _scope = state
                .begin_output_for_test(&context, ViewportId::ROOT, WindowId::from(19))
                .expect("the native output scope exists");
            *observed_token.lock() = current_native_output_token();
            panic!("the UI callback unwound");
        }));

        assert!(interrupted.is_err());
        let token = token
            .lock()
            .expect("the output token was captured before unwinding");
        assert_eq!(
            *host.outputs.lock(),
            [NativeOutputResult {
                token,
                ordinal: NativeOutputOrdinal(NonZeroU64::MIN),
                status: NativeOutputStatus::NotPresented,
            }],
            "unwinding must terminate the exact reserved output token"
        );
    }

    #[test]
    fn wait_does_not_schedule_an_output_loop() {
        let host = Arc::new(RecordingHost::default());
        let handler: Arc<dyn NativeHostHandler> = Arc::<RecordingHost>::clone(&host);
        let state = NativeHostState::new(Some(handler));
        let ctx = egui::Context::default();
        let repaint_count = Arc::new(AtomicUsize::new(0));
        ctx.set_request_repaint_callback({
            let repaint_count = Arc::clone(&repaint_count);
            move |_| {
                repaint_count.fetch_add(1, Ordering::Relaxed);
            }
        });

        state
            .begin_output_for_test(&ctx, ViewportId::ROOT, WindowId::from(11))
            .unwrap()
            .finish()
            .present();

        assert_eq!(host.outputs.lock().len(), 1);
        assert_eq!(repaint_count.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn output_wake_schedules_one_causal_root_pass() {
        let host = Arc::new(RecordingHost {
            wake: NativeHostWake::RepaintRoot,
            ..Default::default()
        });
        let handler: Arc<dyn NativeHostHandler> = Arc::<RecordingHost>::clone(&host);
        let state = NativeHostState::new(Some(handler));
        let ctx = egui::Context::default();
        for _ in 0..3 {
            let mut warm_up = ctx.run_ui(Default::default(), |_| {});
            warm_up.textures_delta.clear();
        }
        let repaint_count = Arc::new(AtomicUsize::new(0));
        ctx.set_request_repaint_callback({
            let repaint_count = Arc::clone(&repaint_count);
            move |_| {
                repaint_count.fetch_add(1, Ordering::Relaxed);
            }
        });

        state
            .begin_output_for_test(&ctx, ViewportId::ROOT, WindowId::from(11))
            .unwrap()
            .finish()
            .present();
        assert_eq!(repaint_count.load(Ordering::Relaxed), 1);
        let mut output = ctx.run_ui(Default::default(), |_| {});
        output.textures_delta.clear();

        assert_eq!(host.outputs.lock().len(), 1);
        assert_eq!(repaint_count.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn durable_output_wake_is_owned_by_the_outer_event_boundary() {
        let host = Arc::new(RecordingHost {
            wake: NativeHostWake::RepaintRootAfterCurrent,
            ..Default::default()
        });
        let handler: Arc<dyn NativeHostHandler> = Arc::<RecordingHost>::clone(&host);
        let state = NativeHostState::new(Some(handler));
        let ctx = egui::Context::default();
        let repaint_count = Arc::new(AtomicUsize::new(0));
        ctx.set_request_repaint_callback({
            let repaint_count = Arc::clone(&repaint_count);
            move |info| {
                if info.viewport_id == ViewportId::ROOT {
                    repaint_count.fetch_add(1, Ordering::Relaxed);
                }
            }
        });

        state
            .begin_output_for_test(&ctx, ViewportId::ROOT, WindowId::from(11))
            .expect("root output scope exists")
            .finish()
            .present();
        assert_eq!(
            repaint_count.load(Ordering::Relaxed),
            0,
            "the completed renderer callback must not enter egui's pass-local repaint queue"
        );
        assert!(state.take_root_wake_after_current());
        ctx.request_repaint_of(ViewportId::ROOT);
        assert_eq!(
            repaint_count.load(Ordering::Relaxed),
            1,
            "the outer event boundary must re-enter through the repaint callback"
        );
        assert!(!state.take_root_wake_after_current());
    }

    #[test]
    fn output_settlement_is_idempotent() {
        let host = Arc::new(RecordingHost::default());
        let handler: Arc<dyn NativeHostHandler> = Arc::<RecordingHost>::clone(&host);
        let state = NativeHostState::new(Some(handler));
        let ctx = egui::Context::default();

        let mut settlement = state
            .begin_output_for_test(&ctx, ViewportId::ROOT, WindowId::from(11))
            .unwrap()
            .finish();
        settlement.settle(
            NativeOutputStatus::Presented,
            Some(NativeRetentionKind::Retainable),
        );
        settlement.settle(NativeOutputStatus::NotPresented, None);

        assert_eq!(host.outputs.lock().len(), 1);
        assert_eq!(
            host.outputs.lock()[0].status(),
            NativeOutputStatus::Presented
        );
    }

    #[test]
    fn output_token_keeps_the_callback_window_identity() {
        let host = Arc::new(RecordingHost::default());
        let handler: Arc<dyn NativeHostHandler> = Arc::<RecordingHost>::clone(&host);
        let state = NativeHostState::new(Some(handler));
        let window = WindowId::from(17);
        let scope = state
            .begin_output_for_test(&egui::Context::default(), ViewportId::ROOT, window)
            .unwrap();
        let token = current_native_output_token().expect("output scope publishes its token");

        assert_eq!(token.window_id(), window);
        scope.finish().present();
    }

    #[test]
    fn deferred_output_echoes_the_exact_physical_create_attempt() {
        let host = Arc::new(RecordingHost::default());
        let handler: Arc<dyn NativeHostHandler> = Arc::<RecordingHost>::clone(&host);
        let state = NativeHostState::new(Some(handler));
        let ctx = egui::Context::default();
        let viewport_id = ViewportId::from_hash_of("attempt-bound-output");
        let window_id = WindowId::from(19);
        let NativeDeferredWindowPreparation::Admitted { create_attempt, .. } = state
            .prepare_deferred_window(
                &ctx,
                NativeViewportVisibilityStatus::Dispatched,
                viewport_id,
                &ViewportBuilder::default(),
            )
        else {
            panic!("an enabled native host mints one exact create attempt");
        };

        let scope = state
            .begin_output(
                &ctx,
                viewport_id,
                window_id,
                Some(create_attempt),
                NativeWindowSnapshot {
                    content_size: [0, 0],
                    inner_rect: None,
                    outer_rect: None,
                    native_scale_factor: 1.0,
                    presentation_scale_factor: 1.0,
                    visible: None,
                    minimized: None,
                    input_state: NativeWindowInputState::ReceivesInput,
                },
                None,
            )
            .accepted()
            .expect("the deferred output scope exists");
        let token = current_native_output_token().expect("the output token is active");

        assert_eq!(token.viewport_id(), viewport_id);
        assert_eq!(token.window_id(), window_id);
        assert_eq!(token.create_attempt(), Some(create_attempt));
        scope.finish().present();
    }

    #[test]
    fn output_tokens_expose_only_context_equality() {
        let first_host = Arc::new(RecordingHost::default());
        let first_handler: Arc<dyn NativeHostHandler> = Arc::<RecordingHost>::clone(&first_host);
        let first_state = NativeHostState::new(Some(first_handler));
        let context = egui::Context::default();
        let first = first_state
            .begin_output_for_test(&context, ViewportId::ROOT, WindowId::from(11))
            .expect("first scope exists");
        let first_token = current_native_output_token().expect("first token is active");
        let second = first_state
            .begin_output_for_test(
                &context,
                ViewportId::from_hash_of("same-context"),
                WindowId::from(12),
            )
            .expect("second scope exists");
        let second_token = current_native_output_token().expect("second token is active");

        let foreign_host = Arc::new(RecordingHost::default());
        let foreign_handler: Arc<dyn NativeHostHandler> =
            Arc::<RecordingHost>::clone(&foreign_host);
        let foreign_state = NativeHostState::new(Some(foreign_handler));
        let foreign = foreign_state
            .begin_output_for_test(
                &context,
                ViewportId::from_hash_of("other-context"),
                WindowId::from(13),
            )
            .expect("foreign scope exists");
        let foreign_token = current_native_output_token().expect("foreign token is active");

        assert!(first_token.same_context(second_token));
        assert!(!first_token.same_context(foreign_token));

        foreign.finish().present();
        second.finish().present();
        first.finish().present();
    }

    #[test]
    fn viewport_create_failure_keeps_identity_and_wake() {
        let host = Arc::new(RecordingHost {
            wake: NativeHostWake::RepaintRoot,
            ..Default::default()
        });
        let handler: Arc<dyn NativeHostHandler> = Arc::<RecordingHost>::clone(&host);
        let state = NativeHostState::new(Some(handler));
        let ctx = egui::Context::default();
        let repaint_count = Arc::new(AtomicUsize::new(0));
        ctx.set_request_repaint_callback({
            let repaint_count = Arc::clone(&repaint_count);
            move |_| {
                repaint_count.fetch_add(1, Ordering::Relaxed);
            }
        });
        let viewport_id = ViewportId::from_hash_of("failed-child");
        let NativeDeferredWindowPreparation::Admitted { create_attempt, .. } = state
            .prepare_deferred_window(
                &ctx,
                NativeViewportVisibilityStatus::Dispatched,
                viewport_id,
                &ViewportBuilder::default(),
            )
        else {
            panic!("an enabled native host mints one exact create attempt");
        };

        state.notify_viewport_create_failed(
            &ctx,
            create_attempt,
            NativeViewportCreateFailureKind::WindowUnavailable,
        );

        assert_eq!(host.create_failures.lock()[0].viewport_id(), viewport_id);
        assert_eq!(
            host.create_failures.lock()[0].create_attempt(),
            create_attempt
        );
        assert_eq!(
            host.create_failures.lock()[0].kind(),
            NativeViewportCreateFailureKind::WindowUnavailable
        );
        assert_eq!(repaint_count.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn prepared_property_commands_do_not_enqueue_until_the_host_has_reserved_their_tokens() {
        let host = Arc::new(RecordingHost::default());
        let handler: Arc<dyn NativeHostHandler> = Arc::<RecordingHost>::clone(&host);
        let state = NativeHostState::new(Some(handler));
        let context = egui::Context::default();
        state.bind_close_context(&context);
        let viewport = ViewportId::from_hash_of("prepared-property-owner");
        let focus = prepare_native_viewport_focus(viewport);
        let visibility = prepare_native_viewport_visibility(viewport, true);
        let pointer = prepare_native_viewport_pointer_passthrough(viewport, false);
        let focus_token = focus.token();
        let visibility_token = visibility.token();
        let pointer_token = pointer.token();
        assert!(state.take_viewport_property_commands(viewport).is_empty());

        // Rejected preparation spends only its identity, without a command or
        // a retirement callback which would need an unreserved terminal owner.
        let rejected = prepare_native_viewport_focus(viewport);
        let rejected_token = rejected.token();
        assert_ne!(rejected_token, focus_token);
        drop(rejected);
        assert!(state.take_viewport_property_commands(viewport).is_empty());

        focus.enqueue(&context);
        visibility.enqueue(&context);
        pointer.enqueue(&context);
        assert_eq!(
            state
                .take_viewport_property_commands(viewport)
                .into_iter()
                .collect::<Vec<_>>(),
            [
                QueuedNativeViewportPropertyCommand::Focus(focus_token),
                QueuedNativeViewportPropertyCommand::Visibility {
                    token: visibility_token,
                    visible: true
                },
                QueuedNativeViewportPropertyCommand::PointerPassthrough {
                    token: pointer_token,
                    enabled: false
                },
            ]
        );
    }

    #[test]
    fn viewport_visibility_result_keeps_exact_identity_and_wake() {
        let host = Arc::new(RecordingHost {
            wake: NativeHostWake::RepaintRoot,
            ..Default::default()
        });
        let handler: Arc<dyn NativeHostHandler> = Arc::<RecordingHost>::clone(&host);
        let state = NativeHostState::new(Some(handler));
        let ctx = egui::Context::default();
        let repaint_count = Arc::new(AtomicUsize::new(0));
        ctx.set_request_repaint_callback({
            let repaint_count = Arc::clone(&repaint_count);
            move |_| {
                repaint_count.fetch_add(1, Ordering::Relaxed);
            }
        });
        state.bind_close_context(&ctx);
        let viewport_id = ViewportId::from_hash_of("shown-child");
        let token = queue_native_viewport_visibility(&ctx, viewport_id, true);
        let result = NativeViewportVisibilityResult {
            token,
            viewport_id,
            window_id: WindowId::from(31),
            visible: true,
            status: NativeViewportVisibilityStatus::Dispatched,
        };

        state.notify_viewport_visibility_result(&ctx, result);

        assert_eq!(result.token(), token);
        assert_eq!(host.visibility_results.lock().as_slice(), &[result]);
        assert!(ctx.has_requested_repaint_for(&ViewportId::ROOT));
        assert_eq!(repaint_count.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn deferred_window_preparation_is_exact_child_only_and_physical() {
        use winit::dpi::{PhysicalPosition, PhysicalSize, Position, Size};

        let host = Arc::new(RecordingHost::default());
        let viewport_id = ViewportId::from_hash_of("deferred-child");
        let rect = NativePhysicalRect::new(120, 240, 800, 600);
        *host.deferred_rect.lock() = Some((viewport_id, rect));
        let handler: Arc<dyn NativeHostHandler> = Arc::<RecordingHost>::clone(&host);
        let state = NativeHostState::new(Some(handler));

        let ctx = egui::Context::default();
        assert_eq!(
            state.prepare_deferred_window(
                &ctx,
                NativeViewportVisibilityStatus::Dispatched,
                ViewportId::ROOT,
                &ViewportBuilder::default(),
            ),
            NativeDeferredWindowPreparation::Unmanaged
        );

        let NativeDeferredWindowPreparation::Admitted {
            create_attempt: attempt,
            window_override,
        } = state.prepare_deferred_window(
            &ctx,
            NativeViewportVisibilityStatus::Dispatched,
            viewport_id,
            &ViewportBuilder::default(),
        )
        else {
            panic!("the child request is admitted");
        };
        let attributes = window_override
            .expect("the child geometry is available")
            .apply_to_attributes(WindowAttributes::default());

        assert!(!attributes.decorations);
        assert_eq!(
            attributes.position,
            Some(Position::Physical(PhysicalPosition::new(120, 240)))
        );
        assert_eq!(
            attributes.inner_size,
            Some(Size::Physical(PhysicalSize::new(800, 600)))
        );
        assert_eq!(
            (rect.x(), rect.y(), rect.width(), rect.height()),
            (120, 240, 800, 600)
        );
        assert_eq!(attempt.viewport_id(), viewport_id);

        let NativeDeferredWindowPreparation::Admitted {
            create_attempt: second_attempt,
            window_override: incompatible_override,
        } = state.prepare_deferred_window(
            &ctx,
            NativeViewportVisibilityStatus::Dispatched,
            viewport_id,
            &ViewportBuilder::default().with_maximized(true),
        )
        else {
            panic!("incompatible geometry does not cancel physical creation");
        };
        assert_ne!(second_attempt, attempt);
        assert!(incompatible_override.is_none());
    }

    #[test]
    fn deferred_admission_does_not_create_a_fake_failure_or_override() {
        let host = Arc::new(RecordingHost::default());
        let viewport_id = ViewportId::from_hash_of("deferred-admission");
        *host.deferred_viewport.lock() = Some(viewport_id);
        let handler: Arc<dyn NativeHostHandler> = Arc::<RecordingHost>::clone(&host);
        let state = NativeHostState::new(Some(handler));

        assert_eq!(
            state.prepare_deferred_window(
                &egui::Context::default(),
                NativeViewportVisibilityStatus::Dispatched,
                viewport_id,
                &ViewportBuilder::default(),
            ),
            NativeDeferredWindowPreparation::Defer
        );
        assert!(host.create_failures.lock().is_empty());
        assert_eq!(host.create_attempts.lock().len(), 1);
    }

    #[test]
    #[should_panic(expected = "show_viewport_immediate is unavailable")]
    fn immediate_viewports_are_rejected_with_a_precise_error() {
        let host = Arc::new(RecordingHost::default());
        let handler: Arc<dyn NativeHostHandler> = Arc::<RecordingHost>::clone(&host);
        NativeHostState::new(Some(handler)).assert_immediate_viewports_supported();
    }
}

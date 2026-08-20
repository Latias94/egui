//! Narrow native event and renderer-settlement seam.
//!
//! This module deliberately does not expose renderer resources, platform handles, or an
//! application transaction protocol. A host observes immutable native events before egui input
//! translation and receives terminal presentation results for context-local output tokens.

use std::cell::RefCell;
use std::collections::{BTreeMap, VecDeque};
use std::num::NonZeroU64;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use egui::{ViewportBuilder, ViewportId};
#[cfg(target_os = "linux")]
use raw_window_handle::{HasDisplayHandle as _, HasWindowHandle as _};
use winit::event_loop::ActiveEventLoop;
use winit::window::{Window, WindowAttributes, WindowId};

mod work_area;

use work_area::OwnedNativeWorkAreaRoster;
pub use work_area::{NativeDisplayId, NativeWorkAreaRecord, NativeWorkAreaRoster};

static NEXT_CONTEXT_ID: AtomicU64 = AtomicU64::new(1);
static NEXT_POINTER_PASSTHROUGH_COMMAND: AtomicU64 = AtomicU64::new(1);

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

/// Exact native close request retained until eframe decides whether the
/// matching viewport output cancelled it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct NativeViewportCloseRequest {
    event: NativeEventOrdinal,
    viewport_id: ViewportId,
    window_id: WindowId,
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
}

/// Result of one native visibility dispatch.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct NativeViewportVisibilityResult {
    viewport_id: ViewportId,
    window_id: WindowId,
    visible: bool,
    status: NativeViewportVisibilityStatus,
}

impl NativeViewportVisibilityResult {
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

/// Window facts captured for the exact callback that owns an output token.
///
/// Unsupported or unavailable facts remain [`None`]. In particular, this
/// snapshot does not infer desktop geometry from cached viewport information.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct NativeWindowSnapshot {
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
        let inner_rect = window
            .inner_position()
            .ok()
            .map(|position| physical_rect(position, window.inner_size()));

        #[cfg(any(target_os = "windows", target_os = "macos"))]
        let outer_rect = window
            .outer_position()
            .ok()
            .map(|position| physical_rect(position, window.outer_size()));
        #[cfg(not(any(target_os = "windows", target_os = "macos")))]
        let outer_rect = None;

        #[cfg(target_os = "windows")]
        let minimized = window.is_minimized();
        #[cfg(not(target_os = "windows"))]
        let minimized = None;

        let visible = window.is_visible();

        Self {
            inner_rect,
            outer_rect,
            native_scale_factor: window.scale_factor(),
            presentation_scale_factor: f64::from(egui_winit::pixels_per_point(egui_ctx, window)),
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
    event: &'a winit::event::WindowEvent,
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

    /// Returns the immutable native event before egui-winit translation.
    pub const fn event(&self) -> &winit::event::WindowEvent {
        self.event
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
}

/// Exact viewport focus command consumed by eframe.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NativeViewportFocusResult {
    viewport_id: ViewportId,
    window_id: WindowId,
    status: NativeViewportFocusStatus,
}

impl NativeViewportFocusResult {
    /// Returns the target eframe viewport.
    pub const fn viewport_id(self) -> ViewportId {
        self.viewport_id
    }

    /// Returns the exact target native window.
    pub const fn window_id(self) -> WindowId {
        self.window_id
    }

    /// Returns whether the command observed focus or requested it.
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct QueuedNativeViewportPointerPassthroughCommand {
    token: NativeViewportPointerPassthroughCommandToken,
    enabled: bool,
}

#[derive(Clone, Default)]
struct QueuedNativeViewportPointerPassthroughCommands(
    BTreeMap<ViewportId, VecDeque<QueuedNativeViewportPointerPassthroughCommand>>,
);

fn pointer_passthrough_commands_id() -> egui::Id {
    egui::Id::new("eframe::native_host::pointer_passthrough_commands")
}

/// Queues one host-owned pointer pass-through command for an exact viewport.
///
/// Ordinary [`egui::ViewportCommand::MousePassthrough`] commands remain application-owned and do
/// not produce [`NativeHostHandler::on_viewport_pointer_passthrough`] callbacks. This dedicated
/// path returns an opaque token which is echoed by the terminal callback, so a native coordinator
/// cannot accidentally consume an unrelated same-value application command.
pub fn queue_native_viewport_pointer_passthrough(
    ctx: &egui::Context,
    viewport_id: ViewportId,
    enabled: bool,
) -> NativeViewportPointerPassthroughCommandToken {
    let token = NativeViewportPointerPassthroughCommandToken(next_non_zero(
        &NEXT_POINTER_PASSTHROUGH_COMMAND,
        "native pointer pass-through command identity exhausted",
    ));
    ctx.data_mut(|data| {
        data.get_temp_mut_or_default::<QueuedNativeViewportPointerPassthroughCommands>(
            pointer_passthrough_commands_id(),
        )
        .0
        .entry(viewport_id)
        .or_default()
        .push_back(QueuedNativeViewportPointerPassthroughCommand { token, enabled });
    });

    // Ensure the target viewport participates in platform-output processing. The ordinary command
    // is intentionally callback-free; the host-owned command is applied last and supplies the
    // exact terminal result.
    ctx.send_viewport_cmd_to(
        viewport_id,
        egui::ViewportCommand::MousePassthrough(enabled),
    );
    token
}

fn take_native_viewport_pointer_passthrough_commands(
    ctx: &egui::Context,
    viewport_id: ViewportId,
) -> VecDeque<QueuedNativeViewportPointerPassthroughCommand> {
    ctx.data_mut(|data| {
        data.get_temp_mut_or_default::<QueuedNativeViewportPointerPassthroughCommands>(
            pointer_passthrough_commands_id(),
        )
        .0
        .remove(&viewport_id)
        .unwrap_or_default()
    })
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
    fn on_window_event(&self, _event: NativeWindowEvent<'_>) {}

    /// Observes one physical-device removal before eframe handles it.
    fn on_device_removed(&self, _removal: NativeDeviceRemoval) {}

    /// Observes globally consistent focus after eframe applies one focus event.
    fn on_global_focus(&self, _observation: NativeGlobalFocusObservation) -> NativeHostWake {
        NativeHostWake::Wait
    }

    /// Reports that eframe consumed one exact viewport focus command.
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

    /// Announces the output token before the viewport UI callback runs.
    ///
    /// Hosts may reserve the token here and attach the affine painted output
    /// after the core frame commits. The callback is observation-only and must
    /// not re-enter eframe.
    fn on_output_begin(
        &self,
        _token: NativeOutputToken,
        _window: NativeWindowSnapshot,
        _root_roster: Option<NativeViewportRoster<'_>>,
    ) {
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

    /// Reports a native visibility request after eframe attempts to dispatch it.
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
    pending_viewport_closes: Mutex<BTreeMap<ViewportId, NativeViewportCloseRequest>>,
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
        event: &winit::event::WindowEvent,
    ) {
        let Some(inner) = &self.inner else {
            return;
        };
        if let Some(viewport_id) = viewport_id
            && matches!(event, winit::event::WindowEvent::CloseRequested)
        {
            let mut pending = inner
                .pending_viewport_closes
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            pending
                .entry(viewport_id)
                .or_insert(NativeViewportCloseRequest {
                    event: ordinal,
                    viewport_id,
                    window_id,
                });
        }
        inner.handler.on_window_event(NativeWindowEvent {
            ordinal,
            window_id,
            viewport_id,
            event,
        });
        if let Some(ctx) = ctx {
            ctx.request_repaint_of(ViewportId::ROOT);
        }
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
        cancelled: bool,
    ) {
        let Some(inner) = &self.inner else {
            return;
        };
        let request = inner
            .pending_viewport_closes
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&viewport_id);
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
    ) -> Option<NativeOutputScope> {
        let inner = Arc::clone(self.inner.as_ref()?);
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
        let frame = inner.output_frame(viewport_id, window_id, snapshot);
        let retain_eligible = inner.has_presented_output(viewport_id, frame)
            && inner.has_renderer_output(viewport_id, frame);
        inner.handler.on_output_begin(token, snapshot, root_roster);
        ACTIVE_OUTPUTS.with(|outputs| {
            outputs.borrow_mut().push(ActiveNativeOutput {
                token,
                render_mode: NativeOutputRenderMode::Replace,
                retain_eligible,
            });
        });
        Some(NativeOutputScope {
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

    fn notify_viewport_visibility(
        &self,
        ctx: &egui::Context,
        viewport_id: ViewportId,
        window: &Window,
        visible: bool,
    ) {
        let status = visibility_control_for_window(window);
        self.notify_viewport_visibility_result(
            ctx,
            NativeViewportVisibilityResult {
                viewport_id,
                window_id: window.id(),
                visible,
                status,
            },
        );
    }

    fn notify_viewport_visibility_result(
        &self,
        ctx: &egui::Context,
        result: NativeViewportVisibilityResult,
    ) {
        let Some(inner) = &self.inner else {
            return;
        };
        if inner.handler.on_viewport_visibility(result) == NativeHostWake::RepaintRoot {
            ctx.request_repaint_of(ViewportId::ROOT);
        }
    }

    pub(crate) fn deferred_visibility_status(
        &self,
        event_loop: &ActiveEventLoop,
    ) -> NativeViewportVisibilityStatus {
        visibility_control_for_event_loop(event_loop)
    }

    pub(crate) fn process_viewport_commands(
        &self,
        ctx: &egui::Context,
        viewport_id: ViewportId,
        info: &mut egui::ViewportInfo,
        commands: impl IntoIterator<Item = egui::ViewportCommand>,
        window: &Window,
        actions_requested: &mut Vec<egui_winit::ActionRequested>,
    ) {
        for command in commands {
            let requested_visibility = match command {
                egui::ViewportCommand::Visible(visible) => Some(visible),
                _ => None,
            };
            let requested_focus = matches!(command, egui::ViewportCommand::Focus).then(|| {
                if window.has_focus() {
                    NativeViewportFocusStatus::AlreadyFocused
                } else {
                    NativeViewportFocusStatus::Requested
                }
            });
            if self.inner.is_some()
                && let egui::ViewportCommand::MousePassthrough(enabled) = &command
            {
                match self.apply_window_input_state(viewport_id, window, *enabled) {
                    Ok(_) => {}
                    Err(error) => {
                        log::warn!("{command:?}: {error}");
                    }
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
            if let Some(visible) = requested_visibility {
                self.notify_viewport_visibility(ctx, viewport_id, window, visible);
            }
            if let Some(status) = requested_focus {
                self.notify_viewport_focus(ctx, viewport_id, window.id(), status);
            }
        }

        for command in take_native_viewport_pointer_passthrough_commands(ctx, viewport_id) {
            let status = match self.apply_window_input_state(viewport_id, window, command.enabled) {
                Ok(_) => NativeViewportPointerPassthroughStatus::Applied,
                Err(winit::error::ExternalError::NotSupported(_)) => {
                    NativeViewportPointerPassthroughStatus::Unsupported
                }
                Err(winit::error::ExternalError::Ignored | winit::error::ExternalError::Os(_)) => {
                    NativeViewportPointerPassthroughStatus::Failed
                }
            };
            self.notify_viewport_pointer_passthrough(
                ctx,
                NativeViewportPointerPassthroughResult {
                    token: command.token,
                    viewport_id,
                    window_id: window.id(),
                    enabled: command.enabled,
                    status,
                },
            );
        }
    }

    fn notify_viewport_focus(
        &self,
        ctx: &egui::Context,
        viewport_id: ViewportId,
        window_id: WindowId,
        status: NativeViewportFocusStatus,
    ) {
        let Some(inner) = &self.inner else {
            return;
        };
        if inner.handler.on_viewport_focus(NativeViewportFocusResult {
            viewport_id,
            window_id,
            status,
        }) == NativeHostWake::RepaintRoot
        {
            ctx.request_repaint_of(ViewportId::ROOT);
        }
    }

    fn notify_viewport_pointer_passthrough(
        &self,
        ctx: &egui::Context,
        result: NativeViewportPointerPassthroughResult,
    ) {
        let Some(inner) = &self.inner else {
            return;
        };
        if inner.handler.on_viewport_pointer_passthrough(result) == NativeHostWake::RepaintRoot {
            ctx.request_repaint_of(ViewportId::ROOT);
        }
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
        PresentedNativeOutputFrame {
            window_id,
            inner_size: snapshot
                .inner_rect()
                .map(|rect| (rect.width(), rect.height())),
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
    }

    fn forget_presented_viewport(&self, viewport_id: ViewportId) {
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

fn next_non_zero(counter: &AtomicU64, exhausted: &str) -> NonZeroU64 {
    let value = counter
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
            current.checked_add(1)
        })
        .unwrap_or_else(|_| panic!("{exhausted}"));
    NonZeroU64::new(value).unwrap_or_else(|| panic!("{exhausted}"))
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

        fn on_window_event(&self, event: NativeWindowEvent<'_>) {
            let facts = match event.event() {
                winit::event::WindowEvent::MouseInput { facts, .. }
                | winit::event::WindowEvent::MouseWheel { facts, .. } => *facts,
                winit::event::WindowEvent::CloseRequested => return,
                other => panic!("expected pointer event, got {other:?}"),
            };
            self.events.lock().push((
                event.ordinal().get(),
                event.window_id(),
                event.viewport_id(),
                facts,
            ));
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
        ) {
            self.output_begins.lock().push((
                token,
                window,
                root_roster.map(RecordedNativeViewportRoster::from),
            ));
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
            &press,
        );
        state.observe_device_removal(None, sequencer.next());
        state.observe_window_event(
            None,
            sequencer.next(),
            window,
            Some(ViewportId::ROOT),
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
            &WindowEvent::CloseRequested,
        );
        state.finish_viewport_close_request(&ctx, ViewportId::ROOT, false);
        assert!(host.viewport_close_cancellations.lock().is_empty());

        state.observe_window_event(
            None,
            sequencer.next(),
            child_window,
            Some(child_viewport),
            &WindowEvent::CloseRequested,
        );
        state.finish_viewport_close_request(&ctx, child_viewport, true);

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

        state.notify_viewport_focus(
            &egui::Context::default(),
            child_viewport,
            child_window,
            NativeViewportFocusStatus::AlreadyFocused,
        );

        let results = host.viewport_focus_results.lock();
        assert_eq!(results.len(), 1);
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
        let first_viewport = ViewportId::from_hash_of("pointer-passthrough-first");
        let second_viewport = ViewportId::from_hash_of("pointer-passthrough-second");
        let first = queue_native_viewport_pointer_passthrough(&context, first_viewport, true);
        let other = queue_native_viewport_pointer_passthrough(&context, second_viewport, true);
        let second = queue_native_viewport_pointer_passthrough(&context, first_viewport, false);

        let first_commands =
            take_native_viewport_pointer_passthrough_commands(&context, first_viewport);
        assert_eq!(
            first_commands.into_iter().collect::<Vec<_>>(),
            [
                QueuedNativeViewportPointerPassthroughCommand {
                    token: first,
                    enabled: true,
                },
                QueuedNativeViewportPointerPassthroughCommand {
                    token: second,
                    enabled: false,
                },
            ]
        );
        assert_eq!(
            take_native_viewport_pointer_passthrough_commands(&context, second_viewport)
                .into_iter()
                .collect::<Vec<_>>(),
            [QueuedNativeViewportPointerPassthroughCommand {
                token: other,
                enabled: true,
            }]
        );
        assert!(
            take_native_viewport_pointer_passthrough_commands(&context, first_viewport).is_empty()
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
        state.observe_window_event(None, sequencer.next(), first_window, None, &release);

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
            &press,
        );

        assert_eq!(
            *host.events.lock(),
            vec![
                (1, first_window, None, first_facts),
                (2, second_window, Some(second_viewport), second_facts),
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
            &event,
        );

        assert_eq!(repaint_count.load(Ordering::Relaxed), 1);
        assert_eq!(
            *host.events.lock(),
            vec![(1, child_window, Some(child_viewport), facts)]
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
        };
        let second_facts = PointerEventFacts {
            surface_position: Some(PhysicalPosition::new(6.0, 7.0)),
            desktop_position: Some(PhysicalPosition::new(306.0, 407.0)),
            modifiers: None,
            hover: PointerWindowRoute::Foreign,
            capture: PointerWindowRoute::Unknown,
        };

        let first = WindowEvent::MouseWheel {
            device_id: DeviceId::dummy(),
            delta: MouseScrollDelta::LineDelta(1.0, -2.0),
            phase: TouchPhase::Moved,
            facts: first_facts,
        };
        state.observe_window_event(None, sequencer.next(), first_window, None, &first);

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
            &second,
        );

        assert_eq!(
            *host.events.lock(),
            vec![
                (1, first_window, None, first_facts),
                (2, second_window, Some(second_viewport), second_facts),
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
            inner_rect: Some(NativePhysicalRect::new(0, 0, 800, 600)),
            outer_rect: None,
            native_scale_factor: 1.0,
            presentation_scale_factor: 1.0,
            visible: Some(true),
            minimized: Some(false),
            input_state: NativeWindowInputState::ReceivesInput,
        };
        let resized_snapshot = NativeWindowSnapshot {
            inner_rect: Some(NativePhysicalRect::new(0, 0, 801, 600)),
            ..initial_snapshot
        };

        state
            .begin_output(&ctx, viewport_id, window_id, None, initial_snapshot, None)
            .expect("the initial output scope exists")
            .finish()
            .present();

        let retained_same_size = state
            .begin_output(&ctx, viewport_id, window_id, None, initial_snapshot, None)
            .expect("the retained output scope exists");
        assert!(retain_current_native_output());
        retained_same_size.finish().present();

        let resized_output = state
            .begin_output(&ctx, viewport_id, window_id, None, resized_snapshot, None)
            .expect("the resized output scope exists");
        assert!(!retain_current_native_output());
        resized_output.finish().present();

        state.invalidate_presented_viewport(viewport_id);

        let invalidated_generation = state
            .begin_output(&ctx, viewport_id, window_id, None, resized_snapshot, None)
            .expect("the invalidated output scope exists");
        assert!(!retain_current_native_output());
        invalidated_generation.finish().present();
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
            inner_rect: Some(NativePhysicalRect::new(0, 0, 800, 600)),
            outer_rect: None,
            native_scale_factor: 1.0,
            presentation_scale_factor: 1.0,
            visible: Some(true),
            minimized: Some(false),
            input_state: NativeWindowInputState::ReceivesInput,
        };
        let resized_snapshot = NativeWindowSnapshot {
            inner_rect: Some(NativePhysicalRect::new(0, 0, 801, 600)),
            ..initial_snapshot
        };

        let first_frame = state
            .begin_output(&ctx, viewport_id, window_id, None, initial_snapshot, None)
            .expect("the first output scope exists");
        assert!(!retain_current_native_output_with_overlay());
        let first_frame = first_frame.finish();
        assert_eq!(first_frame.render_mode(), NativeOutputRenderMode::NoFrame);
        first_frame.present();

        let still_unavailable = state
            .begin_output(&ctx, viewport_id, window_id, None, initial_snapshot, None)
            .expect("the next no-frame output scope exists");
        assert!(!retain_current_native_output_with_overlay());
        still_unavailable.finish().present();

        state
            .begin_output(&ctx, viewport_id, window_id, None, initial_snapshot, None)
            .expect("the semantic output scope exists")
            .finish()
            .present();

        let overlay = state
            .begin_output(&ctx, viewport_id, window_id, None, initial_snapshot, None)
            .expect("the overlay output scope exists");
        assert!(retain_current_native_output_with_overlay());
        assert!(retain_current_native_output());
        let overlay = overlay.finish();
        assert_eq!(overlay.render_mode(), NativeOutputRenderMode::Overlay);
        overlay.present();

        let resized = state
            .begin_output(&ctx, viewport_id, window_id, None, resized_snapshot, None)
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
            inner_rect: Some(NativePhysicalRect::new(0, 0, 800, 600)),
            outer_rect: None,
            native_scale_factor: 1.0,
            presentation_scale_factor: 1.0,
            visible: Some(true),
            minimized: Some(false),
            input_state: NativeWindowInputState::ReceivesInput,
        };
        let resized_snapshot = NativeWindowSnapshot {
            inner_rect: Some(NativePhysicalRect::new(0, 0, 801, 600)),
            ..initial_snapshot
        };

        state
            .begin_output(&ctx, viewport_id, window_id, None, initial_snapshot, None)
            .expect("the semantic output scope exists")
            .finish()
            .present();

        let no_frame = state
            .begin_output(&ctx, viewport_id, window_id, None, resized_snapshot, None)
            .expect("the resized output scope exists");
        assert!(!retain_current_native_output_with_overlay());
        let no_frame = no_frame.finish();
        assert_eq!(no_frame.render_mode(), NativeOutputRenderMode::NoFrame);
        no_frame.present();

        let restored_size = state
            .begin_output(&ctx, viewport_id, window_id, None, initial_snapshot, None)
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
            inner_rect: Some(NativePhysicalRect::new(0, 0, 800, 600)),
            outer_rect: None,
            native_scale_factor: 1.0,
            presentation_scale_factor: 1.0,
            visible: Some(true),
            minimized: Some(false),
            input_state: NativeWindowInputState::ReceivesInput,
        };
        let resized_snapshot = NativeWindowSnapshot {
            inner_rect: Some(NativePhysicalRect::new(0, 0, 801, 600)),
            ..initial_snapshot
        };

        state
            .begin_output(&ctx, viewport_id, window_id, None, initial_snapshot, None)
            .expect("the semantic output scope exists")
            .finish()
            .present();

        let no_frame = state
            .begin_output(&ctx, viewport_id, window_id, None, resized_snapshot, None)
            .expect("the resized output scope exists");
        assert!(!retain_current_native_output_with_overlay());
        let no_frame = no_frame.finish();
        assert_eq!(no_frame.render_mode(), NativeOutputRenderMode::NoFrame);
        drop(no_frame);

        let restored_size = state
            .begin_output(&ctx, viewport_id, window_id, None, initial_snapshot, None)
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
            inner_rect: Some(NativePhysicalRect::new(0, 0, 800, 600)),
            outer_rect: None,
            native_scale_factor: 1.0,
            presentation_scale_factor: 1.0,
            visible: Some(true),
            minimized: Some(false),
            input_state: NativeWindowInputState::ReceivesInput,
        };
        let resized_snapshot = NativeWindowSnapshot {
            inner_rect: Some(NativePhysicalRect::new(0, 0, 801, 600)),
            ..initial_snapshot
        };

        state
            .begin_output(&ctx, viewport_id, window_id, None, initial_snapshot, None)
            .expect("the initial semantic output scope exists")
            .finish()
            .present();

        let predecessor = state
            .begin_output(&ctx, viewport_id, window_id, None, resized_snapshot, None)
            .expect("the no-frame predecessor scope exists");
        assert!(!retain_current_native_output_with_overlay());
        let predecessor = predecessor.finish();
        assert_eq!(predecessor.render_mode(), NativeOutputRenderMode::NoFrame);

        state
            .begin_output(&ctx, viewport_id, window_id, None, initial_snapshot, None)
            .expect("the semantic successor scope exists")
            .finish()
            .present();
        predecessor.present();

        let after_late_predecessor = state
            .begin_output(&ctx, viewport_id, window_id, None, initial_snapshot, None)
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
            inner_rect: Some(NativePhysicalRect::new(0, 0, 800, 600)),
            outer_rect: None,
            native_scale_factor: 1.0,
            presentation_scale_factor: 1.0,
            visible: Some(true),
            minimized: Some(false),
            input_state: NativeWindowInputState::ReceivesInput,
        };
        let resized_snapshot = NativeWindowSnapshot {
            inner_rect: Some(NativePhysicalRect::new(0, 0, 801, 600)),
            ..initial_snapshot
        };

        state
            .begin_output(&ctx, viewport_id, window_id, None, initial_snapshot, None)
            .expect("the initial semantic output scope exists")
            .finish()
            .present();

        let no_frame = state
            .begin_output(&ctx, viewport_id, window_id, None, resized_snapshot, None)
            .expect("the no-frame output scope exists");
        assert!(!retain_current_native_output_with_overlay());
        let no_frame = no_frame.finish();

        let overlay = state
            .begin_output(&ctx, viewport_id, window_id, None, initial_snapshot, None)
            .expect("the renderer-only successor scope exists");
        assert!(retain_current_native_output_with_overlay());
        let overlay = overlay.finish();

        no_frame.present();
        overlay.present();

        let after_overlay = state
            .begin_output(&ctx, viewport_id, window_id, None, initial_snapshot, None)
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
            inner_rect: Some(NativePhysicalRect::new(10, 20, 800, 600)),
            outer_rect: Some(NativePhysicalRect::new(2, -10, 816, 638)),
            native_scale_factor: 2.0,
            presentation_scale_factor: 2.5,
            visible: Some(true),
            minimized: Some(false),
            input_state: NativeWindowInputState::ReceivesInput,
        };
        let child_snapshot = NativeWindowSnapshot {
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
        let result = NativeViewportVisibilityResult {
            viewport_id: ViewportId::from_hash_of("shown-child"),
            window_id: WindowId::from(31),
            visible: true,
            status: NativeViewportVisibilityStatus::Dispatched,
        };

        state.notify_viewport_visibility_result(&ctx, result);

        assert_eq!(host.visibility_results.lock().as_slice(), &[result]);
        assert_eq!(repaint_count.load(Ordering::Relaxed), 1);
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

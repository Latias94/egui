//! Narrow native event and renderer-settlement seam.
//!
//! This module deliberately does not expose renderer resources, platform handles, or an
//! application transaction protocol. A host observes immutable native events before egui input
//! translation and receives terminal presentation results for context-local output tokens.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::num::NonZeroU64;
use std::sync::atomic::{AtomicU64, Ordering};
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

thread_local! {
    static ACTIVE_OUTPUTS: RefCell<Vec<NativeOutputToken>> = const { RefCell::new(Vec::new()) };
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

/// Terminal renderer disposition for one generated output.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum NativeOutputStatus {
    /// The renderer submitted and presented this viewport output.
    Presented,
    /// This viewport output was not presented.
    ///
    /// Texture commands remain owned by eframe's context-global texture batch and may be applied
    /// by a different viewport output.
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

    /// Returns the renderer's terminal disposition.
    pub const fn status(self) -> NativeOutputStatus {
        self.status
    }
}

/// Optional wake requested after a terminal host callback.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum NativeHostWake {
    /// Do not schedule an additional frame.
    #[default]
    Wait,
    /// Schedule the root viewport so queued host records can be reduced.
    RepaintRoot,
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
    ) -> Self {
        Self {
            viewport_id,
            window_id: window.id(),
            window: NativeWindowSnapshot::capture(egui_ctx, window),
        }
    }
}

/// Borrowed exact native-window roster attached to one root output callback.
#[derive(Clone, Copy, Debug)]
pub struct NativeViewportRoster<'a> {
    records: &'a [NativeViewportRecord],
    work_areas: NativeWorkAreaRoster<'a>,
}

impl<'a> NativeViewportRoster<'a> {
    /// Creates one complete root roster from exact native facts.
    pub(crate) const fn new(
        records: &'a [NativeViewportRecord],
        work_areas: NativeWorkAreaRoster<'a>,
    ) -> Self {
        Self {
            records,
            work_areas,
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
}

pub(crate) struct NativeViewportRosterCapture {
    records: Vec<NativeViewportRecord>,
    work_areas: OwnedNativeWorkAreaRoster,
}

impl NativeViewportRosterCapture {
    pub(crate) fn capture(
        event_loop: &ActiveEventLoop,
        records: Vec<NativeViewportRecord>,
    ) -> Self {
        Self {
            records,
            work_areas: OwnedNativeWorkAreaRoster::capture(event_loop),
        }
    }

    pub(crate) fn as_borrowed(&self) -> NativeViewportRoster<'_> {
        NativeViewportRoster::new(&self.records, self.work_areas.as_borrowed())
    }
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

    pub(crate) fn capture(egui_ctx: &egui::Context, window: &Window) -> Self {
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

/// Exact pointer pass-through command result for one native viewport.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NativeViewportPointerPassthroughResult {
    viewport_id: ViewportId,
    window_id: WindowId,
    enabled: bool,
    status: NativeViewportPointerPassthroughStatus,
}

impl NativeViewportPointerPassthroughResult {
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

/// External native coordinator callbacks.
///
/// Implementations must copy any retained facts during the callback. Borrowed winit values remain
/// owned by eframe. Immediate viewports are rejected while a handler is installed; use deferred
/// viewports so native side effects remain outside application UI recursion. Callbacks run on the
/// native event-loop thread and must not re-enter eframe.
pub trait NativeHostHandler: Send + Sync + 'static {
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
    ACTIVE_OUTPUTS.with(|outputs| outputs.borrow().last().copied())
}

#[derive(Clone, Default)]
pub(crate) struct NativeHostState {
    inner: Option<Arc<NativeHostStateInner>>,
}

struct NativeHostStateInner {
    handler: Arc<dyn NativeHostHandler>,
    context: NonZeroU64,
    next_token: AtomicU64,
    next_output: AtomicU64,
    next_create_attempt: AtomicU64,
    pending_viewport_closes: Mutex<BTreeMap<ViewportId, NativeViewportCloseRequest>>,
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
        Self {
            inner: Some(Arc::new(NativeHostStateInner {
                handler,
                context: next_non_zero(&NEXT_CONTEXT_ID, "native context identity exhausted"),
                next_token: AtomicU64::new(1),
                next_output: AtomicU64::new(1),
                next_create_attempt: AtomicU64::new(1),
                pending_viewport_closes: Mutex::new(BTreeMap::new()),
            })),
        }
    }

    pub(crate) fn is_enabled(&self) -> bool {
        self.inner.is_some()
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
                attempt.context == inner.context && attempt.viewport_id == viewport_id
            }),
            "native output attempt must belong to the producing context and viewport"
        );
        let token = NativeOutputToken {
            context: inner.context,
            nonce: next_non_zero(&inner.next_token, "native output token exhausted"),
            viewport_id,
            window_id,
            create_attempt,
        };
        inner.handler.on_output_begin(token, snapshot, root_roster);
        ACTIVE_OUTPUTS.with(|outputs| outputs.borrow_mut().push(token));
        Some(NativeOutputScope {
            inner,
            ctx: ctx.clone(),
            token,
            active: true,
        })
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
            },
            None,
        )
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
            context: inner.context,
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
            if self.is_enabled()
                && let egui::ViewportCommand::MousePassthrough(enabled) = command
            {
                let status = match window.set_cursor_hittest(!enabled) {
                    Ok(()) => NativeViewportPointerPassthroughStatus::Applied,
                    Err(winit::error::ExternalError::NotSupported(_)) => {
                        NativeViewportPointerPassthroughStatus::Unsupported
                    }
                    Err(
                        winit::error::ExternalError::Ignored | winit::error::ExternalError::Os(_),
                    ) => NativeViewportPointerPassthroughStatus::Failed,
                };
                self.notify_viewport_pointer_passthrough(
                    ctx,
                    NativeViewportPointerPassthroughResult {
                        viewport_id,
                        window_id: window.id(),
                        enabled,
                        status,
                    },
                );
                continue;
            }
            egui_winit::process_viewport_commands(
                ctx,
                info,
                std::iter::once(command),
                window,
                actions_requested,
            );
            if let Some(visible) = requested_visibility {
                self.notify_viewport_visibility(ctx, viewport_id, window, visible);
            }
            if let Some(status) = requested_focus {
                self.notify_viewport_focus(ctx, viewport_id, window.id(), status);
            }
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
    active: bool,
}

impl NativeOutputScope {
    pub(crate) fn finish(mut self) -> NativeOutputSettlement {
        self.leave();
        NativeOutputSettlement {
            inner: Arc::clone(&self.inner),
            ctx: self.ctx.clone(),
            token: self.token,
            ordinal: NativeOutputOrdinal(next_non_zero(
                &self.inner.next_output,
                "native output ordinal exhausted",
            )),
            settled: false,
        }
    }

    fn leave(&mut self) {
        if !self.active {
            return;
        }
        ACTIVE_OUTPUTS.with(|outputs| {
            let popped = outputs.borrow_mut().pop();
            debug_assert_eq!(
                popped,
                Some(self.token),
                "native output scopes must leave in stack order"
            );
        });
        self.active = false;
    }
}

impl Drop for NativeOutputScope {
    fn drop(&mut self) {
        if !self.active {
            return;
        }
        self.leave();
        if std::thread::panicking() {
            log::error!(
                "native output generation unwound; skipping the external settlement callback"
            );
            return;
        }
        let ordinal = NativeOutputOrdinal(next_non_zero(
            &self.inner.next_output,
            "native output ordinal exhausted",
        ));
        notify_output(
            &self.inner,
            &self.ctx,
            NativeOutputResult {
                token: self.token,
                ordinal,
                status: NativeOutputStatus::NotPresented,
            },
        );
    }
}

pub(crate) struct NativeOutputSettlement {
    inner: Arc<NativeHostStateInner>,
    ctx: egui::Context,
    token: NativeOutputToken,
    ordinal: NativeOutputOrdinal,
    settled: bool,
}

impl NativeOutputSettlement {
    pub(crate) fn present(mut self) {
        self.settle(NativeOutputStatus::Presented);
    }

    fn settle(&mut self, status: NativeOutputStatus) {
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
        );
    }
}

impl Drop for NativeOutputSettlement {
    fn drop(&mut self) {
        self.settle(NativeOutputStatus::NotPresented);
    }
}

fn notify_output(inner: &NativeHostStateInner, ctx: &egui::Context, result: NativeOutputResult) {
    if inner.handler.on_output(result) == NativeHostWake::RepaintRoot {
        ctx.request_repaint_of(ViewportId::ROOT);
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
        let child_viewport = ViewportId::from_hash_of("pointer-passthrough-child");
        let child_window = WindowId::from(23);

        state.notify_viewport_pointer_passthrough(
            &egui::Context::default(),
            NativeViewportPointerPassthroughResult {
                viewport_id: child_viewport,
                window_id: child_window,
                enabled: true,
                status: NativeViewportPointerPassthroughStatus::Applied,
            },
        );

        let results = host.viewport_pointer_passthrough_results.lock();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].viewport_id(), child_viewport);
        assert_eq!(results[0].window_id(), child_window);
        assert!(results[0].enabled());
        assert_eq!(
            results[0].status(),
            NativeViewportPointerPassthroughStatus::Applied
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
        };
        let child_snapshot = NativeWindowSnapshot {
            inner_rect: Some(NativePhysicalRect::new(900, 20, 640, 480)),
            outer_rect: None,
            native_scale_factor: 1.5,
            presentation_scale_factor: 1.25,
            visible: None,
            minimized: None,
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
                )),
            )
            .expect("root output scope exists");
        let token = current_native_output_token().expect("root token is active");

        let output_begins = host.output_begins.lock();
        assert_eq!(output_begins.len(), 1);
        assert_eq!(output_begins[0].0, token);
        assert_eq!(output_begins[0].1, root_snapshot);
        assert_eq!(
            output_begins[0].2,
            Some(RecordedNativeViewportRoster {
                records: roster.to_vec(),
                work_areas: Some(work_areas.to_vec()),
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
    fn output_settlement_is_idempotent() {
        let host = Arc::new(RecordingHost::default());
        let handler: Arc<dyn NativeHostHandler> = Arc::<RecordingHost>::clone(&host);
        let state = NativeHostState::new(Some(handler));
        let ctx = egui::Context::default();

        let mut settlement = state
            .begin_output_for_test(&ctx, ViewportId::ROOT, WindowId::from(11))
            .unwrap()
            .finish();
        settlement.settle(NativeOutputStatus::Presented);
        settlement.settle(NativeOutputStatus::NotPresented);

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

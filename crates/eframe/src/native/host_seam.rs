//! Narrow native event and renderer-settlement seam.
//!
//! This module deliberately does not expose renderer resources, platform handles, or an
//! application transaction protocol. A host observes immutable native events before egui input
//! translation and receives terminal presentation results for context-local output tokens.

use std::cell::RefCell;
use std::num::NonZeroU64;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use egui::ViewportId;
use winit::window::WindowId;

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

/// Opaque identity available while one viewport UI callback is producing an output.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NativeOutputToken {
    context: NonZeroU64,
    nonce: NonZeroU64,
    viewport_id: ViewportId,
    window_id: WindowId,
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

/// Terminal failure to create the native window for one deferred viewport.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct NativeViewportCreateFailure {
    viewport_id: ViewportId,
}

impl NativeViewportCreateFailure {
    /// Returns the deferred viewport whose native window could not be created.
    pub const fn viewport_id(self) -> ViewportId {
        self.viewport_id
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

/// External native coordinator callbacks.
///
/// Implementations must copy any retained facts during the callback. Borrowed winit values remain
/// owned by eframe. Immediate viewports are rejected while a handler is installed; use deferred
/// viewports so native side effects remain outside application UI recursion. Callbacks run on the
/// native event-loop thread and must not re-enter eframe.
pub trait NativeHostHandler: Send + Sync + 'static {
    /// Observes one window event before egui-winit translates it.
    fn on_window_event(&self, _event: NativeWindowEvent<'_>) {}

    /// Announces the output token before the viewport UI callback runs.
    ///
    /// Hosts may reserve the token here and attach the affine painted output
    /// after the core frame commits. The callback is observation-only and must
    /// not re-enter eframe.
    fn on_output_begin(&self, _token: NativeOutputToken) {}

    /// Receives one terminal output result and decides whether queued work needs another frame.
    fn on_output(&self, _result: NativeOutputResult) -> NativeHostWake {
        NativeHostWake::Wait
    }

    /// Reports that eframe could not create the native window for a deferred viewport.
    fn on_viewport_create_failed(&self, _failure: NativeViewportCreateFailure) -> NativeHostWake {
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

    pub(crate) fn begin_output(
        &self,
        ctx: &egui::Context,
        viewport_id: ViewportId,
        window_id: WindowId,
    ) -> Option<NativeOutputScope> {
        let inner = Arc::clone(self.inner.as_ref()?);
        let token = NativeOutputToken {
            context: inner.context,
            nonce: next_non_zero(&inner.next_token, "native output token exhausted"),
            viewport_id,
            window_id,
        };
        inner.handler.on_output_begin(token);
        ACTIVE_OUTPUTS.with(|outputs| outputs.borrow_mut().push(token));
        Some(NativeOutputScope {
            inner,
            ctx: ctx.clone(),
            token,
            active: true,
        })
    }

    pub(crate) fn notify_viewport_create_failed(
        &self,
        ctx: &egui::Context,
        viewport_id: ViewportId,
    ) {
        let Some(inner) = &self.inner else {
            return;
        };
        if inner
            .handler
            .on_viewport_create_failed(NativeViewportCreateFailure { viewport_id })
            == NativeHostWake::RepaintRoot
        {
            ctx.request_repaint_of(ViewportId::ROOT);
        }
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
        output_begins: Mutex<Vec<NativeOutputToken>>,
        outputs: Mutex<Vec<NativeOutputResult>>,
        create_failures: Mutex<Vec<NativeViewportCreateFailure>>,
        wake: NativeHostWake,
    }

    impl NativeHostHandler for RecordingHost {
        fn on_window_event(&self, event: NativeWindowEvent<'_>) {
            let facts = match event.event() {
                winit::event::WindowEvent::MouseInput { facts, .. }
                | winit::event::WindowEvent::MouseWheel { facts, .. } => *facts,
                other => panic!("expected pointer event, got {other:?}"),
            };
            self.events.lock().push((
                event.ordinal().get(),
                event.window_id(),
                event.viewport_id(),
                facts,
            ));
        }

        fn on_output(&self, result: NativeOutputResult) -> NativeHostWake {
            self.outputs.lock().push(result);
            self.wake
        }

        fn on_output_begin(&self, token: NativeOutputToken) {
            self.output_begins.lock().push(token);
        }

        fn on_viewport_create_failed(
            &self,
            failure: NativeViewportCreateFailure,
        ) -> NativeHostWake {
            self.create_failures.lock().push(failure);
            self.wake
        }
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
            .begin_output(&ctx, ViewportId::ROOT, WindowId::from(11))
            .unwrap();
        let outer_token = current_native_output_token().unwrap();
        let child_id = ViewportId::from_hash_of("child");
        let child = state
            .begin_output(&ctx, child_id, WindowId::from(22))
            .unwrap();
        let child_token = current_native_output_token().unwrap();

        child.finish().present();
        assert_eq!(current_native_output_token(), Some(outer_token));
        drop(outer.finish());
        assert_eq!(current_native_output_token(), None);

        let outputs = host.outputs.lock();
        assert_eq!(*host.output_begins.lock(), vec![outer_token, child_token]);
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
            .begin_output(&ctx, ViewportId::ROOT, WindowId::from(11))
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
            .begin_output(&ctx, ViewportId::ROOT, WindowId::from(11))
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
            .begin_output(&egui::Context::default(), ViewportId::ROOT, window)
            .unwrap();
        let token = current_native_output_token().expect("output scope publishes its token");

        assert_eq!(token.window_id(), window);
        scope.finish().present();
    }

    #[test]
    fn output_tokens_expose_only_context_equality() {
        let first_host = Arc::new(RecordingHost::default());
        let first_handler: Arc<dyn NativeHostHandler> = Arc::<RecordingHost>::clone(&first_host);
        let first_state = NativeHostState::new(Some(first_handler));
        let context = egui::Context::default();
        let first = first_state
            .begin_output(&context, ViewportId::ROOT, WindowId::from(11))
            .expect("first scope exists");
        let first_token = current_native_output_token().expect("first token is active");
        let second = first_state
            .begin_output(
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
            .begin_output(
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

        state.notify_viewport_create_failed(&ctx, viewport_id);

        assert_eq!(host.create_failures.lock()[0].viewport_id(), viewport_id);
        assert_eq!(repaint_count.load(Ordering::Relaxed), 1);
    }

    #[test]
    #[should_panic(expected = "show_viewport_immediate is unavailable")]
    fn immediate_viewports_are_rejected_with_a_precise_error() {
        let host = Arc::new(RecordingHost::default());
        let handler: Arc<dyn NativeHostHandler> = Arc::<RecordingHost>::clone(&host);
        NativeHostState::new(Some(handler)).assert_immediate_viewports_supported();
    }
}

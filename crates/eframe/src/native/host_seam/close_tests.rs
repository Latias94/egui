use super::*;
use winit::event::WindowEvent;

#[derive(Default)]
struct CloseHost {
    requests: Mutex<Vec<NativeViewportCloseRequest>>,
    cancellations: Mutex<Vec<NativeViewportCloseRequest>>,
    continue_events: AtomicBool,
    foreign_hold: Mutex<Option<NativeViewportCloseRequest>>,
}

impl NativeHostHandler for CloseHost {
    fn on_window_event(&self, event: NativeWindowEvent<'_>) -> NativeWindowEventDisposition {
        let Some(request) = event.close_request() else {
            return NativeWindowEventDisposition::Continue;
        };
        self.requests.lock().unwrap().push(request);
        if let Some(foreign) = *self.foreign_hold.lock().unwrap() {
            NativeWindowEventDisposition::HoldClose(foreign)
        } else if self.continue_events.load(Ordering::Relaxed) {
            NativeWindowEventDisposition::Continue
        } else {
            NativeWindowEventDisposition::HoldClose(request)
        }
    }

    fn on_viewport_close_cancelled(&self, request: NativeViewportCloseRequest) -> NativeHostWake {
        self.cancellations.lock().unwrap().push(request);
        NativeHostWake::RepaintRoot
    }
}

fn fixture() -> (
    Arc<CloseHost>,
    NativeHostState,
    egui::Context,
    NativeEventSequencer,
) {
    let host = Arc::new(CloseHost::default());
    let handler: Arc<dyn NativeHostHandler> = host.clone();
    (
        host,
        NativeHostState::new(Some(handler)),
        egui::Context::default(),
        NativeEventSequencer::default(),
    )
}

fn held_close(
    state: &NativeHostState,
    ctx: &egui::Context,
    sequence: &mut NativeEventSequencer,
    viewport: ViewportId,
    window: WindowId,
) -> NativeViewportCloseRequest {
    let disposition = state.observe_window_event(
        Some(ctx),
        sequence.next(),
        window,
        Some(viewport),
        None,
        &WindowEvent::CloseRequested,
    );
    let NativeWindowEventDisposition::HoldClose(request) = disposition else {
        panic!("the exact native close must be held before backend translation");
    };
    request
}

#[test]
fn held_root_crosses_updates_without_cancellation_then_allows_once() {
    let (host, state, ctx, mut sequence) = fixture();
    let window = WindowId::from(10);
    let request = held_close(&state, &ctx, &mut sequence, ViewportId::ROOT, window);
    for _ in 0..3 {
        assert!(!state.settle_close_resolutions(&ctx));
        state.invalidate_presented_viewport(ViewportId::ROOT);
        state.finish_viewport_close_request(&ctx, ViewportId::ROOT, window, true);
        assert!(state.is_close_held(ViewportId::ROOT));
        assert!(host.cancellations.lock().unwrap().is_empty());
    }
    assert!(resolve_native_viewport_close(
        &ctx,
        request,
        NativeViewportCloseDecision::Allow
    ));
    assert!(resolve_native_viewport_close(
        &ctx,
        request,
        NativeViewportCloseDecision::Allow
    ));
    assert!(!resolve_native_viewport_close(
        &ctx,
        request,
        NativeViewportCloseDecision::Veto
    ));
    assert!(state.settle_close_resolutions(&ctx));
    assert!(!state.is_close_held(ViewportId::ROOT));
    assert!(!state.settle_close_resolutions(&ctx));
    assert!(!resolve_native_viewport_close(
        &ctx,
        request,
        NativeViewportCloseDecision::Allow
    ));
    assert!(host.cancellations.lock().unwrap().is_empty());
}

#[test]
fn duplicate_os_close_keeps_the_original_held_identity_even_if_handler_continues() {
    let (host, state, ctx, mut sequence) = fixture();
    let window = WindowId::from(10);
    let original = held_close(&state, &ctx, &mut sequence, ViewportId::ROOT, window);
    host.continue_events.store(true, Ordering::Relaxed);
    let repeated = held_close(&state, &ctx, &mut sequence, ViewportId::ROOT, window);
    assert_eq!(original, repeated);
    assert_eq!(*host.requests.lock().unwrap(), [original, original]);
    assert_eq!(repeated.event().get(), 1);
    assert!(resolve_native_viewport_close(
        &ctx,
        original,
        NativeViewportCloseDecision::Veto
    ));
    assert!(!state.settle_close_resolutions(&ctx));
    assert_eq!(*host.cancellations.lock().unwrap(), [original]);
    host.continue_events.store(false, Ordering::Relaxed);
    let successor = held_close(&state, &ctx, &mut sequence, ViewportId::ROOT, window);
    assert_ne!(successor, original);
    assert_eq!(successor.event().get(), 3);
    assert!(!resolve_native_viewport_close(
        &ctx,
        original,
        NativeViewportCloseDecision::Allow
    ));
}

#[test]
fn child_veto_reports_original_request_and_child_allow_never_closes_root() {
    let (host, state, ctx, mut sequence) = fixture();
    let child = ViewportId::from_hash_of("held-child");
    let window = WindowId::from(20);
    let vetoed = held_close(&state, &ctx, &mut sequence, child, window);
    assert!(resolve_native_viewport_close(
        &ctx,
        vetoed,
        NativeViewportCloseDecision::Veto
    ));
    assert!(!state.settle_close_resolutions(&ctx));
    assert_eq!(*host.cancellations.lock().unwrap(), [vetoed]);
    assert!(!state.settle_close_resolutions(&ctx));
    assert_eq!(host.cancellations.lock().unwrap().len(), 1);
    let allowed = held_close(&state, &ctx, &mut sequence, child, window);
    assert!(resolve_native_viewport_close(
        &ctx,
        allowed,
        NativeViewportCloseDecision::Allow
    ));
    assert!(!state.settle_close_resolutions(&ctx));
    assert!(!state.is_close_held(child));
    assert_eq!(*host.cancellations.lock().unwrap(), [vetoed]);
}

#[test]
fn context_and_callback_hold_authority_cannot_cross_attachments() {
    let (_, first, first_ctx, mut first_sequence) = fixture();
    let first_request = held_close(
        &first,
        &first_ctx,
        &mut first_sequence,
        ViewportId::ROOT,
        WindowId::from(10),
    );
    let (second_host, second, second_ctx, mut second_sequence) = fixture();
    let second_request = held_close(
        &second,
        &second_ctx,
        &mut second_sequence,
        ViewportId::ROOT,
        WindowId::from(10),
    );
    assert_ne!(first_request, second_request);
    assert!(!resolve_native_viewport_close(
        &second_ctx,
        first_request,
        NativeViewportCloseDecision::Allow
    ));
    assert!(!resolve_native_viewport_close(
        &first_ctx,
        second_request,
        NativeViewportCloseDecision::Allow
    ));
    assert!(first.is_close_held(ViewportId::ROOT));
    assert!(second.is_close_held(ViewportId::ROOT));

    let child = ViewportId::from_hash_of("foreign-held-child");
    *second_host.foreign_hold.lock().unwrap() = Some(first_request);
    assert_eq!(
        second.observe_window_event(
            Some(&second_ctx),
            second_sequence.next(),
            WindowId::from(20),
            Some(child),
            None,
            &WindowEvent::CloseRequested
        ),
        NativeWindowEventDisposition::Continue
    );
    assert!(!second.is_close_held(child));
    let unmanaged = *second_host.requests.lock().unwrap().last().unwrap();
    assert!(!resolve_native_viewport_close(
        &second_ctx,
        unmanaged,
        NativeViewportCloseDecision::Veto
    ));
    second.finish_viewport_close_request(&second_ctx, child, WindowId::from(99), true);
    assert!(second_host.cancellations.lock().unwrap().is_empty());
    second.finish_viewport_close_request(&second_ctx, child, WindowId::from(20), true);
    assert_eq!(*second_host.cancellations.lock().unwrap(), [unmanaged]);
}

#[test]
fn replacement_destroyed_and_detachment_revoke_only_exact_requests() {
    let (host, state, ctx, mut sequence) = fixture();
    let viewport = ViewportId::ROOT;
    let old_window = WindowId::from(10);
    let successor_window = WindowId::from(11);
    let old = held_close(&state, &ctx, &mut sequence, viewport, old_window);
    assert!(resolve_native_viewport_close(
        &ctx,
        old,
        NativeViewportCloseDecision::Allow
    ));
    let successor = held_close(&state, &ctx, &mut sequence, viewport, successor_window);
    state.forget_presented_window(viewport, old_window);
    state.observe_window_event(
        Some(&ctx),
        sequence.next(),
        old_window,
        Some(viewport),
        None,
        &WindowEvent::Destroyed,
    );
    assert!(state.is_close_held(viewport));
    assert!(!state.settle_close_resolutions(&ctx));
    assert!(!resolve_native_viewport_close(
        &ctx,
        old,
        NativeViewportCloseDecision::Allow
    ));
    assert!(resolve_native_viewport_close(
        &ctx,
        successor,
        NativeViewportCloseDecision::Veto
    ));
    assert_eq!(
        state.observe_window_event(
            Some(&ctx),
            sequence.next(),
            successor_window,
            Some(viewport),
            None,
            &WindowEvent::Destroyed
        ),
        NativeWindowEventDisposition::Continue
    );
    assert!(!state.settle_close_resolutions(&ctx));
    assert!(host.cancellations.lock().unwrap().is_empty());
    assert!(!resolve_native_viewport_close(
        &ctx,
        successor,
        NativeViewportCloseDecision::Veto
    ));

    let child = ViewportId::from_hash_of("retired-held-child");
    let retired = held_close(&state, &ctx, &mut sequence, child, WindowId::from(20));
    state.forget_presented_viewport(child);
    assert!(!resolve_native_viewport_close(
        &ctx,
        retired,
        NativeViewportCloseDecision::Veto
    ));
    let detached = held_close(&state, &ctx, &mut sequence, viewport, WindowId::from(30));
    drop(state);
    assert!(!resolve_native_viewport_close(
        &ctx,
        detached,
        NativeViewportCloseDecision::Allow
    ));
}

#[test]
fn held_raw_input_is_filtered_and_logic_only_context_can_schedule_a_decision() {
    let (host, state, ctx, mut sequence) = fixture();
    let window = WindowId::from(10);
    let disposition = state.observe_window_event(
        None,
        sequence.next(),
        window,
        Some(ViewportId::ROOT),
        None,
        &WindowEvent::CloseRequested,
    );
    let NativeWindowEventDisposition::HoldClose(request) = disposition else {
        panic!("the close must hold without a context");
    };
    assert!(!resolve_native_viewport_close(
        &ctx,
        request,
        NativeViewportCloseDecision::Veto
    ));
    let mut raw = egui::RawInput::default();
    raw.viewports
        .get_mut(&ViewportId::ROOT)
        .unwrap()
        .events
        .push(egui::ViewportEvent::Close);
    state.bind_close_context(&ctx);
    state.prepare_close_input(&mut raw);
    assert!(!raw.viewport().close_requested());
    assert!(resolve_native_viewport_close(
        &ctx,
        request,
        NativeViewportCloseDecision::Veto
    ));
    assert!(!state.settle_close_resolutions(&ctx));
    assert_eq!(*host.cancellations.lock().unwrap(), [request]);
}

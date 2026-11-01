use super::*;

// Winit exposes no safe constructor for a press token. Abstract identities exercise the actual
// prepared command and queue without forging a native token or entering the platform adapter.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum PressIdentity {
    Native(PointerPressToken),
    Abstract(u64),
}

impl From<PointerPressToken> for PressIdentity {
    fn from(press: PointerPressToken) -> Self {
        Self::Native(press)
    }
}

impl std::ops::Deref for PressIdentity {
    type Target = PointerPressToken;

    fn deref(&self) -> &Self::Target {
        match self {
            Self::Native(press) => press,
            Self::Abstract(_) => panic!("abstract presses must not enter the native backend"),
        }
    }
}

#[derive(Default)]
struct CaptureHost {
    results: egui::mutex::Mutex<Vec<NativeViewportPropertyResult>>,
    retirements: egui::mutex::Mutex<Vec<NativeViewportPropertyCommandRetirement>>,
    requeue: egui::mutex::Mutex<
        Option<(
            egui::Context,
            PreparedNativeViewportCommand<NativeViewportPointerCaptureCommandToken>,
        )>,
    >,
}

impl CaptureHost {
    fn reenqueue(&self) {
        let pending = self.requeue.lock().take();
        if let Some((ctx, command)) = pending {
            command.enqueue(&ctx);
        }
    }
}

impl NativeHostHandler for CaptureHost {
    fn on_viewport_focus(&self, result: NativeViewportFocusResult) -> NativeHostWake {
        self.results
            .lock()
            .push(NativeViewportPropertyResult::Focus(result));
        NativeHostWake::Wait
    }

    fn on_viewport_pointer_capture(
        &self,
        result: NativeViewportPointerCaptureResult,
    ) -> NativeHostWake {
        self.results
            .lock()
            .push(NativeViewportPropertyResult::PointerCapture(result));
        self.reenqueue();
        NativeHostWake::RepaintRootAfterCurrent
    }

    fn on_viewport_property_command_retired(
        &self,
        retirement: NativeViewportPropertyCommandRetirement,
    ) -> NativeHostWake {
        self.retirements.lock().push(retirement);
        self.reenqueue();
        NativeHostWake::RepaintRootAfterCurrent
    }
}

fn fixture() -> (Arc<CaptureHost>, NativeHostState, egui::Context) {
    let host = Arc::new(CaptureHost::default());
    let handler: Arc<dyn NativeHostHandler> = host.clone();
    let state = NativeHostState::new(Some(handler));
    let ctx = egui::Context::default();
    state.bind_close_context(&ctx);
    (host, state, ctx)
}

fn prepare(
    window: WindowId,
    press: PressIdentity,
    action: NativeViewportPointerCaptureAction,
) -> PreparedNativeViewportCommand<NativeViewportPointerCaptureCommandToken> {
    prepare_native_viewport_pointer_capture_with(
        ViewportId::ROOT,
        window,
        DeviceId::dummy(),
        press,
        action,
    )
}

fn dispatch(
    state: &NativeHostState,
    ctx: &egui::Context,
    actual_window: WindowId,
    event_loop: Option<&()>,
    mut execute: impl FnMut(
        WindowId,
        DeviceId,
        &PressIdentity,
        NativeViewportPointerCaptureAction,
    ) -> NativeViewportPointerCaptureStatus,
) {
    state.process_viewport_commands_with(
        ctx,
        ViewportId::ROOT,
        [],
        |_| panic!("no ordinary commands were queued"),
        |command| match command {
            QueuedNativeViewportPropertyCommand::PointerCapture(command) => {
                NativeViewportPropertyResult::PointerCapture(command.execute_with(
                    ViewportId::ROOT,
                    actual_window,
                    event_loop,
                    |(), window, device, press, action| execute(window, device, press, action),
                ))
            }
            QueuedNativeViewportPropertyCommand::Focus(token) => {
                NativeViewportPropertyResult::Focus(NativeViewportFocusResult {
                    token,
                    viewport_id: ViewportId::ROOT,
                    window_id: actual_window,
                    status: NativeViewportFocusStatus::Requested,
                })
            }
            _ => panic!("only capture and focus commands belong to this fixture"),
        },
    );
}

#[test]
fn preparation_and_mixed_fifo_preserve_original_press_action_and_single_pending_result() {
    let (host, state, ctx) = fixture();
    let window = WindowId::from(241);
    let mut current_press = PressIdentity::Abstract(1);
    let acquire = prepare(
        window,
        current_press.clone(),
        NativeViewportPointerCaptureAction::Acquire,
    );
    let acquire_token = acquire.token();
    let rejected = prepare(
        window,
        current_press.clone(),
        NativeViewportPointerCaptureAction::Release,
    );
    assert_ne!(rejected.token(), acquire_token);
    drop(rejected);
    assert!(
        state
            .take_viewport_property_commands(ViewportId::ROOT)
            .is_empty()
    );
    assert!(host.results.lock().is_empty());
    assert!(host.retirements.lock().is_empty());

    acquire.enqueue(&ctx);
    let focus = queue_native_viewport_focus(&ctx, ViewportId::ROOT);
    let release = prepare(
        window,
        current_press.clone(),
        NativeViewportPointerCaptureAction::Release,
    );
    let release_token = release.token();
    release.enqueue(&ctx);
    current_press = PressIdentity::Abstract(2);
    let mut calls = 0;
    dispatch(
        &state,
        &ctx,
        window,
        Some(&()),
        |expected, device, press, action| {
            assert_eq!((expected, device), (window, DeviceId::dummy()));
            assert_eq!(*press, PressIdentity::Abstract(1));
            assert_ne!(*press, current_press);
            let status = match calls {
                0 => {
                    assert_eq!(action, NativeViewportPointerCaptureAction::Acquire);
                    assert!(host.results.lock().is_empty());
                    NativeViewportPointerCaptureStatus::Pending
                }
                1 => {
                    assert_eq!(action, NativeViewportPointerCaptureAction::Release);
                    assert_eq!(host.results.lock().len(), 2);
                    NativeViewportPointerCaptureStatus::Released
                }
                _ => panic!("a command must invoke the backend only once"),
            };
            calls += 1;
            status
        },
    );
    assert_eq!(calls, 2);
    let results = host.results.lock().clone();
    let [
        NativeViewportPropertyResult::PointerCapture(acquired),
        NativeViewportPropertyResult::Focus(focused),
        NativeViewportPropertyResult::PointerCapture(released),
    ] = results.as_slice()
    else {
        panic!("mixed typed commands must callback in FIFO order");
    };
    assert_eq!(focused.token(), focus);
    for (result, token, action, status) in [
        (
            *acquired,
            acquire_token,
            NativeViewportPointerCaptureAction::Acquire,
            NativeViewportPointerCaptureStatus::Pending,
        ),
        (
            *released,
            release_token,
            NativeViewportPointerCaptureAction::Release,
            NativeViewportPointerCaptureStatus::Released,
        ),
    ] {
        assert_eq!(result.token(), token);
        assert_eq!(result.viewport_id(), ViewportId::ROOT);
        assert_eq!(result.expected_window_id(), window);
        assert_eq!(result.device_id(), DeviceId::dummy());
        assert_eq!(result.action(), action);
        assert_eq!(result.status(), status);
    }
    dispatch(&state, &ctx, window, Some(&()), |_, _, _, _| {
        panic!("Pending must not retry itself")
    });
    state.forget_presented_viewport(ViewportId::ROOT);
    assert_eq!(host.results.lock().len(), 3);
    assert!(host.retirements.lock().is_empty());
}

#[test]
fn missing_loop_and_window_mismatch_never_invoke_either_capture_action() {
    for action in [
        NativeViewportPointerCaptureAction::Acquire,
        NativeViewportPointerCaptureAction::Release,
    ] {
        for (actual, event_loop) in [
            (WindowId::from(242), Some(&())),
            (WindowId::from(241), None),
        ] {
            let (host, state, ctx) = fixture();
            let expected = WindowId::from(241);
            let command = prepare(expected, PressIdentity::Abstract(1), action);
            let token = command.token();
            command.enqueue(&ctx);
            dispatch(&state, &ctx, actual, event_loop, |_, _, _, _| {
                panic!("a skipped operation must not invoke the native function")
            });
            let results = host.results.lock();
            let [NativeViewportPropertyResult::PointerCapture(result)] = results.as_slice() else {
                panic!("each command must produce exactly one result");
            };
            assert_eq!(result.token(), token);
            assert_eq!(result.expected_window_id(), expected);
            assert_eq!(result.action(), action);
            assert_eq!(
                result.status(),
                NativeViewportPointerCaptureStatus::NotExecuted
            );
            drop(results);
            state.forget_presented_viewport(ViewportId::ROOT);
            assert!(host.retirements.lock().is_empty());
        }
    }
}

#[test]
fn retirement_and_result_callbacks_can_reenqueue_without_retiring_successors() {
    let (host, state, ctx) = fixture();
    let viewport = ViewportId::ROOT;
    let original = WindowId::from(241);
    let replacement = WindowId::from(242);
    let inner = state.inner.as_ref().unwrap();
    inner.capture_window_input_state(viewport, original);
    let command = prepare(
        original,
        PressIdentity::Abstract(1),
        NativeViewportPointerCaptureAction::Acquire,
    );
    let retired = command.token();
    command.enqueue(&ctx);
    let successor = prepare(
        replacement,
        PressIdentity::Abstract(2),
        NativeViewportPointerCaptureAction::Release,
    );
    let successor_token = successor.token();
    *host.requeue.lock() = Some((ctx.clone(), successor));
    state.forget_presented_window(viewport, original);
    assert_eq!(
        *host.retirements.lock(),
        [NativeViewportPropertyCommandRetirement::PointerCapture {
            viewport_id: viewport,
            token: retired,
            expected_window_id: original,
            device_id: DeviceId::dummy(),
            action: NativeViewportPointerCaptureAction::Acquire,
        }]
    );
    inner.capture_window_input_state(viewport, replacement);
    state.forget_presented_window(viewport, original);
    state.forget_presented_window(viewport, original);
    let followup = prepare(
        replacement,
        PressIdentity::Abstract(2),
        NativeViewportPointerCaptureAction::Release,
    );
    let followup_token = followup.token();
    *host.requeue.lock() = Some((ctx.clone(), followup));
    dispatch(
        &state,
        &ctx,
        replacement,
        Some(&()),
        |_, _, press, action| {
            assert_eq!(*press, PressIdentity::Abstract(2));
            assert_eq!(action, NativeViewportPointerCaptureAction::Release);
            NativeViewportPointerCaptureStatus::Pending
        },
    );
    assert_eq!(host.results.lock().len(), 1);
    assert!(
        state
            .with_pending_viewport_property_targets(Default::default())
            .contains_key(&viewport)
    );
    dispatch(&state, &ctx, replacement, Some(&()), |_, _, _, _| {
        NativeViewportPointerCaptureStatus::Released
    });
    let results = host.results.lock();
    let [
        NativeViewportPropertyResult::PointerCapture(first),
        NativeViewportPropertyResult::PointerCapture(second),
    ] = results.as_slice()
    else {
        panic!("each detached queue must callback once");
    };
    assert_eq!(first.token(), successor_token);
    assert_eq!(second.token(), followup_token);
    drop(results);
    state.forget_presented_viewport(viewport);
    assert_eq!(host.retirements.lock().len(), 1);
    assert!(state.take_root_wake_after_current());
    assert!(!state.take_root_wake_after_current());
}

use super::*;

#[derive(Clone, Debug, PartialEq, Eq)]
enum Trace {
    Ordinary(egui::ViewportCommand),
    Applied(QueuedNativeViewportPropertyCommand),
    Callback(NativeViewportPropertyResult),
}

#[derive(Default)]
struct PropertyHost {
    trace: egui::mutex::Mutex<Vec<Trace>>,
}

impl NativeHostHandler for PropertyHost {
    fn on_viewport_focus(&self, result: NativeViewportFocusResult) -> NativeHostWake {
        self.trace
            .lock()
            .push(Trace::Callback(NativeViewportPropertyResult::Focus(result)));
        NativeHostWake::Wait
    }

    fn on_viewport_visibility(&self, result: NativeViewportVisibilityResult) -> NativeHostWake {
        self.trace
            .lock()
            .push(Trace::Callback(NativeViewportPropertyResult::Visibility(
                result,
            )));
        NativeHostWake::Wait
    }

    fn on_viewport_pointer_passthrough(
        &self,
        result: NativeViewportPointerPassthroughResult,
    ) -> NativeHostWake {
        self.trace.lock().push(Trace::Callback(
            NativeViewportPropertyResult::PointerPassthrough(result),
        ));
        NativeHostWake::Wait
    }
}

fn fixture() -> (Arc<PropertyHost>, NativeHostState, egui::Context) {
    let host = Arc::new(PropertyHost::default());
    let handler: Arc<dyn NativeHostHandler> = host.clone();
    let state = NativeHostState::new(Some(handler));
    let ctx = egui::Context::default();
    state.bind_close_context(&ctx);
    (host, state, ctx)
}

// Stub only the OS operations. Tokens always come from the public queue functions, and ordinary
// dispatch, queue consumption, and host callbacks use the same boundary as Glow and WGPU.
fn dispatch(
    host: &PropertyHost,
    state: &NativeHostState,
    ctx: &egui::Context,
    viewport_id: ViewportId,
    ordinary: impl IntoIterator<Item = egui::ViewportCommand>,
) {
    state.process_viewport_commands_with(
        ctx,
        viewport_id,
        ordinary,
        |command| host.trace.lock().push(Trace::Ordinary(command)),
        |command| apply_stub(host, viewport_id, command),
    );
}

fn apply_stub(
    host: &PropertyHost,
    viewport_id: ViewportId,
    command: QueuedNativeViewportPropertyCommand,
) -> NativeViewportPropertyResult {
    let window_id = WindowId::from(41);
    host.trace.lock().push(Trace::Applied(command));
    match command {
        QueuedNativeViewportPropertyCommand::Focus(token) => {
            NativeViewportPropertyResult::Focus(NativeViewportFocusResult {
                token,
                viewport_id,
                window_id,
                status: NativeViewportFocusStatus::Requested,
            })
        }
        QueuedNativeViewportPropertyCommand::Visibility { token, visible } => {
            NativeViewportPropertyResult::Visibility(NativeViewportVisibilityResult {
                token,
                viewport_id,
                window_id,
                visible,
                status: NativeViewportVisibilityStatus::Unsupported,
            })
        }
        QueuedNativeViewportPropertyCommand::PointerPassthrough { token, enabled } => {
            NativeViewportPropertyResult::PointerPassthrough(
                NativeViewportPointerPassthroughResult {
                    token,
                    viewport_id,
                    window_id,
                    enabled,
                    status: NativeViewportPointerPassthroughStatus::Applied,
                },
            )
        }
    }
}

#[test]
fn ordinary_same_value_and_builder_commands_never_claim_typed_tokens() {
    let (host, state, ctx) = fixture();
    let viewport = ViewportId::ROOT;
    let focus = queue_native_viewport_focus(&ctx, viewport);
    let visibility = queue_native_viewport_visibility(&ctx, viewport, true);
    let logic_commands = ctx
        .run_logic(&egui::RawInput::default(), |_| {})
        .viewport_commands;
    assert!(
        logic_commands.is_empty(),
        "typed queues must not emit ordinary companions"
    );
    let mut commands = state
        .with_pending_viewport_property_targets(logic_commands)
        .remove(&viewport)
        .expect("typed targets must participate even in hidden-window logic");
    commands.extend([
        egui::ViewportCommand::Focus,
        egui::ViewportCommand::Visible(true),
    ]);
    let (builder_commands, _) = ViewportBuilder::default()
        .with_visible(false)
        .patch(ViewportBuilder::default().with_visible(true));
    commands.extend(builder_commands);
    let ordinary_count = commands.len();

    state.process_viewport_commands_with(
        &ctx,
        viewport,
        commands,
        |command| {
            assert!(
                host.trace
                    .lock()
                    .iter()
                    .all(|entry| matches!(entry, Trace::Ordinary(_)))
            );
            assert_eq!(
                state
                    .inner
                    .as_ref()
                    .unwrap()
                    .viewport_property_commands
                    .lock()
                    .0[&viewport]
                    .len(),
                2,
                "ordinary same-value commands must leave both typed commands pending"
            );
            host.trace.lock().push(Trace::Ordinary(command));
        },
        |command| {
            let expected = match command {
                QueuedNativeViewportPropertyCommand::Focus(token) => {
                    assert_eq!(token, focus);
                    ordinary_count
                }
                QueuedNativeViewportPropertyCommand::Visibility { token, visible } => {
                    assert_eq!(token, visibility);
                    assert!(visible);
                    ordinary_count + 2
                }
                _ => panic!("only the two explicitly queued operations may execute"),
            };
            assert_eq!(host.trace.lock().len(), expected);
            apply_stub(&host, viewport, command)
        },
    );

    assert_eq!(host.trace.lock().len(), ordinary_count + 4);
    assert!(state.take_viewport_property_commands(viewport).is_empty());
    dispatch(
        &host,
        &state,
        &ctx,
        viewport,
        [
            egui::ViewportCommand::Focus,
            egui::ViewportCommand::Visible(true),
        ],
    );
    assert_eq!(host.trace.lock().len(), ordinary_count + 6);
}

#[test]
fn typed_properties_execute_and_callback_in_one_per_viewport_order() {
    let (host, state, ctx) = fixture();
    let viewport = ViewportId::from_hash_of("ordered-properties");
    let other_viewport = ViewportId::from_hash_of("isolated-properties");
    let show = queue_native_viewport_visibility(&ctx, viewport, true);
    let focus = queue_native_viewport_focus(&ctx, viewport);
    let other = queue_native_viewport_focus(&ctx, other_viewport);
    let pointer = queue_native_viewport_pointer_passthrough(&ctx, viewport, true);
    let hide = queue_native_viewport_visibility(&ctx, viewport, false);
    let refocus = queue_native_viewport_focus(&ctx, viewport);
    assert_ne!(focus, refocus);
    assert_ne!(show, hide);

    let logic_commands = ctx
        .run_logic(&egui::RawInput::default(), |_| {})
        .viewport_commands;
    assert!(logic_commands.is_empty());
    assert!(ctx.has_requested_repaint_for(&ViewportId::ROOT));
    let mut targets = state.with_pending_viewport_property_targets(logic_commands);
    let commands = targets
        .remove(&viewport)
        .expect("typed-only targets must be dispatched");
    assert!(commands.is_empty());
    dispatch(&host, &state, &ctx, viewport, commands);
    let expected = [
        QueuedNativeViewportPropertyCommand::Visibility {
            token: show,
            visible: true,
        },
        QueuedNativeViewportPropertyCommand::Focus(focus),
        QueuedNativeViewportPropertyCommand::PointerPassthrough {
            token: pointer,
            enabled: true,
        },
        QueuedNativeViewportPropertyCommand::Visibility {
            token: hide,
            visible: false,
        },
        QueuedNativeViewportPropertyCommand::Focus(refocus),
    ];
    let trace = host.trace.lock();
    assert_eq!(trace.len(), expected.len() * 2);
    for (pair, expected) in trace.chunks_exact(2).zip(expected) {
        assert_eq!(pair[0], Trace::Applied(expected));
        match (expected, &pair[1]) {
            (
                QueuedNativeViewportPropertyCommand::Focus(token),
                Trace::Callback(NativeViewportPropertyResult::Focus(result)),
            ) => {
                assert_eq!(result.token(), token);
                assert_eq!(result.viewport_id(), viewport);
                assert_eq!(result.window_id(), WindowId::from(41));
                assert_eq!(result.status(), NativeViewportFocusStatus::Requested);
            }
            (
                QueuedNativeViewportPropertyCommand::Visibility { token, visible },
                Trace::Callback(NativeViewportPropertyResult::Visibility(result)),
            ) => {
                assert_eq!(result.token(), token);
                assert_eq!(result.visible(), visible);
                assert_eq!(result.viewport_id(), viewport);
                assert_eq!(result.window_id(), WindowId::from(41));
                assert_eq!(result.status(), NativeViewportVisibilityStatus::Unsupported);
            }
            (
                QueuedNativeViewportPropertyCommand::PointerPassthrough { token, enabled },
                Trace::Callback(NativeViewportPropertyResult::PointerPassthrough(result)),
            ) => {
                assert_eq!(result.token(), token);
                assert_eq!(result.enabled(), enabled);
            }
            _ => panic!("each typed operation must immediately echo its own token"),
        }
    }
    drop(trace);
    assert_eq!(
        Vec::from(state.take_viewport_property_commands(other_viewport)),
        [QueuedNativeViewportPropertyCommand::Focus(other)]
    );
    assert!(state.take_viewport_property_commands(viewport).is_empty());
}

#[test]
fn missing_viewport_cannot_acknowledge_another_viewports_commands() {
    let (host, state, ctx) = fixture();
    let viewport = ViewportId::from_hash_of("missing-property-target");
    let focus = queue_native_viewport_focus(&ctx, viewport);
    let visibility = queue_native_viewport_visibility(&ctx, viewport, true);
    dispatch(
        &host,
        &state,
        &ctx,
        ViewportId::ROOT,
        [
            egui::ViewportCommand::Focus,
            egui::ViewportCommand::Visible(true),
        ],
    );
    assert_eq!(host.trace.lock().len(), 2);
    assert_eq!(
        Vec::from(state.take_viewport_property_commands(viewport)),
        [
            QueuedNativeViewportPropertyCommand::Focus(focus),
            QueuedNativeViewportPropertyCommand::Visibility {
                token: visibility,
                visible: true
            },
        ]
    );
}

#[test]
fn property_commands_do_not_cross_attachment_or_viewport_retirement() {
    let (host, state, ctx) = fixture();
    let viewport = ViewportId::ROOT;
    queue_native_viewport_focus(&ctx, viewport);
    queue_native_viewport_visibility(&ctx, viewport, true);
    state.forget_presented_viewport(viewport);
    assert!(state.take_viewport_property_commands(viewport).is_empty());
    queue_native_viewport_focus(&ctx, viewport);
    queue_native_viewport_visibility(&ctx, viewport, true);
    state.forget_presented_window(viewport, WindowId::from(41));
    assert!(state.take_viewport_property_commands(viewport).is_empty());
    let stale = queue_native_viewport_focus(&ctx, viewport);
    let clone = state.clone();
    drop(state);
    assert_eq!(
        Vec::from(clone.take_viewport_property_commands(viewport)),
        [QueuedNativeViewportPropertyCommand::Focus(stale)]
    );
    queue_native_viewport_focus(&ctx, viewport);
    queue_native_viewport_visibility(&ctx, viewport, true);
    drop(clone);
    queue_native_viewport_focus(&ctx, viewport);
    queue_native_viewport_visibility(&ctx, viewport, true);
    let replacement = NativeHostState::new(Some(host.clone()));
    replacement.bind_close_context(&ctx);
    assert!(
        replacement
            .take_viewport_property_commands(viewport)
            .is_empty()
    );
    let fresh = queue_native_viewport_focus(&ctx, viewport);
    assert_ne!(fresh, stale);
    assert_eq!(
        Vec::from(replacement.take_viewport_property_commands(viewport)),
        [QueuedNativeViewportPropertyCommand::Focus(fresh)]
    );
    assert!(host.trace.lock().is_empty());
}

#[test]
fn retired_typed_properties_leave_no_ordinary_commands_for_a_successor_window() {
    for (retire_window, full_output) in [(false, false), (true, false), (false, true), (true, true)]
    {
        for preserve_app_commands in [false, true] {
            let (host, state, ctx) = fixture();
            // Settle upstream's initial theme command and font texture before this operation.
            let mut initial = ctx.run_ui(egui::RawInput::default(), |_| {});
            initial.textures_delta.clear();
            let viewport = ViewportId::ROOT;
            let predecessor = WindowId::from(40);
            state
                .inner
                .as_ref()
                .unwrap()
                .capture_window_input_state(viewport, predecessor);
            let app_commands = [
                egui::ViewportCommand::Focus,
                egui::ViewportCommand::Visible(true),
                egui::ViewportCommand::MousePassthrough(true),
            ];
            if preserve_app_commands {
                for command in &app_commands {
                    ctx.send_viewport_cmd_to(viewport, command.clone());
                }
            }
            queue_native_viewport_focus(&ctx, viewport);
            queue_native_viewport_visibility(&ctx, viewport, true);
            queue_native_viewport_pointer_passthrough(&ctx, viewport, true);

            // Preserve output/deferred-command copies collected before the old window retires.
            let mut output_commands = if full_output {
                let mut output = ctx.run_ui(egui::RawInput::default(), |_| {});
                // This command-only model deliberately does not submit renderer texture work.
                output.textures_delta.clear();
                output.viewport_output.remove(&viewport).unwrap().commands
            } else {
                ctx.run_logic(&egui::RawInput::default(), |_| {})
                    .viewport_commands
                    .remove(&viewport)
                    .unwrap_or_default()
            };
            let mut targets = state.with_pending_viewport_property_targets(Default::default());
            assert!(targets[&viewport].is_empty());
            targets
                .entry(viewport)
                .or_default()
                .append(&mut output_commands);
            if retire_window {
                state.forget_presented_window(viewport, predecessor);
            } else {
                state.forget_presented_viewport(viewport);
            }
            assert!(
                state
                    .with_pending_viewport_property_targets(Default::default())
                    .is_empty()
            );

            // Also collect any residual Context output after retirement, then replay the saved
            // ordinary commands against the successor (the dispatch stub uses window 41).
            let residual = ctx
                .run_logic(&egui::RawInput::default(), |_| {})
                .viewport_commands;
            for (target, commands) in residual {
                targets.entry(target).or_default().extend(commands);
            }
            let deferred_commands = targets.remove(&viewport).unwrap();
            dispatch(&host, &state, &ctx, viewport, deferred_commands);

            let expected = if preserve_app_commands {
                app_commands
                    .into_iter()
                    .map(Trace::Ordinary)
                    .collect::<Vec<_>>()
            } else {
                Vec::new()
            };
            assert_eq!(*host.trace.lock(), expected);
        }
    }
}

#[test]
fn property_commands_without_a_backend_handler_do_not_schedule_callbacks() {
    let ctx = egui::Context::default();
    let state = NativeHostState::new(None);
    let host = PropertyHost::default();
    queue_native_viewport_focus(&ctx, ViewportId::ROOT);
    queue_native_viewport_visibility(&ctx, ViewportId::ROOT, true);
    queue_native_viewport_pointer_passthrough(&ctx, ViewportId::ROOT, true);
    assert!(
        ctx.run_logic(&egui::RawInput::default(), |_| {})
            .viewport_commands
            .is_empty()
    );
    dispatch(&host, &state, &ctx, ViewportId::ROOT, []);
    assert!(host.trace.lock().is_empty());
}

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use egui::{ViewportCommand, ViewportId};
use winit::{event::WindowEvent, window::WindowId};

use super::EpiIntegration;
use crate::native::host_seam::{
    NativeEventSequencer, NativeHostHandler, NativeHostState, NativeHostWake,
    NativeViewportCloseDecision, NativeViewportCloseRequest, NativeWindowEvent,
    NativeWindowEventDisposition, resolve_native_viewport_close,
};

#[derive(Default)]
struct CloseHost {
    hold: AtomicBool,
    requests: Mutex<Vec<NativeViewportCloseRequest>>,
    cancellations: Mutex<Vec<NativeViewportCloseRequest>>,
}

impl NativeHostHandler for CloseHost {
    fn on_window_event(&self, event: NativeWindowEvent<'_>) -> NativeWindowEventDisposition {
        let request = event.close_request().unwrap();
        self.requests.lock().unwrap().push(request);
        if self.hold.load(Ordering::Relaxed) {
            NativeWindowEventDisposition::HoldClose(request)
        } else {
            NativeWindowEventDisposition::Continue
        }
    }

    fn on_viewport_close_cancelled(&self, request: NativeViewportCloseRequest) -> NativeHostWake {
        self.cancellations.lock().unwrap().push(request);
        NativeHostWake::RepaintRoot
    }
}

#[derive(Clone, Copy)]
enum UpdatePath {
    Full,
    LogicOnly,
}

struct CloseApp {
    path: UpdatePath,
    target: ViewportId,
    request: Option<NativeViewportCloseRequest>,
    decision: Option<NativeViewportCloseDecision>,
    ordinary_commands: bool,
    cancel_app_close: bool,
    saw_close: bool,
    logic_calls: usize,
    ui_calls: usize,
}

impl CloseApp {
    fn new(path: UpdatePath, target: ViewportId) -> Self {
        Self {
            path,
            target,
            request: None,
            decision: None,
            ordinary_commands: false,
            cancel_app_close: false,
            saw_close: false,
            logic_calls: 0,
            ui_calls: 0,
        }
    }

    fn send_commands(&self, ctx: &egui::Context) {
        if self.ordinary_commands {
            ctx.send_viewport_cmd_to(self.target, ViewportCommand::Close);
            ctx.send_viewport_cmd_to(self.target, ViewportCommand::CancelClose);
            ctx.send_viewport_cmd_to(self.target, ViewportCommand::Title("still held".into()));
        } else if self.cancel_app_close {
            ctx.send_viewport_cmd_to(self.target, ViewportCommand::CancelClose);
        }
        if let Some(decision) = self.decision {
            assert!(resolve_native_viewport_close(
                ctx,
                self.request.unwrap(),
                decision
            ));
        }
    }
}

impl crate::App for CloseApp {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut crate::Frame) {
        self.logic_calls += 1;
        self.saw_close |= ctx.input(|input| {
            input
                .raw
                .viewports
                .get(&self.target)
                .is_some_and(egui::ViewportInfo::close_requested)
        });
        if matches!(self.path, UpdatePath::LogicOnly) {
            self.send_commands(ctx);
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut crate::Frame) {
        self.ui_calls += 1;
        if self.target != ViewportId::ROOT {
            ui.ctx().show_viewport_deferred(
                self.target,
                egui::ViewportBuilder::default(),
                |_, _| {},
            );
        }
        if matches!(self.path, UpdatePath::Full) {
            self.send_commands(ui.ctx());
        }
    }
}

fn integration() -> EpiIntegration {
    let egui_ctx = egui::Context::default();
    egui_ctx.set_embed_viewports(false);
    EpiIntegration {
        frame: crate::Frame {
            info: crate::IntegrationInfo { cpu_usage: None },
            storage: None,
            #[cfg(feature = "glow")]
            gl: None,
            #[cfg(feature = "glow")]
            glow_register_native_texture: None,
            #[cfg(feature = "wgpu_no_default_features")]
            wgpu_render_state: None,
            #[cfg(feature = "wgpu_no_default_features")]
            wgpu_surface_config: None,
            window: None,
            raw_window_handle: Err(raw_window_handle::HandleError::Unavailable),
            raw_display_handle: Err(raw_window_handle::HandleError::Unavailable),
            activation_token: None,
        },
        last_auto_save: web_time::Instant::now(),
        beginning: web_time::Instant::now(),
        is_first_frame: true,
        egui_ctx,
        pending_raw_input: Default::default(),
        pending_full_output: Default::default(),
        close: false,
        viewport_close_result: None,
        can_drag_window: false,
        #[cfg(feature = "persistence")]
        persist_window: true,
        restored_window_settings: None,
        #[cfg(feature = "persistence")]
        normal_geometry: None,
        app_icon_setter: crate::native::app_icon::AppTitleIconSetter::new(
            "close regression".into(),
            None,
        ),
    }
}

fn close_input(target: ViewportId, close: bool) -> egui::RawInput {
    let mut input = egui::RawInput::default();
    let viewport = input.viewports.entry(target).or_default();
    if close {
        viewport.events.push(egui::ViewportEvent::Close);
    }
    input
}

fn update(
    integration: &mut EpiIntegration,
    app: &mut CloseApp,
    input: egui::RawInput,
    native_host: &NativeHostState,
) -> Vec<(ViewportId, Vec<ViewportCommand>)> {
    match app.path {
        UpdatePath::Full => {
            let mut output = integration.update(app, None, input, native_host);
            let commands = core::mem::take(&mut output.viewport_output)
                .into_iter()
                .map(|(id, output)| (id, output.commands))
                .collect();
            output.drop_without_applying_deltas();
            commands
        }
        UpdatePath::LogicOnly => integration
            .update_logic_only(app, input, native_host)
            .viewport_commands
            .into_iter()
            .collect(),
    }
}

fn held_close(path: UpdatePath, target: ViewportId, decision: NativeViewportCloseDecision) {
    let mut integration = integration();
    let host = Arc::new(CloseHost {
        hold: AtomicBool::new(true),
        ..Default::default()
    });
    let native_host = NativeHostState::new(Some(host.clone()));
    let mut sequencer = NativeEventSequencer::default();
    let window = WindowId::from(20);
    let disposition = native_host.observe_window_event(
        Some(&integration.egui_ctx),
        sequencer.next(),
        window,
        Some(target),
        &WindowEvent::CloseRequested,
    );
    let NativeWindowEventDisposition::HoldClose(request) = disposition else {
        panic!("the actual host callback must hold its native close request");
    };
    assert_eq!(*host.requests.lock().unwrap(), [request]);
    assert_eq!(request.viewport_id(), target);
    assert_eq!(request.window_id(), window);
    let mut app = CloseApp::new(path, target);
    app.request = Some(request);

    for _ in 0..2 {
        update(
            &mut integration,
            &mut app,
            close_input(target, true),
            &native_host,
        );
        assert!(native_host.is_close_held(target));
        assert!(!integration.should_close());
        assert!(!app.saw_close);
        assert!(host.cancellations.lock().unwrap().is_empty());
        assert_eq!(integration.take_viewport_close_result(), None);
    }

    app.ordinary_commands = true;
    for resolving in [false, true] {
        app.decision = resolving.then_some(decision);
        let output = update(
            &mut integration,
            &mut app,
            close_input(target, true),
            &native_host,
        );
        assert!(output.iter().any(|(id, commands)| {
            *id == target && commands.contains(&ViewportCommand::Title("still held".into()))
        }));
        assert!(output.iter().all(|(_, commands)| {
            !commands.iter().any(|command| {
                matches!(
                    command,
                    ViewportCommand::Close | ViewportCommand::CancelClose
                )
            })
        }));
        assert_eq!(native_host.is_close_held(target), !resolving);
        assert_eq!(
            integration.should_close(),
            resolving
                && target == ViewportId::ROOT
                && decision == NativeViewportCloseDecision::Allow
        );
        assert!(!app.saw_close);
        assert_eq!(integration.take_viewport_close_result(), None);
        let expected = if resolving && decision == NativeViewportCloseDecision::Veto {
            vec![request]
        } else {
            vec![]
        };
        assert_eq!(*host.cancellations.lock().unwrap(), expected);
    }

    app.decision = None;
    app.ordinary_commands = false;
    for _ in 0..2 {
        update(
            &mut integration,
            &mut app,
            close_input(target, false),
            &native_host,
        );
        assert!(!native_host.is_close_held(target));
        assert_eq!(
            integration.should_close(),
            target == ViewportId::ROOT && decision == NativeViewportCloseDecision::Allow
        );
        assert_eq!(
            *host.cancellations.lock().unwrap(),
            if decision == NativeViewportCloseDecision::Veto {
                vec![request]
            } else {
                vec![]
            }
        );
    }
    assert!(!resolve_native_viewport_close(
        &integration.egui_ctx,
        request,
        decision
    ));
    assert!(app.logic_calls >= 6);
    assert_eq!(app.ui_calls > 0, matches!(path, UpdatePath::Full));
}

#[test]
fn root_full_update_later_allow() {
    held_close(
        UpdatePath::Full,
        ViewportId::ROOT,
        NativeViewportCloseDecision::Allow,
    );
}

#[test]
fn root_full_update_later_veto() {
    held_close(
        UpdatePath::Full,
        ViewportId::ROOT,
        NativeViewportCloseDecision::Veto,
    );
}

#[test]
fn root_logic_only_later_allow() {
    held_close(
        UpdatePath::LogicOnly,
        ViewportId::ROOT,
        NativeViewportCloseDecision::Allow,
    );
}

#[test]
fn root_logic_only_later_veto() {
    held_close(
        UpdatePath::LogicOnly,
        ViewportId::ROOT,
        NativeViewportCloseDecision::Veto,
    );
}

#[test]
fn child_allow_via_root_updates_never_closes_root() {
    for path in [UpdatePath::Full, UpdatePath::LogicOnly] {
        held_close(
            path,
            ViewportId::from_hash_of("held-child"),
            NativeViewportCloseDecision::Allow,
        );
    }
}

#[test]
fn child_veto_via_root_updates_reports_exact_request_once() {
    for path in [UpdatePath::Full, UpdatePath::LogicOnly] {
        held_close(
            path,
            ViewportId::from_hash_of("held-child"),
            NativeViewportCloseDecision::Veto,
        );
    }
}

#[test]
fn application_owned_close_keeps_traditional_cancel_behavior() {
    for path in [UpdatePath::Full, UpdatePath::LogicOnly] {
        for cancel in [false, true] {
            let mut integration = integration();
            let host = Arc::new(CloseHost::default());
            let native_host = NativeHostState::new(Some(host.clone()));
            let mut sequencer = NativeEventSequencer::default();
            assert_eq!(
                native_host.observe_window_event(
                    Some(&integration.egui_ctx),
                    sequencer.next(),
                    WindowId::from(10),
                    Some(ViewportId::ROOT),
                    &WindowEvent::CloseRequested,
                ),
                NativeWindowEventDisposition::Continue
            );
            let mut app = CloseApp::new(path, ViewportId::ROOT);
            app.cancel_app_close = cancel;
            let output = update(
                &mut integration,
                &mut app,
                close_input(ViewportId::ROOT, true),
                &native_host,
            );
            assert!(app.saw_close);
            assert!(!native_host.is_close_held(ViewportId::ROOT));
            assert_eq!(integration.should_close(), !cancel);
            assert_eq!(
                integration.take_viewport_close_result(),
                Some((ViewportId::ROOT, cancel))
            );
            assert_eq!(
                output.iter().any(|(id, commands)| {
                    *id == ViewportId::ROOT && commands.contains(&ViewportCommand::CancelClose)
                }),
                cancel
            );
            assert!(host.cancellations.lock().unwrap().is_empty());
        }
    }
}

#[test]
fn child_veto_consumes_backend_root_close_before_releasing_gate() {
    let mut integration = integration();
    let host = Arc::new(CloseHost::default());
    let native_host = NativeHostState::new(Some(host.clone()));
    let child = ViewportId::from_hash_of("child-resolves-root-veto");
    let mut app = CloseApp::new(UpdatePath::Full, child);
    update(
        &mut integration,
        &mut app,
        close_input(child, false),
        &native_host,
    );
    let mut sequence = NativeEventSequencer::default();
    let window = WindowId::from(10);
    assert_eq!(
        native_host.observe_window_event(
            Some(&integration.egui_ctx),
            sequence.next(),
            window,
            Some(ViewportId::ROOT),
            &WindowEvent::CloseRequested
        ),
        NativeWindowEventDisposition::Continue
    );
    let mut backend = close_input(ViewportId::ROOT, true).viewports;
    backend.entry(child).or_default().parent = Some(ViewportId::ROOT);
    host.hold.store(true, Ordering::Relaxed);
    let NativeWindowEventDisposition::HoldClose(request) = native_host.observe_window_event(
        Some(&integration.egui_ctx),
        sequence.next(),
        window,
        Some(ViewportId::ROOT),
        &WindowEvent::CloseRequested,
    ) else {
        panic!("the repeated event must claim the same canonical request");
    };
    assert_eq!(*host.requests.lock().unwrap(), [request, request]);

    // Keep the renderer-owned ViewportInfo records across both updates. The
    // production capture entry must consume the held Close before cloning it.
    let snapshot = backend
        .iter_mut()
        .map(|(id, info)| (*id, native_host.capture_viewport_info(*id, info)))
        .collect();
    let callback = move |ui: &mut egui::Ui| {
        assert!(
            !ui.ctx()
                .input(|input| input.raw.viewports[&ViewportId::ROOT].close_requested())
        );
        assert!(resolve_native_viewport_close(
            ui.ctx(),
            request,
            NativeViewportCloseDecision::Veto
        ));
    };
    let output = integration.update(
        &mut app,
        Some(&callback),
        egui::RawInput {
            viewport_id: child,
            viewports: snapshot,
            ..Default::default()
        },
        &native_host,
    );
    output.drop_without_applying_deltas();
    assert!(!native_host.is_close_held(ViewportId::ROOT));
    assert_eq!(*host.cancellations.lock().unwrap(), [request]);

    app.target = ViewportId::ROOT;
    let snapshot = backend
        .iter_mut()
        .map(|(id, info)| (*id, native_host.capture_viewport_info(*id, info)))
        .collect();
    update(
        &mut integration,
        &mut app,
        egui::RawInput {
            viewports: snapshot,
            ..Default::default()
        },
        &native_host,
    );
    assert!(
        !integration.should_close(),
        "a Veto must consume the renderer-owned old Close, not only its snapshot"
    );
    assert!(!app.saw_close);
    assert_eq!(*host.cancellations.lock().unwrap(), [request]);
}

#[test]
fn first_input_hook_can_resolve_a_close_captured_before_context_binding() {
    struct HookVeto {
        request: NativeViewportCloseRequest,
        resolved: bool,
    }
    impl crate::App for HookVeto {
        fn raw_input_hook(&mut self, ctx: &egui::Context, _input: &mut egui::RawInput) {
            self.resolved =
                resolve_native_viewport_close(ctx, self.request, NativeViewportCloseDecision::Veto);
            assert!(self.resolved);
        }
        fn ui(&mut self, _ui: &mut egui::Ui, _frame: &mut crate::Frame) {}
    }
    for path in [UpdatePath::Full, UpdatePath::LogicOnly] {
        let mut integration = integration();
        let host = Arc::new(CloseHost {
            hold: AtomicBool::new(true),
            ..Default::default()
        });
        let native_host = NativeHostState::new(Some(host.clone()));
        let mut sequence = NativeEventSequencer::default();
        let NativeWindowEventDisposition::HoldClose(request) = native_host.observe_window_event(
            None,
            sequence.next(),
            WindowId::from(10),
            Some(ViewportId::ROOT),
            &WindowEvent::CloseRequested,
        ) else {
            panic!("the first close must remain held before an egui context is available");
        };
        let mut app = HookVeto {
            request,
            resolved: false,
        };
        match path {
            UpdatePath::Full => integration
                .update(
                    &mut app,
                    None,
                    close_input(ViewportId::ROOT, true),
                    &native_host,
                )
                .drop_without_applying_deltas(),
            UpdatePath::LogicOnly => {
                integration.update_logic_only(
                    &mut app,
                    close_input(ViewportId::ROOT, true),
                    &native_host,
                );
            }
        }
        assert!(app.resolved);
        assert!(!integration.should_close());
        assert_eq!(*host.cancellations.lock().unwrap(), [request]);
    }
}

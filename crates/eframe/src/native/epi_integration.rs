//! Common tools used by [`super::glow_integration`] and [`super::wgpu_integration`].

use web_time::Instant;

use std::{path::PathBuf, sync::Arc};
use winit::event_loop::ActiveEventLoop;

use raw_window_handle::{HasDisplayHandle as _, HasWindowHandle as _};

use egui::{
    DeferredViewportUiCallback, OrderedViewportIdMap, ViewportBuilder, ViewportCommand, ViewportId,
    ViewportOutput,
};
use egui_winit::{EventResponse, WindowSettings};

use crate::epi;

use super::hosted_cycle::HostedViewportOutputConsolidationError;

#[cfg_attr(target_os = "ios", allow(dead_code, unused_variables, unused_mut))]
pub fn viewport_builder(
    egui_zoom_factor: f32,
    event_loop: &ActiveEventLoop,
    native_options: &mut epi::NativeOptions,
    window_settings: Option<WindowSettings>,
) -> ViewportBuilder {
    profiling::function_scope!();

    let mut viewport_builder = native_options.viewport.clone();

    // On some Linux systems, a window size larger than the monitor causes crashes,
    // and on Windows the window does not appear at all.
    let clamp_size_to_monitor_size = viewport_builder.clamp_size_to_monitor_size.unwrap_or(true);

    // Always use the default window size / position on iOS. Trying to restore the previous position
    // causes the window to be shown too small.
    #[cfg(not(target_os = "ios"))]
    let inner_size_points = if let Some(mut window_settings) = window_settings {
        // Restore pos/size from previous session

        if clamp_size_to_monitor_size {
            window_settings.clamp_size_to_sane_values(largest_monitor_point_size(
                egui_zoom_factor,
                event_loop,
            ));
        }
        window_settings.clamp_position_to_monitors(egui_zoom_factor, event_loop);

        viewport_builder = window_settings.initialize_viewport_builder(
            egui_zoom_factor,
            event_loop,
            viewport_builder,
        );
        window_settings.inner_size_points()
    } else {
        if let Some(pos) = viewport_builder.position {
            viewport_builder = viewport_builder.with_position(pos);
        }

        if clamp_size_to_monitor_size && let Some(initial_window_size) = viewport_builder.inner_size
        {
            let initial_window_size = egui::NumExt::at_most(
                initial_window_size,
                largest_monitor_point_size(egui_zoom_factor, event_loop),
            );
            viewport_builder = viewport_builder.with_inner_size(initial_window_size);
        }

        viewport_builder.inner_size
    };

    #[cfg(not(target_os = "ios"))]
    if native_options.centered {
        profiling::scope!("center");
        if let Some(monitor) = event_loop
            .primary_monitor()
            .or_else(|| event_loop.available_monitors().next())
        {
            let monitor_size = monitor
                .size()
                .to_logical::<f32>(egui_zoom_factor as f64 * monitor.scale_factor());
            let inner_size = inner_size_points.unwrap_or(egui::Vec2 { x: 800.0, y: 600.0 });
            if 0.0 < monitor_size.width && 0.0 < monitor_size.height {
                let x = (monitor_size.width - inner_size.x) / 2.0;
                let y = (monitor_size.height - inner_size.y) / 2.0;
                viewport_builder = viewport_builder.with_position([x, y]);
            }
        }
    }

    match std::mem::take(&mut native_options.window_builder) {
        Some(hook) => hook(viewport_builder),
        None => viewport_builder,
    }
}

pub fn apply_window_settings(
    window: &winit::window::Window,
    window_settings: Option<WindowSettings>,
) {
    profiling::function_scope!();
    if let Some(window_settings) = window_settings {
        window_settings.initialize_window(window);
    }
}

#[cfg(not(target_os = "ios"))]
fn largest_monitor_point_size(egui_zoom_factor: f32, event_loop: &ActiveEventLoop) -> egui::Vec2 {
    profiling::function_scope!();
    let mut max_size = egui::Vec2::ZERO;

    let available_monitors = {
        profiling::scope!("available_monitors");
        event_loop.available_monitors()
    };

    for monitor in available_monitors {
        let size = monitor
            .size()
            .to_logical::<f32>(egui_zoom_factor as f64 * monitor.scale_factor());
        let size = egui::vec2(size.width, size.height);
        max_size = max_size.max(size);
    }

    if max_size == egui::Vec2::ZERO {
        egui::Vec2::splat(16000.0)
    } else {
        max_size
    }
}

// ----------------------------------------------------------------------------

/// For loading/saving app state and/or egui memory to disk.
pub fn create_storage(_app_name: &str) -> Option<Box<dyn epi::Storage>> {
    #[cfg(feature = "persistence")]
    if let Some(storage) = super::file_storage::FileStorage::from_app_id(_app_name) {
        return Some(Box::new(storage));
    }
    None
}

#[allow(clippy::allow_attributes, clippy::unnecessary_wraps)]
pub fn create_storage_with_file(_file: impl Into<PathBuf>) -> Option<Box<dyn epi::Storage>> {
    #[cfg(feature = "persistence")]
    return Some(Box::new(
        super::file_storage::FileStorage::from_ron_filepath(_file),
    ));
    #[cfg(not(feature = "persistence"))]
    None
}

// ----------------------------------------------------------------------------

#[derive(Default)]
struct RootCloseLifecycle {
    candidate: bool,
    committed: bool,
}

struct PreparedRootCloseCommit {
    candidate: bool,
    cancelled: bool,
}

pub(super) struct PreparedHostedViewportCycleCommit {
    root_close: PreparedRootCloseCommit,
}

impl RootCloseLifecycle {
    fn request(&mut self) {
        self.candidate = true;
    }

    fn abort(&mut self) {
        self.candidate = false;
    }

    fn prepare(&self, sealed_root_commands: &[ViewportCommand]) -> PreparedRootCloseCommit {
        PreparedRootCloseCommit {
            candidate: self.candidate,
            cancelled: sealed_root_commands.contains(&ViewportCommand::CancelClose),
        }
    }

    fn commit(&mut self, prepared: PreparedRootCloseCommit) {
        debug_assert_eq!(self.candidate, prepared.candidate);
        self.candidate = false;
        if !prepared.candidate {
            return;
        }
        if prepared.cancelled {
            log::debug!("Closing of root viewport canceled with ViewportCommand::CancelClose");
        } else {
            log::debug!("Closing root viewport (ViewportCommand::CancelClose was not sent)");
            self.committed = true;
        }
    }

    fn should_close(&self) -> bool {
        self.committed
    }
}

// ----------------------------------------------------------------------------

/// Everything needed to make a winit-based integration for [`epi`].
///
/// Only one instance per app (not one per viewport).
pub struct EpiIntegration {
    pub frame: epi::Frame,
    last_auto_save: Instant,
    pub beginning: Instant,
    is_first_frame: bool,
    pub egui_ctx: egui::Context,
    pending_full_output: egui::FullOutput,
    aborted_full_outputs: Vec<crate::HostedViewportOutput<egui::FullOutput>>,

    root_close: RootCloseLifecycle,

    can_drag_window: bool,
    #[cfg(feature = "persistence")]
    persist_window: bool,
    app_icon_setter: super::app_icon::AppTitleIconSetter,
}

impl EpiIntegration {
    #[allow(clippy::allow_attributes, clippy::too_many_arguments)]
    pub fn new(
        egui_ctx: egui::Context,
        window: &Arc<winit::window::Window>,
        app_name: &str,
        native_options: &crate::NativeOptions,
        storage: Option<Box<dyn epi::Storage>>,
        #[cfg(feature = "glow")] gl: Option<std::sync::Arc<glow::Context>>,
        #[cfg(feature = "glow")] glow_register_native_texture: Option<
            Box<dyn FnMut(glow::Texture) -> egui::TextureId>,
        >,
        #[cfg(feature = "wgpu_no_default_features")] wgpu_render_state: Option<
            egui_wgpu::RenderState,
        >,
    ) -> Self {
        let frame = epi::Frame {
            info: epi::IntegrationInfo { cpu_usage: None },
            storage,
            #[cfg(feature = "glow")]
            gl,
            #[cfg(feature = "glow")]
            glow_register_native_texture,
            #[cfg(feature = "wgpu_no_default_features")]
            wgpu_render_state,
            window: Some(Arc::clone(window)),
            raw_display_handle: window.display_handle().map(|h| h.as_raw()),
            raw_window_handle: window.window_handle().map(|h| h.as_raw()),
        };

        let icon = native_options
            .viewport
            .icon
            .clone()
            .unwrap_or_else(|| std::sync::Arc::new(load_default_egui_icon()));

        let app_icon_setter = super::app_icon::AppTitleIconSetter::new(
            native_options
                .viewport
                .title
                .clone()
                .unwrap_or_else(|| app_name.to_owned()),
            Some(icon),
        );

        Self {
            frame,
            last_auto_save: Instant::now(),
            pending_full_output: Default::default(),
            aborted_full_outputs: Vec::new(),
            root_close: RootCloseLifecycle::default(),
            can_drag_window: false,
            #[cfg(feature = "persistence")]
            persist_window: native_options.persist_window,
            app_icon_setter,
            beginning: Instant::now()
                .checked_sub(web_time::Duration::from_secs_f64(egui_ctx.time()))
                .unwrap_or_else(Instant::now),
            is_first_frame: true,
            egui_ctx,
        }
    }

    /// If `true`, it is time to close the native window.
    pub fn should_close(&self) -> bool {
        self.root_close.should_close()
    }

    pub fn on_window_event(
        &mut self,
        window: &winit::window::Window,
        egui_winit: &mut egui_winit::State,
        event: &winit::event::WindowEvent,
        sequence: Option<egui::BackendEventSequence>,
    ) -> EventResponse {
        profiling::function_scope!(egui_winit::short_window_event_description(event));

        use winit::event::{ElementState, MouseButton, WindowEvent};

        if let WindowEvent::MouseInput {
            button: MouseButton::Left,
            state: ElementState::Pressed,
            ..
        } = event
        {
            self.can_drag_window = true;
        }

        if let Some(sequence) = sequence {
            egui_winit.on_window_event_with_sequence(window, event, sequence)
        } else {
            egui_winit.on_window_event(window, event)
        }
    }

    pub fn pre_update(&mut self) {
        self.app_icon_setter.update();
    }

    /// Applies the application raw-input hook without running any UI.
    ///
    /// Native hosted cycles call this for their complete viewport roster before
    /// invoking the first viewport callback.
    pub fn prepare_raw_input(
        &self,
        app: &mut dyn epi::App,
        mut raw_input: egui::RawInput,
    ) -> egui::RawInput {
        raw_input.time = Some(self.beginning.elapsed().as_secs_f64());
        let event_provenance = raw_input.event_provenance_snapshot();
        app.raw_input_hook(&self.egui_ctx, &mut raw_input);
        raw_input.sanitize_hook_events(&event_provenance);
        raw_input
    }

    /// Notifies the application that a complete hosted input roster is frozen.
    pub fn begin_hosted_viewport_cycle(
        &mut self,
        app: &mut dyn epi::App,
        cycle: &crate::HostedViewportCycle,
    ) -> crate::HostedViewportAppResult<()> {
        if !self.aborted_full_outputs.is_empty() {
            return Err(std::io::Error::other(
                "the previous aborted hosted viewport output has not been terminally settled",
            )
            .into());
        }
        if self.root_close.candidate {
            return Err(std::io::Error::other(
                "the previous hosted root-close candidate was neither committed nor aborted",
            )
            .into());
        }
        app.begin_hosted_viewport_cycle(&self.egui_ctx, cycle, &mut self.frame)
    }

    /// Notifies the application that every hosted viewport output is staged.
    pub fn end_hosted_viewport_cycle(
        &mut self,
        app: &mut dyn epi::App,
        outputs: &mut [crate::HostedViewportOutput<egui::FullOutput>],
    ) -> crate::HostedViewportAppResult<()> {
        app.end_hosted_viewport_cycle(&self.egui_ctx, outputs, &mut self.frame)
    }

    /// Publishes application-owned state after the native host transaction sealed.
    pub fn commit_application_hosted_viewport_cycle(
        &mut self,
        app: &mut dyn epi::App,
        outputs: &mut [crate::HostedViewportOutput<egui::FullOutput>],
    ) -> crate::HostedViewportAppResult<crate::HostedViewportCommitDirective> {
        app.commit_hosted_viewport_cycle(outputs)
    }

    /// Notifies the application that its hosted-cycle transaction aborted.
    pub fn abort_application_hosted_viewport_cycle(&mut self, app: &mut dyn epi::App) {
        app.abort_hosted_viewport_cycle(&self.egui_ctx, &mut self.frame);
    }

    /// Prepares cycle-local state after output consolidation has sealed a
    /// complete physical viewport roster.
    pub(super) fn prepare_hosted_viewport_cycle(
        &self,
        sealed_viewport_output: &OrderedViewportIdMap<ViewportOutput>,
    ) -> Result<PreparedHostedViewportCycleCommit, HostedViewportOutputConsolidationError> {
        let Some(root_output) = sealed_viewport_output.get(&ViewportId::ROOT) else {
            return Err(HostedViewportOutputConsolidationError::MissingRootRecord);
        };
        Ok(PreparedHostedViewportCycleCommit {
            root_close: self.root_close.prepare(&root_output.commands),
        })
    }

    /// Publishes the already validated cycle-local state after the application
    /// commit hook has succeeded.
    pub(super) fn commit_hosted_viewport_cycle(
        &mut self,
        prepared: PreparedHostedViewportCycleCommit,
    ) {
        self.root_close.commit(prepared.root_close);
    }

    /// Aborts cycle-local state and returns the failing callback outputs that
    /// could not be carried by the generic cycle abort.
    ///
    /// These outputs must not be painted or replayed. The backend must still
    /// terminally settle their presentation tokens and account for their texture
    /// deltas before beginning another hosted cycle.
    pub(super) fn abort_hosted_viewport_cycle(
        &mut self,
    ) -> Vec<crate::HostedViewportOutput<egui::FullOutput>> {
        self.root_close.abort();
        std::mem::take(&mut self.aborted_full_outputs)
    }

    /// Runs one viewport callback with input already prepared for its host cycle.
    pub fn update_prepared(
        &mut self,
        app: &mut dyn epi::App,
        viewport_ui_cb: Option<&DeferredViewportUiCallback>,
        raw_input: egui::RawInput,
    ) -> crate::HostedViewportAppResult<egui::FullOutput> {
        let viewport_id = raw_input.viewport_id;
        let close_requested = raw_input.viewport().close_requested();
        let is_root_viewport = viewport_id == ViewportId::ROOT;
        let mut ui_result = Ok(());

        let full_output = self.egui_ctx.run_ui(raw_input, |ui| {
            if ui_result.is_err() {
                return;
            }
            if is_root_viewport {
                {
                    profiling::scope!("App::logic");
                    app.logic(ui.ctx(), &mut self.frame);
                }
            }
            ui_result = dispatch_hosted_viewport_ui(app, viewport_ui_cb, ui, &mut self.frame);
        });
        if let Err(error) = ui_result {
            self.aborted_full_outputs
                .push(crate::HostedViewportOutput::new(viewport_id, full_output));
            return Err(error);
        }

        if is_root_viewport && close_requested {
            self.root_close.request();
        }

        self.pending_full_output.append(full_output);
        Ok(std::mem::take(&mut self.pending_full_output))
    }

    pub fn report_frame_time(&mut self, seconds: f32) {
        self.frame.info.cpu_usage = Some(seconds);
    }

    pub fn post_rendering(&mut self, window: &winit::window::Window) {
        profiling::function_scope!();
        if std::mem::take(&mut self.is_first_frame) {
            // We keep hidden until we've painted something. See https://github.com/emilk/egui/pull/2279
            window.set_visible(true);
        }
    }

    // ------------------------------------------------------------------------
    // Persistence stuff:

    pub fn maybe_autosave(
        &mut self,
        app: &mut dyn epi::App,
        window: Option<&winit::window::Window>,
    ) {
        let now = Instant::now();
        if now - self.last_auto_save > app.auto_save_interval() {
            self.save(app, window);
            self.last_auto_save = now;
        }
    }

    pub fn save(&mut self, app: &mut dyn epi::App, window: Option<&winit::window::Window>) {
        #[cfg(not(feature = "persistence"))]
        let _ = (self, app, window);

        #[cfg(feature = "persistence")]
        if let Some(storage) = self.frame.storage_mut() {
            profiling::function_scope!();

            if let Some(window) = window
                && self.persist_window
            {
                profiling::scope!("native_window");
                epi::set_value(
                    storage,
                    STORAGE_WINDOW_KEY,
                    &WindowSettings::from_window(self.egui_ctx.zoom_factor(), window),
                );
            }
            if app.persist_egui_memory() {
                profiling::scope!("egui_memory");
                self.egui_ctx
                    .memory(|mem| epi::set_value(storage, STORAGE_EGUI_MEMORY_KEY, mem));
            }
            {
                profiling::scope!("App::save");
                app.save(storage);
            }

            profiling::scope!("Storage::flush");
            storage.flush();
        }
    }
}

fn dispatch_hosted_viewport_ui(
    app: &mut dyn epi::App,
    viewport_ui_cb: Option<&DeferredViewportUiCallback>,
    ui: &mut egui::Ui,
    frame: &mut epi::Frame,
) -> crate::HostedViewportAppResult<()> {
    let disposition = app.hosted_viewport_ui(ui.ctx().viewport_id(), ui, frame)?;
    if disposition == crate::HostedViewportUiDisposition::Handled {
        return Ok(());
    }
    if let Some(viewport_ui_cb) = viewport_ui_cb {
        profiling::scope!("viewport_callback");
        viewport_ui_cb(ui);
    } else if ui.ctx().viewport_id() == ViewportId::ROOT {
        profiling::scope!("App::ui");
        app.ui(ui, frame);
    } else {
        return Err(std::io::Error::other(format!(
            "hosted deferred viewport {:?} has no UI callback",
            ui.ctx().viewport_id()
        ))
        .into());
    }
    Ok(())
}

#[cfg(test)]
mod hosted_viewport_ui_tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    #[derive(Default)]
    struct RecordingApp {
        hook_calls: usize,
        default_calls: usize,
        disposition: crate::HostedViewportUiDisposition,
        reject: bool,
        cancel_close: bool,
    }

    impl epi::App for RecordingApp {
        fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut epi::Frame) {
            self.default_calls += 1;
            if self.cancel_close {
                ui.ctx()
                    .send_viewport_cmd(egui::ViewportCommand::CancelClose);
            }
        }

        fn hosted_viewport_ui(
            &mut self,
            _viewport_id: ViewportId,
            _ui: &mut egui::Ui,
            _frame: &mut epi::Frame,
        ) -> crate::HostedViewportAppResult<crate::HostedViewportUiDisposition> {
            self.hook_calls += 1;
            if self.reject {
                Err(std::io::Error::other("hosted UI rejected").into())
            } else {
                Ok(self.disposition)
            }
        }
    }

    struct ImmediateViewportApp {
        viewport_id: ViewportId,
        observed_class: Option<egui::ViewportClass>,
    }

    impl epi::App for ImmediateViewportApp {
        fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut epi::Frame) {
            self.observed_class = Some(ui.ctx().show_viewport_immediate(
                self.viewport_id,
                egui::ViewportBuilder::default(),
                |_, class| class,
            ));
        }
    }

    fn run_dispatch(
        app: &mut RecordingApp,
        viewport_ui_cb: Option<&DeferredViewportUiCallback>,
    ) -> crate::HostedViewportAppResult<()> {
        let context = egui::Context::default();
        let mut frame = epi::Frame::_new_kittest();
        let mut result = Ok(());
        let _ = context.run_ui(egui::RawInput::default(), |ui| {
            result = dispatch_hosted_viewport_ui(app, viewport_ui_cb, ui, &mut frame);
        });
        result
    }

    fn test_integration() -> EpiIntegration {
        EpiIntegration {
            frame: epi::Frame::_new_kittest(),
            last_auto_save: Instant::now(),
            beginning: Instant::now(),
            is_first_frame: false,
            egui_ctx: egui::Context::default(),
            pending_full_output: egui::FullOutput::default(),
            aborted_full_outputs: Vec::new(),
            root_close: RootCloseLifecycle::default(),
            can_drag_window: false,
            #[cfg(feature = "persistence")]
            persist_window: false,
            app_icon_setter: super::super::app_icon::AppTitleIconSetter::new(
                "hosted-test".to_owned(),
                None,
            ),
        }
    }

    #[test]
    fn default_disposition_preserves_root_and_deferred_fallbacks() {
        let mut root = RecordingApp::default();
        run_dispatch(&mut root, None).expect("the root fallback should run");
        assert_eq!(root.hook_calls, 1);
        assert_eq!(root.default_calls, 1);

        let deferred_calls = Arc::new(AtomicUsize::new(0));
        let deferred_counter = Arc::clone(&deferred_calls);
        let deferred: Arc<DeferredViewportUiCallback> = Arc::new(move |_| {
            deferred_counter.fetch_add(1, Ordering::Relaxed);
        });
        let mut child = RecordingApp::default();
        run_dispatch(&mut child, Some(deferred.as_ref()))
            .expect("the deferred fallback should run");
        assert_eq!(child.hook_calls, 1);
        assert_eq!(child.default_calls, 0);
        assert_eq!(deferred_calls.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn minimized_and_occluded_viewports_still_run_hosted_ui() {
        let deferred_calls = Arc::new(AtomicUsize::new(0));
        let deferred_counter = Arc::clone(&deferred_calls);
        let deferred: Arc<DeferredViewportUiCallback> = Arc::new(move |_| {
            deferred_counter.fetch_add(1, Ordering::Relaxed);
        });
        let mut integration = test_integration();
        let mut app = RecordingApp::default();

        for (name, minimized, occluded) in [
            ("minimized-hosted-child", Some(true), Some(false)),
            ("occluded-hosted-child", Some(false), Some(true)),
        ] {
            let viewport_id = ViewportId::from_hash_of(name);
            let mut raw_input = egui::RawInput {
                viewport_id,
                ..Default::default()
            };
            raw_input.viewports.insert(
                viewport_id,
                egui::ViewportInfo {
                    parent: Some(ViewportId::ROOT),
                    minimized,
                    occluded,
                    ..Default::default()
                },
            );

            integration
                .update_prepared(&mut app, Some(deferred.as_ref()), raw_input)
                .expect("visibility must not suppress hosted UI");
        }

        assert_eq!(app.hook_calls, 2);
        assert_eq!(app.default_calls, 0);
        assert_eq!(deferred_calls.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn minimized_root_ui_can_veto_its_close_request() {
        let mut integration = test_integration();
        let mut app = RecordingApp {
            cancel_close: true,
            ..Default::default()
        };
        let mut raw_input = egui::RawInput::default();
        raw_input.viewports.insert(
            ViewportId::ROOT,
            egui::ViewportInfo {
                events: vec![egui::ViewportEvent::Close],
                minimized: Some(true),
                occluded: Some(false),
                ..Default::default()
            },
        );

        let output = integration
            .update_prepared(&mut app, None, raw_input)
            .expect("a minimized root must still run its close-veto UI");
        let prepared = integration
            .prepare_hosted_viewport_cycle(&output.viewport_output)
            .expect("the root output must remain structurally complete");
        integration.commit_hosted_viewport_cycle(prepared);

        assert_eq!(app.default_calls, 1);
        assert!(!integration.should_close());
    }

    #[test]
    fn transactional_immediate_viewport_is_embedded_in_the_physical_output() {
        let viewport_id = ViewportId::from_hash_of("transactional-immediate-app");
        let mut integration = test_integration();
        integration.egui_ctx.set_embed_viewports(false);
        let mut app = ImmediateViewportApp {
            viewport_id,
            observed_class: None,
        };
        let guard =
            crate::HostedViewportTransactionGuard::enter(crate::HostedViewportMode::Transactional);

        let output = integration
            .update_prepared(&mut app, None, egui::RawInput::default())
            .expect("transactional immediate UI should remain inside the root callback");

        assert!(matches!(
            app.observed_class,
            Some(egui::ViewportClass::EmbeddedWindow)
        ));
        assert!(!output.viewport_output.contains_key(&viewport_id));
        assert!(guard.finish().is_empty());
    }

    #[test]
    fn handled_disposition_suppresses_every_default_fallback() {
        let deferred_calls = Arc::new(AtomicUsize::new(0));
        let deferred_counter = Arc::clone(&deferred_calls);
        let deferred: Arc<DeferredViewportUiCallback> = Arc::new(move |_| {
            deferred_counter.fetch_add(1, Ordering::Relaxed);
        });
        let mut app = RecordingApp {
            disposition: crate::HostedViewportUiDisposition::Handled,
            ..Default::default()
        };

        run_dispatch(&mut app, Some(deferred.as_ref()))
            .expect("handled UI should complete without fallback");

        assert_eq!(app.hook_calls, 1);
        assert_eq!(app.default_calls, 0);
        assert_eq!(deferred_calls.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn hook_error_suppresses_default_fallback() {
        let mut app = RecordingApp {
            reject: true,
            ..Default::default()
        };

        let error = run_dispatch(&mut app, None)
            .expect_err("a hosted UI error must suppress the root fallback");

        assert_eq!(error.to_string(), "hosted UI rejected");
        assert_eq!(app.hook_calls, 1);
        assert_eq!(app.default_calls, 0);
    }

    #[test]
    fn root_close_is_only_committed_after_the_sealed_output_is_known() {
        let mut close = RootCloseLifecycle::default();
        close.request();

        assert!(!close.should_close());

        let prepared = close.prepare(&[]);
        assert!(!close.should_close());

        close.commit(prepared);
        assert!(close.should_close());
    }

    #[test]
    fn sealed_cancel_close_from_the_end_hook_vetoes_the_candidate() {
        let mut close = RootCloseLifecycle::default();
        close.request();

        let prepared = close.prepare(&[ViewportCommand::CancelClose]);
        close.commit(prepared);

        assert!(!close.should_close());
    }

    #[test]
    fn aborted_cycle_cannot_commit_its_root_close_candidate() {
        let mut close = RootCloseLifecycle::default();
        close.request();

        let prepared = close.prepare(&[]);
        close.abort();

        assert!(!close.should_close());
        assert!(prepared.candidate);
    }
}

fn load_default_egui_icon() -> egui::IconData {
    profiling::function_scope!();
    #[expect(clippy::unwrap_used)]
    crate::icon_data::from_png_bytes(&include_bytes!("../../data/icon.png")[..]).unwrap()
}

#[cfg(feature = "persistence")]
const STORAGE_EGUI_MEMORY_KEY: &str = "egui";

#[cfg(feature = "persistence")]
const STORAGE_WINDOW_KEY: &str = "window";

pub fn load_window_settings(_storage: Option<&dyn epi::Storage>) -> Option<WindowSettings> {
    profiling::function_scope!();
    #[cfg(feature = "persistence")]
    {
        epi::get_value(_storage?, STORAGE_WINDOW_KEY)
    }
    #[cfg(not(feature = "persistence"))]
    None
}

pub fn load_egui_memory(_storage: Option<&dyn epi::Storage>) -> Option<egui::Memory> {
    profiling::function_scope!();
    #[cfg(feature = "persistence")]
    {
        epi::get_value(_storage?, STORAGE_EGUI_MEMORY_KEY)
    }
    #[cfg(not(feature = "persistence"))]
    None
}

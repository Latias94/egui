use core::time::Duration;
use std::time::Instant;

use winit::{
    application::ApplicationHandler,
    event_loop::{ActiveEventLoop, ControlFlow, EventLoop},
    window::WindowId,
};

use ahash::HashMap;

use super::winit_integration::{UserEvent, WinitApp};
#[cfg(feature = "native-host-seam")]
use crate::native::host_seam::{NativeEventSequencer, NativeHostState, NativeWindowLocalGeometry};
use crate::{
    Result, epi,
    native::{
        event_loop_context,
        winit_integration::{EventResult, PassMode, is_invisible_or_minimized},
    },
};

/// Minimum interval between repaints for invisible windows.
///
/// On Windows, invisible windows don't receive `RedrawRequested` events,
/// so we throttle their repaints to avoid busy-looping while still
/// processing viewport commands like `Visible(true)`.
/// See <https://github.com/emilk/egui/issues/7776>.
const INVISIBLE_WINDOW_REPAINT_INTERVAL: Duration = Duration::from_millis(100);

/// How long to wait for a requested `RedrawRequested` before running the app logic anyway.
///
/// On Wayland, `RedrawRequested` waits for the compositor's frame callback, and compositors
/// may withhold those from a window that isn't shown (e.g. one covered by a fullscreen window),
/// without telling us that it is occluded. Nothing we paint would be shown, and presenting could
/// block, so until the compositor asks for a frame again we only run [`crate::App::logic`].
/// See <https://github.com/emilk/egui/issues/5136>.
const MISSED_REDRAW_TIMEOUT: Duration = Duration::from_millis(250);

// ----------------------------------------------------------------------------
fn create_event_loop(native_options: &mut epi::NativeOptions) -> Result<EventLoop<UserEvent>> {
    #[cfg(target_os = "android")]
    use winit::platform::android::EventLoopBuilderExtAndroid as _;

    profiling::function_scope!();
    let mut builder = winit::event_loop::EventLoop::with_user_event();

    #[cfg(target_os = "android")]
    let mut builder =
        builder.with_android_app(native_options.android_app.take().ok_or_else(|| {
            crate::Error::AppCreation(Box::from(
                "`NativeOptions` is missing required `android_app`",
            ))
        })?);

    if let Some(hook) = core::mem::take(&mut native_options.event_loop_builder) {
        hook(&mut builder);
    }

    profiling::scope!("EventLoopBuilder::build");
    Ok(builder.build()?)
}

/// Access a thread-local event loop.
///
/// We reuse the event-loop so we can support closing and opening an eframe window
/// multiple times. This is just a limitation of winit.
#[cfg(not(target_os = "ios"))]
fn with_event_loop<R>(
    mut native_options: epi::NativeOptions,
    f: impl FnOnce(&mut EventLoop<UserEvent>, epi::NativeOptions) -> R,
) -> Result<R> {
    thread_local!(static EVENT_LOOP: core::cell::RefCell<Option<EventLoop<UserEvent>>> = const { core::cell::RefCell::new(None) });

    EVENT_LOOP.with(|event_loop| {
        // Since we want to reference NativeOptions when creating the EventLoop we can't
        // do that as part of the lazy thread local storage initialization and so we instead
        // create the event loop lazily here
        let mut event_loop_lock = event_loop.borrow_mut();
        let event_loop = if let Some(event_loop) = &mut *event_loop_lock {
            event_loop
        } else {
            event_loop_lock.insert(create_event_loop(&mut native_options)?)
        };
        Ok(f(event_loop, native_options))
    })
}

/// Wraps a [`WinitApp`] to implement [`ApplicationHandler`]. This handles redrawing, exit states, and
/// some events, but otherwise forwards events to the [`WinitApp`].
struct WinitAppWrapper<T: WinitApp> {
    windows_next_repaint_times: HashMap<WindowId, Instant>,
    #[cfg(feature = "native-host-seam")]
    native_host: NativeHostState,
    #[cfg(feature = "native-host-seam")]
    native_event_sequence: NativeEventSequencer,
    #[cfg(feature = "native-host-seam")]
    native_host_service_queued: bool,

    /// Windows we asked to redraw, and when to give up waiting for their `RedrawRequested`.
    windows_redraw_deadlines: HashMap<WindowId, Instant>,

    winit_app: T,
    return_result: Result<(), crate::Error>,
    run_and_return: bool,
}

impl<T: WinitApp> WinitAppWrapper<T> {
    fn new(winit_app: T, run_and_return: bool) -> Self {
        #[cfg(feature = "native-host-seam")]
        let native_host = winit_app.native_host_state();
        Self {
            windows_next_repaint_times: HashMap::default(),
            #[cfg(feature = "native-host-seam")]
            native_host,
            #[cfg(feature = "native-host-seam")]
            native_event_sequence: NativeEventSequencer::default(),
            #[cfg(feature = "native-host-seam")]
            native_host_service_queued: false,
            windows_redraw_deadlines: HashMap::default(),
            winit_app,
            return_result: Ok(()),
            run_and_return,
        }
    }

    fn handle_event_result(
        &mut self,
        event_loop: &ActiveEventLoop,
        event_result: Result<EventResult>,
    ) {
        let mut exit = false;
        let mut save = false;

        log::trace!("event_result: {event_result:?}");

        let mut event_result = event_result;

        if cfg!(target_os = "windows")
            && let Ok(EventResult::RepaintNow(window_id)) = event_result
        {
            log::trace!("RepaintNow of {window_id:?}");
            self.windows_next_repaint_times
                .insert(window_id, Instant::now());

            // Fix flickering on Windows, see https://github.com/emilk/egui/pull/2280
            event_result = self
                .winit_app
                .run_ui_and_paint(event_loop, window_id, PassMode::Full);
        }

        let combined_result = event_result.map(|event_result| match event_result {
            EventResult::Wait => {
                event_loop.set_control_flow(ControlFlow::Wait);
                event_result
            }
            EventResult::RepaintNow(window_id) => {
                log::trace!("RepaintNow of {window_id:?}");
                self.windows_next_repaint_times
                    .insert(window_id, Instant::now());
                event_result
            }
            EventResult::RepaintNext(window_id) => {
                log::trace!("RepaintNext of {window_id:?}");
                self.windows_next_repaint_times
                    .insert(window_id, Instant::now());
                event_result
            }
            EventResult::RepaintAt(window_id, repaint_time) => {
                self.windows_next_repaint_times.insert(
                    window_id,
                    self.windows_next_repaint_times
                        .get(&window_id)
                        .map_or(repaint_time, |last| (*last).min(repaint_time)),
                );
                event_result
            }
            EventResult::Save => {
                save = true;
                event_result
            }
            EventResult::Exit => {
                exit = true;
                event_result
            }
            EventResult::CloseRequested => {
                // The windows need to be dropped whilst the event loop is running to allow for proper cleanup.
                self.winit_app.save_and_destroy();
                event_result
            }
        });

        if let Err(err) = combined_result {
            log::error!("Exiting because of error: {err}");
            exit = true;
            self.return_result = Err(err);
        }

        #[cfg(feature = "native-host-seam")]
        self.apply_native_host_root_wake();

        if save {
            log::debug!("Received an EventResult::Save - saving app state");
            self.winit_app.save();
        }

        if exit {
            if self.run_and_return {
                log::debug!("Asking to exit event loop…");
                event_loop.exit();
            } else {
                log::debug!("Quitting - saving app state…");
                self.winit_app.save_and_destroy();

                log::debug!("Exiting with return code 0");

                std::process::exit(0);
            }
        }

        self.check_redraw_requests(event_loop);
        #[cfg(feature = "native-host-seam")]
        self.queue_native_host_service();
    }

    #[cfg(feature = "native-host-seam")]
    fn apply_native_host_root_wake(&self) {
        if self.native_host.take_root_wake_after_current()
            && let Some(ctx) = self.winit_app.egui_ctx()
        {
            log::trace!("native host durable root wake queued after the current event");
            // Renderer terminal callbacks run after egui's pass-local repaint
            // scheduling. Re-enter through egui's EventLoopProxy-backed
            // repaint callback so X11 cannot coalesce the request with the
            // RedrawRequested event which produced this terminal.
            ctx.request_repaint_of(egui::ViewportId::ROOT);
        }
    }

    #[cfg(feature = "native-host-seam")]
    fn queue_native_host_service(&mut self) {
        if !self.native_host_service_queued
            && self.native_host.service_after_current_requested()
            && self.winit_app.egui_ctx().is_some()
        {
            // Keep this claim until the queued event starts. Other native
            // events may request service before that event is dispatched.
            self.native_host_service_queued = true;
            self.winit_app.request_native_host_service();
        }
    }

    #[cfg(feature = "native-host-seam")]
    fn service_native_host(&mut self) {
        self.native_host_service_queued = false;
        if let Some(context) = self.winit_app.egui_ctx() {
            self.native_host.service(context);
        }
        self.apply_native_host_root_wake();
        self.queue_native_host_service();
    }

    fn check_redraw_requests(&mut self, event_loop: &ActiveEventLoop) {
        let now = Instant::now();

        let mut invisible_window_ids = Vec::new();

        self.windows_next_repaint_times
            .retain(|window_id, repaint_time| {
                if now < *repaint_time {
                    return true; // not yet ready
                }

                if let Some(window) = self.winit_app.window(*window_id) {
                    // On Windows, invisible windows don't receive RedrawRequested
                    // events, so pending viewport commands (e.g. Visible(true)) would
                    // never be processed. We collect these windows to paint them
                    // directly below.
                    // See: https://github.com/emilk/egui/issues/5229
                    if is_invisible_or_minimized(&window) {
                        invisible_window_ids.push(*window_id);
                    } else {
                        log::trace!("request_redraw for {window_id:?}");
                        // Don't switch to `ControlFlow::Poll` here. `request_redraw`
                        // is enough to wake the event loop, and on Wayland the
                        // `RedrawRequested` event is only delivered once the
                        // compositor sends a frame callback. Polling in the meantime
                        // busy-loops a whole CPU core.
                        // See https://github.com/emilk/egui/issues/8326.
                        window.request_redraw();
                        self.windows_redraw_deadlines
                            .entry(*window_id)
                            .or_insert(now + MISSED_REDRAW_TIMEOUT);
                    }
                } else {
                    log::trace!("No window found for {window_id:?}");
                }
                false
            });

        // Paint invisible windows directly, since they won't receive
        // RedrawRequested events on Windows. This ensures that viewport
        // commands like Visible(true) are still processed.
        for window_id in &invisible_window_ids {
            let event_result =
                self.winit_app
                    .run_ui_and_paint(event_loop, *window_id, PassMode::Full);
            self.handle_event_result(event_loop, event_result);
        }

        // Throttle any already-scheduled repaints for invisible windows
        // to avoid busy-looping. If no repaint was requested by the app,
        // the window will simply sleep.
        // See: https://github.com/emilk/egui/issues/7776
        if !invisible_window_ids.is_empty() {
            let next_paint = Instant::now() + INVISIBLE_WINDOW_REPAINT_INTERVAL;
            for window_id in &invisible_window_ids {
                self.windows_next_repaint_times
                    .entry(*window_id)
                    .and_modify(|t| *t = (*t).min(next_paint));
            }
        }

        // Keep the app logic running for windows whose `RedrawRequested` is overdue,
        // e.g. because the compositor withholds frame callbacks from a hidden window.
        let overdue_window_ids: Vec<WindowId> = self
            .windows_redraw_deadlines
            .iter()
            .filter(|(_, deadline)| **deadline <= now)
            .map(|(window_id, _)| *window_id)
            .collect();
        for window_id in overdue_window_ids {
            // `handle_event_result` re-enters `check_redraw_requests`, which may already
            // have handled this window, so only run it if the deadline is still there:
            if self.windows_redraw_deadlines.remove(&window_id).is_some() {
                log::trace!("RedrawRequested is overdue for {window_id:?}: running logic only");
                let event_result =
                    self.winit_app
                        .run_ui_and_paint(event_loop, window_id, PassMode::LogicOnly);
                self.handle_event_result(event_loop, event_result);
            }
        }

        // Always set an explicit, sleeping control flow. Previously we only set
        // `WaitUntil` when a repaint was already scheduled, which meant that a
        // `ControlFlow::Poll` set earlier was never undone once the last timed
        // repaint had been consumed, leaving the loop spinning.
        // See https://github.com/emilk/egui/issues/8326.
        let next_repaint_time = core::iter::chain(
            self.windows_next_repaint_times.values(),
            self.windows_redraw_deadlines.values(),
        )
        .min()
        .copied();
        event_loop.set_control_flow(match next_repaint_time {
            Some(next_repaint_time) => ControlFlow::WaitUntil(next_repaint_time),
            None => ControlFlow::Wait,
        });
    }
}

impl<T: WinitApp> ApplicationHandler<UserEvent> for WinitAppWrapper<T> {
    fn suspended(&mut self, event_loop: &ActiveEventLoop) {
        profiling::scope!("Event::Suspended");

        event_loop_context::with_event_loop_context(event_loop, move || {
            let event_result = self.winit_app.suspended(event_loop);
            self.handle_event_result(event_loop, event_result);
        });
    }

    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        profiling::scope!("Event::Resumed");

        // Nb: Make sure this guard is dropped after this function returns.
        event_loop_context::with_event_loop_context(event_loop, move || {
            let event_result = self.winit_app.resumed(event_loop);
            self.handle_event_result(event_loop, event_result);
        });
    }

    fn exiting(&mut self, event_loop: &ActiveEventLoop) {
        // On Mac, Cmd-Q we get here and then `run_app_on_demand` doesn't return (despite its name),
        // so we need to save state now:
        log::debug!("Received Event::LoopExiting - saving app state…");
        event_loop_context::with_event_loop_context(event_loop, move || {
            self.winit_app.save_and_destroy();
        });
    }

    fn device_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        device_id: winit::event::DeviceId,
        event: winit::event::DeviceEvent,
    ) {
        profiling::function_scope!(egui_winit::short_device_event_description(&event));

        // Nb: Make sure this guard is dropped after this function returns.
        event_loop_context::with_event_loop_context(event_loop, move || {
            #[cfg(feature = "native-host-seam")]
            if self.native_host.is_enabled() && matches!(event, winit::event::DeviceEvent::Removed)
            {
                let ordinal = self.native_event_sequence.next();
                self.native_host
                    .observe_device_removal(self.winit_app.egui_ctx(), ordinal);
            }
            let event_result = self.winit_app.device_event(event_loop, device_id, event);
            self.handle_event_result(event_loop, event_result);
        });
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: UserEvent) {
        profiling::function_scope!(match &event {
            #[cfg(feature = "native-host-seam")]
            UserEvent::NativeHostService => "UserEvent::NativeHostService",
            UserEvent::RequestRepaint { .. } => "UserEvent::RequestRepaint",
            #[cfg(feature = "accesskit")]
            UserEvent::AccessKitActionRequest(_) => "UserEvent::AccessKitActionRequest",
        });

        event_loop_context::with_event_loop_context(event_loop, move || {
            let event_result = match event {
                #[cfg(feature = "native-host-seam")]
                UserEvent::NativeHostService => {
                    self.service_native_host();
                    // Service never enters redraw checks, which may run UI
                    // for invisible windows or overdue redraws. Repaint wakes
                    // return through their own ordinary native events.
                    return;
                }
                UserEvent::RequestRepaint {
                    when,
                    cumulative_pass_nr,
                    viewport_id,
                } => {
                    let current_pass_nr = self
                        .winit_app
                        .egui_ctx()
                        .map_or(0, |ctx| ctx.cumulative_pass_nr_for(viewport_id));
                    if current_pass_nr == cumulative_pass_nr
                        || current_pass_nr == cumulative_pass_nr + 1
                    {
                        log::trace!("UserEvent::RequestRepaint scheduling repaint at {when:?}");
                        if let Some(window_id) =
                            self.winit_app.window_id_from_viewport_id(viewport_id)
                        {
                            // Throttle repaints for invisible windows to prevent
                            // high CPU usage on Windows.
                            // See: https://github.com/emilk/egui/issues/7776
                            let when = if let Some(window) = self.winit_app.window(window_id)
                                && is_invisible_or_minimized(&window)
                            {
                                when.max(Instant::now() + INVISIBLE_WINDOW_REPAINT_INTERVAL)
                            } else {
                                when
                            };
                            Ok(EventResult::RepaintAt(window_id, when))
                        } else {
                            Ok(EventResult::Wait)
                        }
                    } else {
                        log::trace!("Got outdated UserEvent::RequestRepaint");
                        Ok(EventResult::Wait) // old request - we've already repainted
                    }
                }
                #[cfg(feature = "accesskit")]
                UserEvent::AccessKitActionRequest(request) => {
                    self.winit_app.on_accesskit_event(request)
                }
            };
            self.handle_event_result(event_loop, event_result);
        });
    }

    fn new_events(&mut self, event_loop: &ActiveEventLoop, cause: winit::event::StartCause) {
        if let winit::event::StartCause::ResumeTimeReached { .. } = cause {
            log::trace!("Woke up to check next_repaint_time");
        }

        self.check_redraw_requests(event_loop);
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        window_id: WindowId,
        event: winit::event::WindowEvent,
    ) {
        profiling::function_scope!(egui_winit::short_window_event_description(&event));

        // Nb: Make sure this guard is dropped after this function returns.
        event_loop_context::with_event_loop_context(event_loop, move || {
            let event_result = match event {
                winit::event::WindowEvent::RedrawRequested => {
                    self.windows_redraw_deadlines.remove(&window_id);
                    self.winit_app
                        .run_ui_and_paint(event_loop, window_id, PassMode::Full)
                }
                event => {
                    #[cfg(feature = "native-host-seam")]
                    let native_event = if self.native_host.is_enabled() {
                        let ordinal = self.native_event_sequence.next();
                        let viewport_id = self.winit_app.viewport_id_from_window_id(window_id);
                        let ctx = self.winit_app.egui_ctx();
                        let local_geometry = ctx.and_then(|ctx| {
                            self.winit_app
                                .window(window_id)
                                .map(|window| NativeWindowLocalGeometry::capture(ctx, &window))
                        });
                        let disposition = self.native_host.observe_window_event(
                            ctx,
                            ordinal,
                            window_id,
                            viewport_id,
                            local_geometry,
                            &event,
                        );
                        Some((
                            ordinal,
                            matches!(event, winit::event::WindowEvent::Focused(_)),
                            disposition,
                        ))
                    } else {
                        None
                    };
                    #[cfg(feature = "native-host-seam")]
                    let held_close = native_event.is_some_and(|(_, _, disposition)| {
                        matches!(
                            disposition,
                            super::host_seam::NativeWindowEventDisposition::HoldClose(_)
                        )
                    });
                    #[cfg(not(feature = "native-host-seam"))]
                    let held_close = false;
                    let event_result = if held_close {
                        Ok(EventResult::RepaintNext(window_id))
                    } else {
                        self.winit_app.window_event(event_loop, window_id, event)
                    };
                    #[cfg(feature = "native-host-seam")]
                    if let Some((ordinal, true, _)) = native_event {
                        self.native_host.observe_global_focus(
                            self.winit_app.egui_ctx(),
                            ordinal,
                            self.winit_app.focused_native_viewport(),
                        );
                    }
                    event_result
                }
            };

            self.handle_event_result(event_loop, event_result);
        });
    }
}

#[cfg(not(target_os = "ios"))]
fn run_and_return(event_loop: &mut EventLoop<UserEvent>, winit_app: impl WinitApp) -> Result {
    use winit::platform::run_on_demand::EventLoopExtRunOnDemand as _;

    log::trace!("Entering the winit event loop (run_app_on_demand)…");

    let mut app = WinitAppWrapper::new(winit_app, true);
    event_loop.run_app_on_demand(&mut app)?;
    log::debug!("eframe window closed");
    app.return_result
}

fn run_and_exit(event_loop: EventLoop<UserEvent>, winit_app: impl WinitApp) -> Result {
    log::trace!("Entering the winit event loop (run_app)…");

    // When to repaint what window
    let mut app = WinitAppWrapper::new(winit_app, false);
    event_loop.run_app(&mut app)?;

    log::debug!("winit event loop unexpectedly returned");
    Ok(())
}

// ----------------------------------------------------------------------------

#[cfg(feature = "glow")]
pub fn run_glow(
    app_name: &str,
    mut native_options: epi::NativeOptions,
    egui_ctx: Option<egui::Context>,
    app_creator: epi::AppCreator<'_>,
) -> Result {
    use super::glow_integration::GlowWinitApp;

    #[cfg(not(target_os = "ios"))]
    if native_options.run_and_return {
        return with_event_loop(native_options, |event_loop, native_options| {
            let glow_eframe =
                GlowWinitApp::new(event_loop, app_name, native_options, egui_ctx, app_creator);
            run_and_return(event_loop, glow_eframe)
        })?;
    }

    let event_loop = create_event_loop(&mut native_options)?;
    let glow_eframe =
        GlowWinitApp::new(&event_loop, app_name, native_options, egui_ctx, app_creator);
    run_and_exit(event_loop, glow_eframe)
}

#[cfg(feature = "glow")]
pub fn create_glow<'a>(
    app_name: &str,
    native_options: epi::NativeOptions,
    app_creator: epi::AppCreator<'a>,
    event_loop: &EventLoop<UserEvent>,
) -> impl ApplicationHandler<UserEvent> + 'a {
    use super::glow_integration::GlowWinitApp;

    let glow_eframe = GlowWinitApp::new(event_loop, app_name, native_options, None, app_creator);
    WinitAppWrapper::new(glow_eframe, true)
}

// ----------------------------------------------------------------------------

#[cfg(feature = "wgpu_no_default_features")]
pub fn run_wgpu(
    app_name: &str,
    mut native_options: epi::NativeOptions,
    egui_ctx: Option<egui::Context>,
    app_creator: epi::AppCreator<'_>,
) -> Result {
    use super::wgpu_integration::WgpuWinitApp;

    #[cfg(not(target_os = "ios"))]
    if native_options.run_and_return {
        return with_event_loop(native_options, |event_loop, native_options| {
            let wgpu_eframe =
                WgpuWinitApp::new(event_loop, app_name, native_options, egui_ctx, app_creator);
            run_and_return(event_loop, wgpu_eframe)
        })?;
    }

    let event_loop = create_event_loop(&mut native_options)?;
    let wgpu_eframe =
        WgpuWinitApp::new(&event_loop, app_name, native_options, egui_ctx, app_creator);
    run_and_exit(event_loop, wgpu_eframe)
}

#[cfg(feature = "wgpu_no_default_features")]
pub fn create_wgpu<'a>(
    app_name: &str,
    native_options: epi::NativeOptions,
    app_creator: epi::AppCreator<'a>,
    event_loop: &EventLoop<UserEvent>,
) -> impl ApplicationHandler<UserEvent> + 'a {
    use super::wgpu_integration::WgpuWinitApp;

    let wgpu_eframe = WgpuWinitApp::new(event_loop, app_name, native_options, None, app_creator);
    WinitAppWrapper::new(wgpu_eframe, true)
}

// ----------------------------------------------------------------------------

/// A proxy to the eframe application that implements [`ApplicationHandler`].
///
/// This can be run directly on your own [`EventLoop`] by itself or with other
/// windows you manage outside of eframe.
pub struct EframeWinitApplication<'a> {
    wrapper: Box<dyn ApplicationHandler<UserEvent> + 'a>,
    control_flow: ControlFlow,
}

impl ApplicationHandler<UserEvent> for EframeWinitApplication<'_> {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        self.wrapper.resumed(event_loop);
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        window_id: winit::window::WindowId,
        event: winit::event::WindowEvent,
    ) {
        self.wrapper.window_event(event_loop, window_id, event);
    }

    fn new_events(&mut self, event_loop: &ActiveEventLoop, cause: winit::event::StartCause) {
        self.wrapper.new_events(event_loop, cause);
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: UserEvent) {
        self.wrapper.user_event(event_loop, event);
    }

    fn device_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        device_id: winit::event::DeviceId,
        event: winit::event::DeviceEvent,
    ) {
        self.wrapper.device_event(event_loop, device_id, event);
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        self.wrapper.about_to_wait(event_loop);
        self.control_flow = event_loop.control_flow();
    }

    fn suspended(&mut self, event_loop: &ActiveEventLoop) {
        self.wrapper.suspended(event_loop);
    }

    fn exiting(&mut self, event_loop: &ActiveEventLoop) {
        self.wrapper.exiting(event_loop);
    }

    fn memory_warning(&mut self, event_loop: &ActiveEventLoop) {
        self.wrapper.memory_warning(event_loop);
    }
}

impl<'a> EframeWinitApplication<'a> {
    pub(crate) fn new<T: ApplicationHandler<UserEvent> + 'a>(app: T) -> Self {
        Self {
            wrapper: Box::new(app),
            control_flow: ControlFlow::default(),
        }
    }

    /// Pump the `EventLoop` to check for and dispatch pending events to this application.
    ///
    /// Returns either the exit code for the application or the final state of the [`ControlFlow`]
    /// after all events have been dispatched in this iteration.
    ///
    /// This is useful when your [`EventLoop`] is not the main event loop for your application.
    /// See the `external_eventloop_async` example.
    #[cfg(not(target_os = "ios"))]
    pub fn pump_eframe_app(
        &mut self,
        event_loop: &mut EventLoop<UserEvent>,
        timeout: Option<core::time::Duration>,
    ) -> EframePumpStatus {
        use winit::platform::pump_events::{EventLoopExtPumpEvents as _, PumpStatus};

        match event_loop.pump_app_events(timeout, self) {
            PumpStatus::Continue => EframePumpStatus::Continue(self.control_flow),
            PumpStatus::Exit(code) => EframePumpStatus::Exit(code),
        }
    }
}

/// Either an exit code or a [`ControlFlow`] from the [`ActiveEventLoop`].
///
/// The result of [`EframeWinitApplication::pump_eframe_app`].
#[cfg(not(target_os = "ios"))]
pub enum EframePumpStatus {
    /// The final state of the [`ControlFlow`] after all events have been dispatched
    ///
    /// Callers should perform the action that is appropriate for the [`ControlFlow`] value.
    Continue(ControlFlow),

    /// The exit code for the application
    Exit(i32),
}

#[cfg(all(test, feature = "native-host-seam"))]
mod native_host_service_tests {
    use std::cell::RefCell;
    use std::collections::VecDeque;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use super::*;
    use crate::native::host_seam::{
        NativeGlobalFocusObservation, NativeHostHandler, NativeHostWake, NativeOutputAdmission,
        NativeOutputToken, NativeViewportRoster, NativeWindowSnapshot, current_native_output_token,
    };

    #[derive(Default)]
    struct ServiceHost {
        calls: AtomicUsize,
        retry: AtomicBool,
    }

    impl NativeHostHandler for ServiceHost {
        fn on_global_focus(&self, _: NativeGlobalFocusObservation) -> NativeHostWake {
            NativeHostWake::ServiceAfterCurrent
        }

        fn on_service(&self, _: &egui::Context) -> NativeHostWake {
            assert_eq!(current_native_output_token(), None);
            self.calls.fetch_add(1, Ordering::Relaxed);
            if self.retry.swap(false, Ordering::Relaxed) {
                NativeHostWake::ServiceAfterCurrent
            } else {
                NativeHostWake::Wait
            }
        }

        fn on_output_begin(
            &self,
            _: NativeOutputToken,
            _: NativeWindowSnapshot,
            _: Option<NativeViewportRoster<'_>>,
        ) -> NativeOutputAdmission {
            panic!("service must not open an output scope")
        }
    }

    struct ServiceApp {
        native_host: NativeHostState,
        context: egui::Context,
        events: RefCell<VecDeque<UserEvent>>,
        input: egui::RawInput,
    }

    impl WinitApp for ServiceApp {
        fn egui_ctx(&self) -> Option<&egui::Context> {
            Some(&self.context)
        }

        fn native_host_state(&self) -> NativeHostState {
            self.native_host.clone()
        }

        fn request_native_host_service(&self) {
            self.events
                .borrow_mut()
                .push_back(UserEvent::NativeHostService);
        }

        fn window(&self, _: WindowId) -> Option<Arc<winit::window::Window>> {
            panic!("service must not inspect redraw requests")
        }

        fn window_id_from_viewport_id(&self, _: egui::ViewportId) -> Option<WindowId> {
            None
        }

        fn viewport_id_from_window_id(&self, _: WindowId) -> Option<egui::ViewportId> {
            None
        }

        fn focused_native_viewport(&self) -> Option<(egui::ViewportId, WindowId)> {
            None
        }

        fn save(&mut self) {
            panic!("service must not save the application")
        }

        fn save_and_destroy(&mut self) {
            panic!("service must not destroy the application")
        }

        fn run_ui_and_paint(
            &mut self,
            _: &ActiveEventLoop,
            _: WindowId,
            _: PassMode,
        ) -> crate::Result<EventResult> {
            panic!("service must not extract input or run application logic or UI")
        }

        fn suspended(&mut self, _: &ActiveEventLoop) -> crate::Result<EventResult> {
            unreachable!()
        }

        fn resumed(&mut self, _: &ActiveEventLoop) -> crate::Result<EventResult> {
            unreachable!()
        }

        fn device_event(
            &mut self,
            _: &ActiveEventLoop,
            _: winit::event::DeviceId,
            _: winit::event::DeviceEvent,
        ) -> crate::Result<EventResult> {
            unreachable!()
        }

        fn window_event(
            &mut self,
            _: &ActiveEventLoop,
            _: WindowId,
            _: winit::event::WindowEvent,
        ) -> crate::Result<EventResult> {
            unreachable!()
        }

        #[cfg(feature = "accesskit")]
        fn on_accesskit_event(
            &mut self,
            _: egui_winit::accesskit_winit::Event,
        ) -> crate::Result<EventResult> {
            unreachable!()
        }
    }

    fn fixture() -> (Arc<ServiceHost>, WinitAppWrapper<ServiceApp>) {
        #[derive(Debug)]
        struct BufferedFile;

        impl egui::DroppedFile for BufferedFile {
            fn path(&self) -> &std::path::Path {
                std::path::Path::new("buffered-file.txt")
            }

            fn bytes(&self) -> Result<Vec<u8>, String> {
                panic!("host service must not read a buffered dropped file")
            }
        }

        let host = Arc::new(ServiceHost::default());
        let native_host = NativeHostState::new(Some(host.clone()));
        let context = egui::Context::default();
        let mut output = context.run_ui(Default::default(), |_| {});
        output.textures_delta.clear();
        let app = ServiceApp {
            native_host,
            context,
            events: RefCell::default(),
            input: egui::RawInput {
                events: vec![egui::Event::Text("buffered input".into())],
                hovered_files: vec![egui::HoveredFile::default()],
                dropped_files: vec![Arc::new(BufferedFile)],
                ..Default::default()
            },
        };
        (host, WinitAppWrapper::new(app, true))
    }

    fn request_service(wrapper: &mut WinitAppWrapper<ServiceApp>) {
        let ordinal = wrapper.native_event_sequence.next();
        wrapper
            .native_host
            .observe_global_focus(Some(&wrapper.winit_app.context), ordinal, None);
        wrapper.queue_native_host_service();
    }

    fn dispatch_service(wrapper: &mut WinitAppWrapper<ServiceApp>) {
        assert!(matches!(
            wrapper.winit_app.events.borrow_mut().pop_front(),
            Some(UserEvent::NativeHostService)
        ));
        wrapper.service_native_host();
    }

    #[test]
    fn queued_service_coalesces_requests_and_preserves_ui_input_and_pass_number() {
        let (host, mut wrapper) = fixture();
        let pass = wrapper.winit_app.context.cumulative_pass_nr();
        let window = WindowId::from(1);
        let overdue = Instant::now() - Duration::from_secs(1);
        wrapper.windows_next_repaint_times.insert(window, overdue);
        wrapper.windows_redraw_deadlines.insert(window, overdue);

        for _ in 0..3 {
            request_service(&mut wrapper);
            assert_eq!(wrapper.winit_app.events.borrow().len(), 1);
            assert!(wrapper.native_host_service_queued);
            assert!(wrapper.native_host.service_after_current_requested());
            assert_eq!(host.calls.load(Ordering::Relaxed), 0);
        }
        dispatch_service(&mut wrapper);
        wrapper.queue_native_host_service();
        wrapper.service_native_host();

        assert_eq!(host.calls.load(Ordering::Relaxed), 1);
        assert!(!wrapper.native_host_service_queued);
        assert!(!wrapper.native_host.service_after_current_requested());
        assert!(wrapper.winit_app.events.borrow().is_empty());
        assert_eq!(wrapper.winit_app.context.cumulative_pass_nr(), pass);
        assert_eq!(
            wrapper.winit_app.input.events,
            [egui::Event::Text("buffered input".into())]
        );
        assert_eq!(wrapper.winit_app.input.hovered_files.len(), 1);
        assert_eq!(wrapper.winit_app.input.dropped_files.len(), 1);
        assert_eq!(
            wrapper.windows_next_repaint_times.get(&window),
            Some(&overdue)
        );
        assert_eq!(
            wrapper.windows_redraw_deadlines.get(&window),
            Some(&overdue)
        );
        assert_eq!(current_native_output_token(), None);
    }

    #[test]
    fn service_retry_waits_for_a_later_event_and_keeps_one_queue_claim() {
        let (host, mut wrapper) = fixture();
        host.retry.store(true, Ordering::Relaxed);
        request_service(&mut wrapper);
        dispatch_service(&mut wrapper);

        assert_eq!(host.calls.load(Ordering::Relaxed), 1);
        assert!(wrapper.native_host_service_queued);
        assert!(wrapper.native_host.service_after_current_requested());
        assert_eq!(wrapper.winit_app.events.borrow().len(), 1);
        request_service(&mut wrapper);
        assert_eq!(wrapper.winit_app.events.borrow().len(), 1);

        dispatch_service(&mut wrapper);
        assert_eq!(host.calls.load(Ordering::Relaxed), 2);
        assert!(!wrapper.native_host_service_queued);
        assert!(!wrapper.native_host.service_after_current_requested());
        assert!(wrapper.winit_app.events.borrow().is_empty());
    }
}

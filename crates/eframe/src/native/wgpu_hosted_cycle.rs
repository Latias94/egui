//! Complete-roster hosted-viewport coordination for the WGPU backend.
//!
//! Window, surface, and painter lifecycles remain in `wgpu_integration`. This
//! module owns the transaction from a frozen physical input roster through
//! staged output consolidation, renderer setup, commit, or terminal abort.

use std::{cell::RefCell, rc::Rc, sync::Arc};

use egui::{
    DeferredViewportUiCallback, FullOutput, OrderedViewportIdMap, ViewportId, ViewportInfo,
    ViewportOutput,
};
use egui_winit::ActionRequested;
use winit::event_loop::ActiveEventLoop;

use crate::{
    App, HostedViewportCycle, HostedViewportCycleAbort, HostedViewportCycleDriver,
    HostedViewportCycleError, HostedViewportInput, HostedViewportOutput,
    HostedViewportTransactionGuard, Result,
    native::{
        PendingPresentation, PresentationResults,
        epi_integration::EpiIntegration,
        hosted_cycle::{
            HostedTextureSynchronizer, PreparedNativeViewportSchedule, arm_hosted_presentations,
            consolidate_hosted_viewport_outputs, is_active_output_owner,
            schedule_native_viewport_creates, should_present_hosted_output,
        },
        winit_integration::{EventResult, is_invisible_or_minimized},
    },
};

use super::wgpu_integration::{SharedState, Viewport, WgpuWinitRunning, handle_viewport_output};

struct WgpuHostedViewport {
    viewport_ui_cb: Option<Arc<DeferredViewportUiCallback>>,
    is_visible: bool,
}

struct FrozenWgpuHostedCycle {
    raw_inputs: Vec<egui::RawInput>,
    callback_order: Vec<ViewportId>,
    viewports: OrderedViewportIdMap<WgpuHostedViewport>,
    native_ingress: crate::NativeHostIngress,
    native_staging_presentations: Vec<crate::NativeViewportBinding>,
    native_effect_sink: crate::NativeEffectSink,
    native_viewport_create_sink: crate::NativeViewportCreateSink,
    native_ingress_settlement: super::platform_ingress_owner::NativeHostIngressSettlement,
}

struct SealedWgpuHostedCycle {
    outputs: Vec<HostedViewportOutput<FullOutput>>,
    transaction_guard: HostedViewportTransactionGuard,
    active_viewports: OrderedViewportIdMap<ViewportOutput>,
    frozen_viewports: OrderedViewportIdMap<WgpuHostedViewport>,
    render_state: egui_wgpu::RenderState,
    native_effect_sink: crate::NativeEffectSink,
    native_viewport_create_sink: crate::NativeViewportCreateSink,
    viewport_create_schedules: Vec<PreparedNativeViewportSchedule>,
}

struct WgpuHostedCycleUiDriver<'a> {
    integration: &'a mut EpiIntegration,
    app: &'a mut dyn App,
    hosted_viewports: &'a OrderedViewportIdMap<WgpuHostedViewport>,
}

impl HostedViewportCycleDriver for WgpuHostedCycleUiDriver<'_> {
    type Output = FullOutput;

    fn begin(&mut self, cycle: &HostedViewportCycle) -> crate::HostedViewportAppResult<()> {
        self.integration
            .begin_hosted_viewport_cycle(self.app, cycle)
    }

    fn run_viewport(
        &mut self,
        input: HostedViewportInput,
    ) -> crate::HostedViewportAppResult<Self::Output> {
        let (viewport_id, raw_input) = input.into_parts();
        let Some(viewport) = self.hosted_viewports.get(&viewport_id) else {
            return Err(std::io::Error::other(format!(
                "hosted WGPU viewport {viewport_id:?} has no frozen metadata"
            ))
            .into());
        };
        self.integration
            .update_prepared(self.app, viewport.viewport_ui_cb.as_deref(), raw_input)
    }

    fn end(
        &mut self,
        outputs: &mut [HostedViewportOutput<Self::Output>],
    ) -> crate::HostedViewportAppResult<()> {
        self.integration
            .end_hosted_viewport_cycle(self.app, outputs)
    }

    fn abort(&mut self) {
        self.integration
            .abort_application_hosted_viewport_cycle(self.app);
    }
}

struct WgpuHostedCommitter<'a> {
    integration: &'a mut EpiIntegration,
    app: &'a mut dyn App,
    shared: &'a Rc<RefCell<SharedState>>,
    event_loop: &'a ActiveEventLoop,
    presentation_results: &'a PresentationResults,
    frame_timer: &'a mut crate::stopwatch::Stopwatch,
}

/// Runs one complete hosted WGPU transaction.
///
/// No WGPU viewport borrow survives a user callback. Every staged output is
/// either committed against the consolidated active roster or retained in a
/// typed fatal abort after terminal presentation and texture settlement.
pub(super) fn run(
    running: &mut WgpuWinitRunning<'_>,
    event_loop: &ActiveEventLoop,
) -> Result<EventResult> {
    profiling::function_scope!();

    let presentation_results = running.shared.borrow().presentation_results.clone();
    presentation_results.drain();

    profiling::finish_frame!();

    let mut frame_timer = crate::stopwatch::Stopwatch::new();
    frame_timer.start();

    let frozen_cycle = {
        profiling::scope!("Prepare");
        let mut shared = running.shared.borrow_mut();
        freeze_complete_roster(&mut shared, &running.integration.egui_ctx)?
    };
    let Some(frozen_cycle) = frozen_cycle else {
        return Ok(EventResult::Wait);
    };

    let sealed_cycle = stage_and_seal(
        &mut running.integration,
        running.app.as_mut(),
        &running.shared,
        frozen_cycle,
        &presentation_results,
    )?;

    let result = WgpuHostedCommitter {
        integration: &mut running.integration,
        app: running.app.as_mut(),
        shared: &running.shared,
        event_loop,
        presentation_results: &presentation_results,
        frame_timer: &mut frame_timer,
    }
    .commit(sealed_cycle);
    presentation_results.drain();
    Ok(result?)
}

fn freeze_complete_roster(
    shared: &mut SharedState,
    egui_ctx: &egui::Context,
) -> std::result::Result<Option<FrozenWgpuHostedCycle>, HostedViewportCycleError> {
    let mut callback_order = shared
        .viewports
        .iter()
        .filter_map(|(viewport_id, viewport)| {
            let is_hosted = *viewport_id == ViewportId::ROOT || viewport.viewport_ui_cb.is_some();
            (is_hosted && viewport.window.is_some() && viewport.egui_winit.is_some())
                .then_some(*viewport_id)
        })
        .collect::<Vec<_>>();
    if !callback_order.contains(&ViewportId::ROOT) {
        return Ok(None);
    }
    callback_order.sort();
    callback_order.retain(|viewport_id| *viewport_id != ViewportId::ROOT);
    callback_order.insert(0, ViewportId::ROOT);

    for viewport_id in &callback_order {
        let viewport = shared.viewports.get_mut(viewport_id).ok_or_else(|| {
            std::io::Error::other(format!(
                "WGPU hosted viewport {viewport_id:?} vanished while freezing its roster"
            ))
        })?;
        let window = viewport.window.as_ref().ok_or_else(|| {
            std::io::Error::other(format!(
                "WGPU hosted viewport {viewport_id:?} lost its window while freezing its roster"
            ))
        })?;
        egui_winit::update_viewport_info(&mut viewport.info, egui_ctx, window, false);
    }

    let inventory = shared
        .viewports
        .iter()
        .map(|(viewport_id, viewport)| (*viewport_id, viewport.info.clone()))
        .collect::<egui::ViewportIdMap<ViewportInfo>>();
    let mut hosted_viewports = OrderedViewportIdMap::new();
    for viewport_id in &callback_order {
        let viewport = shared.viewports.get(viewport_id).ok_or_else(|| {
            std::io::Error::other(format!(
                "WGPU hosted viewport {viewport_id:?} vanished before metadata collection"
            ))
        })?;
        hosted_viewports.insert(
            *viewport_id,
            WgpuHostedViewport {
                viewport_ui_cb: viewport.viewport_ui_cb.clone(),
                is_visible: viewport.info.visible().unwrap_or(true),
            },
        );
    }

    let mut raw_inputs = Vec::with_capacity(callback_order.len());
    for viewport_id in &callback_order {
        let viewport = shared.viewports.get_mut(viewport_id).ok_or_else(|| {
            std::io::Error::other(format!(
                "WGPU hosted viewport {viewport_id:?} vanished before input collection"
            ))
        })?;
        let window = viewport.window.as_ref().ok_or_else(|| {
            std::io::Error::other(format!(
                "WGPU hosted viewport {viewport_id:?} lost its window before input collection"
            ))
        })?;
        let egui_winit = viewport.egui_winit.as_mut().ok_or_else(|| {
            std::io::Error::other(format!(
                "WGPU hosted viewport {viewport_id:?} lost its input state before collection"
            ))
        })?;
        let mut raw_input = egui_winit.take_egui_input(window);
        raw_input.viewports.clone_from(&inventory);
        shared.painter.handle_screenshots(&mut raw_input.events);
        raw_inputs.push(raw_input);
    }

    let native_windows = shared
        .viewports
        .iter()
        .filter_map(|(viewport_id, viewport)| {
            viewport
                .window
                .as_ref()
                .map(|window| (*viewport_id, Arc::clone(window)))
        })
        .collect::<Vec<_>>();
    let frozen_native_ingress = shared
        .platform_ingress
        .freeze(native_windows, egui_ctx)
        .map_err(std::io::Error::other)?;
    let (
        native_ingress,
        native_staging_presentations,
        native_effect_sink,
        native_viewport_create_sink,
        native_ingress_settlement,
    ) = frozen_native_ingress.into_parts();

    Ok(Some(FrozenWgpuHostedCycle {
        raw_inputs,
        callback_order,
        viewports: hosted_viewports,
        native_ingress,
        native_staging_presentations,
        native_effect_sink,
        native_viewport_create_sink,
        native_ingress_settlement,
    }))
}

fn stage_and_seal(
    integration: &mut EpiIntegration,
    app: &mut dyn App,
    shared: &Rc<RefCell<SharedState>>,
    frozen_cycle: FrozenWgpuHostedCycle,
    presentation_results: &PresentationResults,
) -> Result<SealedWgpuHostedCycle> {
    let FrozenWgpuHostedCycle {
        raw_inputs,
        callback_order,
        viewports,
        native_ingress,
        native_staging_presentations,
        native_effect_sink,
        native_viewport_create_sink,
        native_ingress_settlement,
    } = frozen_cycle;

    let hosted_viewport_mode = app.hosted_viewport_mode();
    let transaction_guard = HostedViewportTransactionGuard::enter(hosted_viewport_mode);
    integration.pre_update();
    let prepared_inputs = raw_inputs
        .into_iter()
        .map(|raw_input| integration.prepare_raw_input(app, raw_input))
        .collect::<Vec<_>>();
    let cycle = HostedViewportCycle::with_native_runtime_ingress(
        prepared_inputs,
        native_ingress,
        native_staging_presentations,
        native_effect_sink,
        native_viewport_create_sink,
    )?;
    let retained_effect_sink = cycle
        .native_effect_sink()
        .cloned()
        .expect("native runtime cycle must expose its effect sink");
    let retained_viewport_create_sink = cycle
        .native_viewport_create_sink()
        .cloned()
        .expect("native runtime cycle must expose its viewport creation sink");

    // User callbacks may recursively request immediate viewports, so no shared
    // WGPU viewport state may be borrowed while this transaction is driven.
    let drive_result = {
        let mut driver = WgpuHostedCycleUiDriver {
            integration,
            app,
            hosted_viewports: &viewports,
        };
        cycle.drive_with_guard(transaction_guard, callback_order, &mut driver)
    };
    let completed = match drive_result {
        Ok(completed) => completed,
        Err(abort) => {
            let render_state = shared.borrow().painter.render_state();
            return Err(finish_abort(
                abort,
                integration,
                presentation_results,
                render_state.as_ref(),
            ));
        }
    };
    let (mut outputs, transaction_guard) = completed.into_parts();

    let viewport_outputs = outputs
        .iter()
        .map(|output| {
            (
                output.viewport_id(),
                output.output().viewport_output.clone(),
            )
        })
        .collect::<Vec<_>>();
    let mut active_viewports = match consolidate_hosted_viewport_outputs(&viewport_outputs) {
        Ok(active_viewports) => active_viewports,
        Err(error) => {
            retained_effect_sink.close_and_cancel();
            retained_viewport_create_sink.close_and_cancel();
            let render_state = shared.borrow().painter.render_state();
            return Err(abort_completed_cycle(
                error.into(),
                outputs,
                integration,
                app,
                presentation_results,
                render_state.as_ref(),
            ));
        }
    };
    let Some(render_state) = shared.borrow().painter.render_state() else {
        retained_effect_sink.close_and_cancel();
        retained_viewport_create_sink.close_and_cancel();
        return Err(abort_completed_cycle(
            std::io::Error::other("WGPU renderer state is unavailable after hosted cycle seal")
                .into(),
            outputs,
            integration,
            app,
            presentation_results,
            None,
        ));
    };

    if let Err(error) = configure_active_surfaces(shared, &outputs, &active_viewports) {
        retained_effect_sink.close_and_cancel();
        retained_viewport_create_sink.close_and_cancel();
        return Err(abort_completed_cycle(
            HostedViewportCycleError::Runtime {
                source: Box::new(error),
            },
            outputs,
            integration,
            app,
            presentation_results,
            Some(&render_state),
        ));
    }
    let viewport_create_schedules =
        schedule_native_viewport_creates(&mut active_viewports, &retained_viewport_create_sink);
    let hosted_commit = match integration.prepare_hosted_viewport_cycle(&active_viewports) {
        Ok(prepared) => prepared,
        Err(error) => {
            retained_effect_sink.close_and_cancel();
            for schedule in viewport_create_schedules {
                schedule.cancel(&retained_viewport_create_sink);
            }
            return Err(abort_completed_cycle(
                error.into(),
                outputs,
                integration,
                app,
                presentation_results,
                Some(&render_state),
            ));
        }
    };
    if let Some(violation) = transaction_guard.violations().first().copied() {
        retained_effect_sink.close_and_cancel();
        for schedule in viewport_create_schedules {
            schedule.cancel(&retained_viewport_create_sink);
        }
        return Err(abort_completed_cycle(
            HostedViewportCycleError::ImmediateViewportAttempt {
                viewport_id: violation.viewport_id(),
            },
            outputs,
            integration,
            app,
            presentation_results,
            Some(&render_state),
        ));
    }
    if let Err(source) = integration.commit_application_hosted_viewport_cycle(app, &mut outputs) {
        retained_effect_sink.close_and_cancel();
        for schedule in viewport_create_schedules {
            schedule.cancel(&retained_viewport_create_sink);
        }
        return Err(abort_completed_cycle(
            HostedViewportCycleError::CommitHook { source },
            outputs,
            integration,
            app,
            presentation_results,
            Some(&render_state),
        ));
    }
    if let Err(error) = native_ingress_settlement.commit() {
        retained_effect_sink.close_and_cancel();
        for schedule in viewport_create_schedules {
            schedule.cancel(&retained_viewport_create_sink);
        }
        return Err(abort_completed_cycle(
            HostedViewportCycleError::Runtime {
                source: Box::new(error),
            },
            outputs,
            integration,
            app,
            presentation_results,
            Some(&render_state),
        ));
    }
    integration.commit_hosted_viewport_cycle(hosted_commit);

    Ok(SealedWgpuHostedCycle {
        outputs,
        transaction_guard,
        active_viewports,
        frozen_viewports: viewports,
        render_state,
        native_effect_sink: retained_effect_sink,
        native_viewport_create_sink: retained_viewport_create_sink,
        viewport_create_schedules,
    })
}

fn configure_active_surfaces(
    shared: &Rc<RefCell<SharedState>>,
    outputs: &[HostedViewportOutput<FullOutput>],
    active_viewports: &OrderedViewportIdMap<ViewportOutput>,
) -> std::result::Result<(), egui_wgpu::WgpuError> {
    let mut shared = shared.borrow_mut();
    for output in outputs {
        let viewport_id = output.viewport_id();
        if !is_active_output_owner(active_viewports, viewport_id) {
            continue;
        }
        let Some(window) = shared
            .viewports
            .get(&viewport_id)
            .and_then(|viewport| viewport.window.as_ref())
        else {
            continue;
        };
        let window = Arc::clone(window);
        profiling::scope!("set_window");
        pollster::block_on(shared.painter.set_window(viewport_id, Some(window)))?;
    }
    Ok(())
}

fn abort_completed_cycle(
    error: HostedViewportCycleError,
    outputs: Vec<HostedViewportOutput<FullOutput>>,
    integration: &mut EpiIntegration,
    app: &mut dyn App,
    presentation_results: &PresentationResults,
    render_state: Option<&egui_wgpu::RenderState>,
) -> crate::Error {
    integration.abort_application_hosted_viewport_cycle(app);
    let mut abort = HostedViewportCycleAbort::from_error(error);
    abort.append_staged_outputs(outputs);
    finish_abort(abort, integration, presentation_results, render_state)
}

fn finish_abort(
    mut abort: HostedViewportCycleAbort<FullOutput>,
    integration: &mut EpiIntegration,
    presentation_results: &PresentationResults,
    render_state: Option<&egui_wgpu::RenderState>,
) -> crate::Error {
    abort.append_staged_outputs(integration.abort_hosted_viewport_cycle());
    settle_aborted_outputs(
        abort.staged_outputs_mut(),
        presentation_results,
        render_state,
    );
    presentation_results.drain();
    abort.into()
}

impl WgpuHostedCommitter<'_> {
    fn commit(self, sealed_cycle: SealedWgpuHostedCycle) -> Result<EventResult> {
        let Self {
            integration,
            app,
            shared,
            event_loop,
            presentation_results,
            frame_timer,
        } = self;
        let SealedWgpuHostedCycle {
            outputs,
            transaction_guard,
            mut active_viewports,
            frozen_viewports,
            render_state,
            native_effect_sink,
            native_viewport_create_sink,
            viewport_create_schedules,
        } = sealed_cycle;
        let outputs = arm_hosted_presentations(outputs, presentation_results);
        let destroyed_viewports = {
            let mut shared = shared.borrow_mut();
            let native_windows = shared
                .viewports
                .iter()
                .filter_map(|(viewport_id, viewport)| {
                    viewport
                        .window
                        .as_ref()
                        .map(|window| (*viewport_id, Arc::clone(window)))
                })
                .collect::<Vec<_>>();
            let (destroyed_viewports, dispatch_error) = shared
                .platform_ingress
                .dispatch_effects(&native_effect_sink, native_windows)
                .into_parts();
            if let Some(error) = dispatch_error {
                log::error!("failed to dispatch sealed native effects: {error}");
            }
            destroyed_viewports
        };
        let destroyed_any = !destroyed_viewports.is_empty();
        for viewport in destroyed_viewports {
            active_viewports.remove(&viewport);
        }
        if destroyed_any {
            integration.egui_ctx.request_repaint_of(ViewportId::ROOT);
        }
        let mut texture_synchronizer = HostedTextureSynchronizer::default();
        let mut total_vsync_secs = 0.0;

        for output in outputs {
            let (viewport_id, full_output, is_native_staging_presentation, pending_presentation) =
                output.into_parts();
            let is_visible = frozen_viewports[&viewport_id].is_visible;
            let should_present =
                should_present_hosted_output(is_visible, is_native_staging_presentation);
            let FullOutput {
                platform_output,
                mut textures_delta,
                shapes,
                pixels_per_point,
                pointer_receiver_journal: _,
                pointer_hit_graph_candidate: _,
                viewport_output: _,
            } = full_output;

            if !is_active_output_owner(&active_viewports, viewport_id) {
                synchronize_texture_output(
                    &mut texture_synchronizer,
                    &mut textures_delta,
                    &render_state,
                );
                pending_presentation.complete(egui::PaintOutcome::Skipped(
                    egui::PaintSkipReason::SupersededByNewerPass,
                ));
                continue;
            }

            let mut shared = shared.borrow_mut();
            let SharedState {
                viewports, painter, ..
            } = &mut *shared;
            let Some(viewport) = viewports.get_mut(&viewport_id) else {
                synchronize_texture_output(
                    &mut texture_synchronizer,
                    &mut textures_delta,
                    &render_state,
                );
                pending_presentation.complete(egui::PaintOutcome::Skipped(
                    egui::PaintSkipReason::ViewportUnavailable,
                ));
                continue;
            };
            viewport.info.events.clear();
            let Viewport {
                window: Some(window),
                egui_winit: Some(egui_winit),
                actions_requested,
                ..
            } = viewport
            else {
                synchronize_texture_output(
                    &mut texture_synchronizer,
                    &mut textures_delta,
                    &render_state,
                );
                pending_presentation.complete(egui::PaintOutcome::Skipped(
                    egui::PaintSkipReason::ViewportUnavailable,
                ));
                continue;
            };

            egui_winit.handle_platform_output_with_event_loop(window, event_loop, platform_output);
            synchronize_texture_output(
                &mut texture_synchronizer,
                &mut textures_delta,
                &render_state,
            );
            if should_present {
                let clipped_primitives = integration.egui_ctx.tessellate(shapes, pixels_per_point);
                let screenshot_commands = if is_native_staging_presentation {
                    actions_requested.clear();
                    Vec::new()
                } else {
                    take_screenshot_actions(actions_requested)
                };
                let paint_result = painter.paint_and_update_textures(
                    viewport_id,
                    pixels_per_point,
                    app.clear_color(&integration.egui_ctx.global_style().visuals),
                    &clipped_primitives,
                    &textures_delta,
                    screenshot_commands,
                    window,
                );
                let paint_outcome = paint_result.outcome;
                if matches!(paint_outcome, egui::PaintOutcome::SubmittedToSwapchain)
                    && !is_native_staging_presentation
                {
                    integration.post_rendering(window);
                }
                pending_presentation.complete(paint_outcome);
                total_vsync_secs += paint_result.vsync_seconds;
                if !is_native_staging_presentation {
                    dispatch_requested_actions(actions_requested, egui_winit);
                }
            } else {
                pending_presentation.complete(egui::PaintOutcome::Skipped(
                    egui::PaintSkipReason::NotVisible,
                ));
            }
        }

        finish_texture_transaction(&render_state, texture_synchronizer);

        let mut shared = shared.borrow_mut();
        let SharedState {
            viewports,
            painter,
            viewport_from_window,
            ..
        } = &mut *shared;
        for schedule in viewport_create_schedules {
            schedule.commit_schedule(&mut active_viewports, &native_viewport_create_sink);
        }
        handle_viewport_output(
            &integration.egui_ctx,
            &active_viewports,
            viewports,
            painter,
            viewport_from_window,
        );

        let root_window = viewports
            .get(&ViewportId::ROOT)
            .and_then(|viewport| viewport.window.as_deref());
        integration.report_frame_time((frame_timer.total_time_sec() - total_vsync_secs).max(0.0));
        integration.maybe_autosave(app, root_window);

        if let Some(violation) = transaction_guard.finish().first().copied() {
            return Err(HostedViewportCycleError::ImmediateViewportAttempt {
                viewport_id: violation.viewport_id(),
            }
            .into());
        }

        if viewports
            .values()
            .filter_map(|viewport| viewport.window.as_deref())
            .any(is_invisible_or_minimized)
        {
            profiling::scope!("minimized_sleep");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }

        if integration.should_close() {
            Ok(EventResult::CloseRequested)
        } else {
            Ok(EventResult::Wait)
        }
    }
}

fn take_screenshot_actions(actions: &mut Vec<ActionRequested>) -> Vec<egui::UserData> {
    let mut screenshots = Vec::new();
    actions.retain(|action| {
        if let ActionRequested::Screenshot(user_data) = action {
            screenshots.push(user_data.clone());
            false
        } else {
            true
        }
    });
    screenshots
}

fn dispatch_requested_actions(
    actions: &mut Vec<ActionRequested>,
    egui_winit: &mut egui_winit::State,
) {
    for action in actions.drain(..) {
        match action {
            ActionRequested::Screenshot { .. } => {}
            ActionRequested::Cut => {
                egui_winit.egui_input_mut().push_event(egui::Event::Cut);
            }
            ActionRequested::Copy => {
                egui_winit.egui_input_mut().push_event(egui::Event::Copy);
            }
            ActionRequested::Paste => {
                if let Some(contents) = egui_winit.clipboard_text() {
                    let contents = contents.replace("\r\n", "\n");
                    if !contents.is_empty() {
                        egui_winit
                            .egui_input_mut()
                            .push_event(egui::Event::Paste(contents));
                    }
                }
            }
        }
    }
}

fn synchronize_texture_output(
    synchronizer: &mut HostedTextureSynchronizer,
    textures_delta: &mut egui::TexturesDelta,
    render_state: &egui_wgpu::RenderState,
) {
    let mut renderer = render_state.renderer.write();
    synchronizer.synchronize_output(textures_delta, |texture_id, image_delta| {
        renderer.update_texture(
            &render_state.device,
            &render_state.queue,
            texture_id,
            image_delta,
        );
    });
}

fn finish_texture_transaction(
    render_state: &egui_wgpu::RenderState,
    synchronizer: HostedTextureSynchronizer,
) {
    // `update_texture` may stage queue writes after the last viewport paint (or
    // in a cycle with no paint at all). Submit a terminal boundary before any
    // deferred destroy so WGPU can retire its staging resources deterministically.
    let _ = render_state.queue.submit([]);
    let mut renderer = render_state.renderer.write();
    for texture_id in synchronizer.into_deferred_frees() {
        renderer.free_texture(&texture_id);
    }
}

fn settle_aborted_outputs(
    outputs: &mut [HostedViewportOutput<FullOutput>],
    presentation_results: &PresentationResults,
    render_state: Option<&egui_wgpu::RenderState>,
) {
    let mut texture_synchronizer = HostedTextureSynchronizer::default();
    for output in outputs {
        let viewport_id = output.viewport_id();
        let native_binding = output.native_binding();
        let full_output = output.output_mut();
        if let Some(render_state) = render_state {
            synchronize_texture_output(
                &mut texture_synchronizer,
                &mut full_output.textures_delta,
                render_state,
            );
        }
        PendingPresentation::new(
            presentation_results.clone(),
            viewport_id,
            native_binding,
            full_output.platform_output.presentation_token.take(),
        )
        .with_pointer_hit_graph_candidate(full_output.pointer_hit_graph_candidate.take())
        .complete(egui::PaintOutcome::Failed(
            egui::PaintFailure::CoordinatorAborted,
        ));
    }
    if let Some(render_state) = render_state {
        finish_texture_transaction(render_state, texture_synchronizer);
    }
}

#[cfg(test)]
mod tests {
    use egui::{ViewportBuilder, ViewportClass};

    use super::*;

    fn viewport_output(parent: ViewportId, class: ViewportClass) -> ViewportOutput {
        ViewportOutput {
            parent,
            class,
            builder: ViewportBuilder::default(),
            viewport_ui_cb: None,
            commands: Vec::new(),
            repaint_delay: std::time::Duration::MAX,
        }
    }

    fn image_delta() -> egui::epaint::ImageDelta {
        egui::epaint::ImageDelta::full(
            egui::ColorImage::filled([1, 1], egui::Color32::WHITE),
            egui::TextureOptions::LINEAR,
        )
    }

    #[test]
    fn only_authorized_hidden_output_bypasses_not_visible_skip() {
        assert!(!should_present_hosted_output(false, false));
        assert!(should_present_hosted_output(false, true));
        assert!(should_present_hosted_output(true, false));
    }

    #[test]
    fn parent_removal_makes_staged_child_inactive() {
        let child = ViewportId::from_hash_of("wgpu-inactive-staged-child");
        let root_output = OrderedViewportIdMap::from([(
            ViewportId::ROOT,
            viewport_output(ViewportId::ROOT, ViewportClass::Root),
        )]);
        let child_output = OrderedViewportIdMap::from([
            (
                ViewportId::ROOT,
                viewport_output(ViewportId::ROOT, ViewportClass::Root),
            ),
            (
                child,
                viewport_output(ViewportId::ROOT, ViewportClass::Deferred),
            ),
        ]);
        let consolidated = consolidate_hosted_viewport_outputs(&[
            (ViewportId::ROOT, root_output),
            (child, child_output),
        ])
        .expect("valid outputs should consolidate");

        assert!(is_active_output_owner(&consolidated, ViewportId::ROOT));
        assert!(!is_active_output_owner(&consolidated, child));
    }

    #[test]
    fn texture_sets_follow_output_order_and_frees_wait_for_the_cycle() {
        let texture_a = egui::TextureId::User(41);
        let texture_b = egui::TextureId::User(42);
        let mut root_delta = egui::TexturesDelta {
            set: vec![(texture_a, image_delta())],
            free: vec![texture_b],
        };
        let mut child_delta = egui::TexturesDelta {
            set: vec![(texture_b, image_delta())],
            free: vec![texture_a],
        };
        let mut observed_sets = Vec::new();
        let mut synchronizer = HostedTextureSynchronizer::default();

        synchronizer.synchronize_output(&mut root_delta, |texture_id, _| {
            observed_sets.push(texture_id);
        });
        synchronizer.synchronize_output(&mut child_delta, |texture_id, _| {
            observed_sets.push(texture_id);
        });

        assert_eq!(observed_sets, [texture_a, texture_b]);
        assert!(root_delta.is_empty());
        assert!(child_delta.is_empty());
        assert_eq!(synchronizer.into_deferred_frees(), [texture_a]);
    }
}

//! Transactional hosted-viewport coordination for the Glow backend.
//!
//! The window and OpenGL objects remain owned by `glow_integration`. This
//! module owns the complete-roster frame transaction from frozen input through
//! staged output consolidation and terminal renderer settlement.

use std::{cell::RefCell, sync::Arc};

use egui::{
    DeferredViewportUiCallback, FullOutput, OrderedViewportIdMap, ViewportId, ViewportInfo,
    ViewportOutput,
};
use egui_winit::ActionRequested;
use glutin::prelude::GlSurface as _;
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

#[cfg(feature = "__screenshot")]
use super::glow_integration::save_screenshot_and_exit;
use super::glow_integration::{GlowWinitRunning, GlutinWindowContext, Viewport, change_gl_context};

struct GlowHostedViewport {
    viewport_ui_cb: Option<Arc<DeferredViewportUiCallback>>,
    is_visible: bool,
}

struct FrozenGlowHostedCycle {
    raw_inputs: Vec<egui::RawInput>,
    callback_order: Vec<ViewportId>,
    viewports: OrderedViewportIdMap<GlowHostedViewport>,
    native_ingress: crate::NativeHostIngress,
    native_staging_presentations: Vec<crate::NativeViewportBinding>,
    native_effect_sink: crate::NativeEffectSink,
    native_viewport_create_sink: crate::NativeViewportCreateSink,
    native_ingress_settlement: super::platform_ingress_owner::NativeHostIngressSettlement,
}

struct SealedGlowHostedCycle {
    outputs: Vec<HostedViewportOutput<FullOutput>>,
    transaction_guard: HostedViewportTransactionGuard,
    active_viewports: OrderedViewportIdMap<ViewportOutput>,
    frozen_viewports: OrderedViewportIdMap<GlowHostedViewport>,
    native_effect_sink: crate::NativeEffectSink,
    native_viewport_create_sink: crate::NativeViewportCreateSink,
    viewport_create_schedules: Vec<PreparedNativeViewportSchedule>,
}

struct GlowHostedCommitter<'a> {
    integration: &'a mut EpiIntegration,
    app: &'a mut dyn App,
    glutin: &'a RefCell<GlutinWindowContext>,
    painter: &'a RefCell<egui_glow::Painter>,
    event_loop: &'a ActiveEventLoop,
    presentation_results: &'a PresentationResults,
    frame_timer: &'a mut crate::stopwatch::Stopwatch,
}

struct GlowHostedCycleUiDriver<'a> {
    integration: &'a mut EpiIntegration,
    app: &'a mut dyn App,
    hosted_viewports: &'a OrderedViewportIdMap<GlowHostedViewport>,
}

impl HostedViewportCycleDriver for GlowHostedCycleUiDriver<'_> {
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
                "hosted Glow viewport {viewport_id:?} has no frozen metadata"
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

/// Runs one complete hosted Glow transaction.
///
/// No renderer or viewport borrow survives a user callback. All staged outputs
/// are either committed against the sealed active roster or terminally aborted
/// before this function returns.
pub(super) fn run(
    running: &mut GlowWinitRunning<'_>,
    event_loop: &ActiveEventLoop,
) -> Result<EventResult> {
    profiling::function_scope!();
    let presentation_results = running.glutin.borrow().presentation_results.clone();
    presentation_results.drain();

    profiling::finish_frame!();

    let mut frame_timer = crate::stopwatch::Stopwatch::new();
    frame_timer.start();

    let frozen_cycle = {
        let mut glutin = running.glutin.borrow_mut();
        freeze_complete_roster(&mut glutin)?
    };
    let Some(frozen_cycle) = frozen_cycle else {
        return Ok(EventResult::Wait);
    };

    let sealed_cycle = stage_and_seal(
        &mut running.integration,
        running.app.as_mut(),
        &running.painter,
        frozen_cycle,
        &presentation_results,
    )?;

    let result = GlowHostedCommitter {
        integration: &mut running.integration,
        app: running.app.as_mut(),
        glutin: &running.glutin,
        painter: &running.painter,
        event_loop,
        presentation_results: &presentation_results,
        frame_timer: &mut frame_timer,
    }
    .commit(sealed_cycle);
    presentation_results.drain();
    Ok(result?)
}

fn freeze_complete_roster(
    glutin: &mut GlutinWindowContext,
) -> std::result::Result<Option<FrozenGlowHostedCycle>, HostedViewportCycleError> {
    let egui_ctx = glutin.egui_ctx.clone();
    let mut callback_order = glutin
        .viewports
        .iter()
        .filter_map(|(viewport_id, viewport)| {
            let is_hosted = *viewport_id == ViewportId::ROOT || viewport.viewport_ui_cb.is_some();
            (is_hosted
                && viewport.window.is_some()
                && viewport.egui_winit.is_some()
                && viewport.gl_surface.is_some())
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
        let Some(viewport) = glutin.viewports.get_mut(viewport_id) else {
            log::error!("Glow hosted viewport {viewport_id:?} vanished while freezing its roster");
            return Ok(None);
        };
        let Some(window) = viewport.window.as_ref() else {
            log::error!(
                "Glow hosted viewport {viewport_id:?} lost its window while freezing its roster"
            );
            return Ok(None);
        };
        egui_winit::update_viewport_info(&mut viewport.info, &egui_ctx, window, false);
    }

    let inventory: egui::ViewportIdMap<ViewportInfo> = glutin
        .viewports
        .iter()
        .map(|(viewport_id, viewport)| (*viewport_id, viewport.info.clone()))
        .collect();
    let mut hosted_viewports = OrderedViewportIdMap::new();
    for viewport_id in &callback_order {
        let Some(viewport) = glutin.viewports.get(viewport_id) else {
            log::error!("Glow hosted viewport {viewport_id:?} vanished before metadata collection");
            return Ok(None);
        };
        hosted_viewports.insert(
            *viewport_id,
            GlowHostedViewport {
                viewport_ui_cb: viewport.viewport_ui_cb.clone(),
                is_visible: viewport.info.visible().unwrap_or(true),
            },
        );
    }

    let mut raw_inputs = Vec::with_capacity(callback_order.len());
    for viewport_id in &callback_order {
        let Some(viewport) = glutin.viewports.get_mut(viewport_id) else {
            log::error!("Glow hosted viewport {viewport_id:?} vanished before input collection");
            return Ok(None);
        };
        let Some(window) = viewport.window.as_ref() else {
            log::error!(
                "Glow hosted viewport {viewport_id:?} lost its window before input collection"
            );
            return Ok(None);
        };
        let Some(egui_winit) = viewport.egui_winit.as_mut() else {
            log::error!(
                "Glow hosted viewport {viewport_id:?} lost its input state before collection"
            );
            return Ok(None);
        };
        let mut raw_input = egui_winit.take_egui_input(window);
        raw_input.viewports = inventory.clone();
        raw_inputs.push(raw_input);
    }

    let native_windows = glutin
        .viewports
        .iter()
        .filter_map(|(viewport_id, viewport)| {
            viewport
                .window
                .as_ref()
                .map(|window| (*viewport_id, Arc::clone(window)))
        })
        .collect::<Vec<_>>();
    let frozen_native_ingress = glutin
        .platform_ingress
        .freeze(native_windows, &egui_ctx)
        .map_err(std::io::Error::other)?;
    let (
        native_ingress,
        native_staging_presentations,
        native_effect_sink,
        native_viewport_create_sink,
        native_ingress_settlement,
    ) = frozen_native_ingress.into_parts();

    Ok(Some(FrozenGlowHostedCycle {
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
    painter: &RefCell<egui_glow::Painter>,
    frozen_cycle: FrozenGlowHostedCycle,
    presentation_results: &PresentationResults,
) -> Result<SealedGlowHostedCycle> {
    let FrozenGlowHostedCycle {
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
    let drive_result = {
        let mut driver = GlowHostedCycleUiDriver {
            integration,
            app,
            hosted_viewports: &viewports,
        };
        cycle.drive_with_guard(transaction_guard, callback_order, &mut driver)
    };
    let completed = match drive_result {
        Ok(completed) => completed,
        Err(abort) => {
            return Err(finish_abort(
                abort,
                integration,
                painter,
                presentation_results,
            ));
        }
    };
    let (mut outputs, mut transaction_guard) = completed.into_parts();

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
        Ok(viewport_output) => viewport_output,
        Err(error) => {
            retained_effect_sink.close_and_cancel();
            retained_viewport_create_sink.close_and_cancel();
            return Err(abort_completed_cycle(
                error.into(),
                outputs,
                integration,
                app,
                painter,
                presentation_results,
            ));
        }
    };
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
                painter,
                presentation_results,
            ));
        }
    };
    let native_ingress_commit = match native_ingress_settlement.prepare_commit() {
        Ok(commit) => commit,
        Err(error) => {
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
                painter,
                presentation_results,
            ));
        }
    };
    if let Err(violation) = transaction_guard.seal_before_application_commit() {
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
            painter,
            presentation_results,
        ));
    }
    let application_commit =
        match integration.commit_application_hosted_viewport_cycle(app, &mut outputs) {
            Ok(commit) => commit,
            Err(source) => {
                retained_effect_sink.close_and_cancel();
                for schedule in viewport_create_schedules {
                    schedule.cancel(&retained_viewport_create_sink);
                }
                return Err(abort_completed_cycle(
                    HostedViewportCycleError::CommitHook { source },
                    outputs,
                    integration,
                    app,
                    painter,
                    presentation_results,
                ));
            }
        };
    native_ingress_commit.commit();
    integration.commit_hosted_viewport_cycle(hosted_commit);
    if application_commit.requests_root_repaint() {
        integration.egui_ctx.request_repaint_of(ViewportId::ROOT);
    }

    Ok(SealedGlowHostedCycle {
        outputs,
        transaction_guard,
        active_viewports,
        frozen_viewports: viewports,
        native_effect_sink: retained_effect_sink,
        native_viewport_create_sink: retained_viewport_create_sink,
        viewport_create_schedules,
    })
}

fn abort_completed_cycle(
    error: HostedViewportCycleError,
    outputs: Vec<HostedViewportOutput<FullOutput>>,
    integration: &mut EpiIntegration,
    app: &mut dyn App,
    painter: &RefCell<egui_glow::Painter>,
    presentation_results: &PresentationResults,
) -> crate::Error {
    integration.abort_application_hosted_viewport_cycle(app);
    let mut abort = HostedViewportCycleAbort::from_error(error);
    abort.append_staged_outputs(outputs);
    finish_abort(abort, integration, painter, presentation_results)
}

fn finish_abort(
    mut abort: HostedViewportCycleAbort<FullOutput>,
    integration: &mut EpiIntegration,
    painter: &RefCell<egui_glow::Painter>,
    presentation_results: &PresentationResults,
) -> crate::Error {
    abort.append_staged_outputs(integration.abort_hosted_viewport_cycle());
    settle_aborted_outputs(
        abort.staged_outputs_mut(),
        &mut painter.borrow_mut(),
        presentation_results,
    );
    presentation_results.drain();
    abort.into()
}

impl GlowHostedCommitter<'_> {
    fn commit(self, sealed_cycle: SealedGlowHostedCycle) -> Result<EventResult> {
        let Self {
            integration,
            app,
            glutin,
            painter,
            event_loop,
            presentation_results,
            frame_timer,
        } = self;
        let SealedGlowHostedCycle {
            outputs,
            transaction_guard,
            mut active_viewports,
            frozen_viewports,
            native_effect_sink,
            native_viewport_create_sink,
            viewport_create_schedules,
        } = sealed_cycle;
        let outputs = arm_hosted_presentations(outputs, presentation_results);
        let clear_color = app.clear_color(&integration.egui_ctx.global_style().visuals);
        let mut glutin = glutin.borrow_mut();
        let native_windows = glutin
            .viewports
            .iter()
            .filter_map(|(viewport_id, viewport)| {
                viewport
                    .window
                    .as_ref()
                    .map(|window| (*viewport_id, Arc::clone(window)))
            })
            .collect::<Vec<_>>();
        let (destroyed_viewports, dispatch_error) = glutin
            .platform_ingress
            .dispatch_effects(&native_effect_sink, native_windows)
            .into_parts();
        if let Some(error) = dispatch_error {
            log::error!("failed to dispatch sealed native effects: {error}");
        }
        let destroyed_any = !destroyed_viewports.is_empty();
        for viewport in destroyed_viewports {
            active_viewports.remove(&viewport);
        }
        if destroyed_any {
            integration.egui_ctx.request_repaint_of(ViewportId::ROOT);
        }
        let mut painter = painter.borrow_mut();
        let mut texture_synchronizer = HostedTextureSynchronizer::default();

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
                ..
            } = full_output;

            if !is_active_output_owner(&active_viewports, viewport_id) {
                synchronize_texture_output(
                    &mut texture_synchronizer,
                    &mut textures_delta,
                    &mut painter,
                );
                pending_presentation.complete(egui::PaintOutcome::Skipped(
                    egui::PaintSkipReason::SupersededByNewerPass,
                ));
                continue;
            }

            let GlutinWindowContext {
                viewports,
                current_gl_context,
                not_current_gl_context,
                ..
            } = &mut *glutin;
            let Some(viewport) = viewports.get_mut(&viewport_id) else {
                synchronize_texture_output(
                    &mut texture_synchronizer,
                    &mut textures_delta,
                    &mut painter,
                );
                pending_presentation.complete(egui::PaintOutcome::Skipped(
                    egui::PaintSkipReason::ViewportUnavailable,
                ));
                continue;
            };
            viewport.info.events.clear();
            let Viewport {
                window,
                gl_surface,
                egui_winit,
                actions_requested,
                ..
            } = viewport;
            let (Some(window), Some(gl_surface), Some(egui_winit)) =
                (window.clone(), gl_surface.as_ref(), egui_winit.as_mut())
            else {
                synchronize_texture_output(
                    &mut texture_synchronizer,
                    &mut textures_delta,
                    &mut painter,
                );
                pending_presentation.complete(egui::PaintOutcome::Skipped(
                    egui::PaintSkipReason::ViewportUnavailable,
                ));
                continue;
            };

            egui_winit.handle_platform_output_with_event_loop(&window, event_loop, platform_output);
            synchronize_texture_output(
                &mut texture_synchronizer,
                &mut textures_delta,
                &mut painter,
            );

            if should_present {
                frame_timer.pause();
                change_gl_context(current_gl_context, not_current_gl_context, gl_surface);
                frame_timer.resume();
            }

            if should_present {
                let clipped_primitives = integration.egui_ctx.tessellate(shapes, pixels_per_point);
                let screen_size_in_pixels: [u32; 2] = window.inner_size().into();
                painter.clear(screen_size_in_pixels, clear_color);
                painter.paint_primitives(
                    screen_size_in_pixels,
                    pixels_per_point,
                    &clipped_primitives,
                );

                if is_native_staging_presentation {
                    actions_requested.clear();
                } else {
                    handle_requested_actions(
                        actions_requested,
                        &painter,
                        egui_winit,
                        viewport_id,
                        screen_size_in_pixels,
                    );
                }

                frame_timer.pause();
                profiling::scope!("swap_buffers");
                let swap_result = current_gl_context.as_ref().map_or_else(
                    || Err("failed to get current context to swap buffers".to_owned()),
                    |context| {
                        gl_surface
                            .swap_buffers(context)
                            .map_err(|error| error.to_string())
                    },
                );
                frame_timer.resume();
                let swapped = settle_glow_swap(pending_presentation, swap_result);
                if swapped && !is_native_staging_presentation {
                    integration.post_rendering(&window);
                }

                #[cfg(feature = "__screenshot")]
                if swapped
                    && !is_native_staging_presentation
                    && integration.egui_ctx.cumulative_pass_nr() == 2
                    && let Ok(path) = std::env::var("EFRAME_SCREENSHOT_TO")
                {
                    save_screenshot_and_exit(&path, &painter, screen_size_in_pixels);
                }
            } else {
                pending_presentation.complete(egui::PaintOutcome::Skipped(
                    egui::PaintSkipReason::NotVisible,
                ));
            }
        }

        for texture_id in texture_synchronizer.into_deferred_frees() {
            painter.free_texture(texture_id);
        }

        for schedule in viewport_create_schedules {
            schedule.commit_schedule(&mut active_viewports, &native_viewport_create_sink);
        }
        glutin.handle_viewport_output(event_loop, &integration.egui_ctx, &active_viewports);

        let root_window = glutin.window_opt(ViewportId::ROOT);
        let should_sleep = glutin.viewports.values().any(|viewport| {
            viewport
                .window
                .as_deref()
                .is_some_and(is_invisible_or_minimized)
        });

        integration.report_frame_time(frame_timer.total_time_sec());
        integration.maybe_autosave(app, root_window.as_deref());

        if let Some(violation) = transaction_guard.finish().first().copied() {
            return Err(HostedViewportCycleError::ImmediateViewportAttempt {
                viewport_id: violation.viewport_id(),
            }
            .into());
        }

        if should_sleep {
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

fn handle_requested_actions(
    actions_requested: &mut Vec<ActionRequested>,
    painter: &egui_glow::Painter,
    egui_winit: &mut egui_winit::State,
    viewport_id: ViewportId,
    screen_size_in_pixels: [u32; 2],
) {
    for action in actions_requested.drain(..) {
        match action {
            ActionRequested::Screenshot(user_data) => {
                let screenshot = painter.read_screen_rgba(screen_size_in_pixels);
                egui_winit
                    .egui_input_mut()
                    .push_event(egui::Event::Screenshot {
                        viewport_id,
                        user_data,
                        image: screenshot.into(),
                    });
            }
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
    painter: &mut egui_glow::Painter,
) {
    synchronizer.synchronize_output(textures_delta, |texture_id, image_delta| {
        painter.set_texture(texture_id, image_delta);
    });
}

fn settle_aborted_outputs(
    outputs: &mut [HostedViewportOutput<FullOutput>],
    painter: &mut egui_glow::Painter,
    presentation_results: &PresentationResults,
) {
    let mut texture_synchronizer = HostedTextureSynchronizer::default();
    for output in outputs {
        let viewport_id = output.viewport_id();
        let native_binding = output.native_binding();
        let full_output = output.output_mut();
        synchronize_texture_output(
            &mut texture_synchronizer,
            &mut full_output.textures_delta,
            painter,
        );
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
    for texture_id in texture_synchronizer.into_deferred_frees() {
        painter.free_texture(texture_id);
    }
}

fn settle_glow_swap(
    pending_presentation: PendingPresentation,
    swap_result: std::result::Result<(), String>,
) -> bool {
    let paint_outcome = match swap_result {
        Ok(()) => egui::PaintOutcome::Swapped,
        Err(error) => egui::PaintOutcome::Failed(egui::PaintFailure::SwapBuffers(error)),
    };
    let swapped = matches!(paint_outcome, egui::PaintOutcome::Swapped);
    pending_presentation.complete(paint_outcome);
    swapped
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

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

    #[test]
    fn only_authorized_hidden_output_bypasses_not_visible_skip() {
        assert!(!should_present_hosted_output(false, false));
        assert!(should_present_hosted_output(false, true));
        assert!(should_present_hosted_output(true, false));
    }

    #[test]
    fn parent_removal_makes_staged_child_inactive() {
        let child = ViewportId::from_hash_of("glow-inactive-staged-child");
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
    fn unavailable_output_cannot_drop_its_texture_synchronization() {
        let texture_id = egui::TextureId::Managed(41);
        let image_delta = egui::epaint::ImageDelta::full(
            egui::ColorImage::filled([1, 1], egui::Color32::WHITE),
            egui::TextureOptions::LINEAR,
        );
        let mut delta = egui::TexturesDelta {
            set: vec![(texture_id, image_delta)],
            free: vec![texture_id],
        };

        let mut synchronized_sets = Vec::new();
        let mut synchronizer = HostedTextureSynchronizer::default();
        synchronizer.synchronize_output(&mut delta, |texture_id, _| {
            synchronized_sets.push(texture_id);
        });
        let viewport_available = false;

        assert!(!viewport_available);
        assert_eq!(synchronized_sets, [texture_id]);
        assert_eq!(synchronizer.into_deferred_frees(), [texture_id]);
        assert!(delta.is_empty());
    }

    #[test]
    fn failed_swap_does_not_prevent_later_swap_settlement() {
        let first = ViewportId::from_hash_of("failed-glow-swap");
        let second = ViewportId::from_hash_of("later-glow-swap");
        let settled = Arc::new(egui::mutex::Mutex::new(Vec::new()));
        let hook_settled = Arc::clone(&settled);
        let coordinator = Arc::new(egui::mutex::Mutex::new(
            crate::native::platform_provider::NativePlatformCoordinator::default(),
        ));
        let first_binding = coordinator.lock().register_viewport(first).unwrap();
        let second_binding = coordinator.lock().register_viewport(second).unwrap();
        let results = PresentationResults::new(
            Some(Arc::new(move |result| {
                hook_settled.lock().push(result);
            })),
            coordinator,
        );

        assert!(!settle_glow_swap(
            PendingPresentation::new(
                results.clone(),
                first,
                Some(first_binding),
                Some(egui::UserData::new("first")),
            ),
            Err("first swap failed".to_owned()),
        ));
        assert!(settle_glow_swap(
            PendingPresentation::new(
                results.clone(),
                second,
                Some(second_binding),
                Some(egui::UserData::new("second")),
            ),
            Ok(()),
        ));
        results.drain();

        let outcomes = settled.lock();
        assert_eq!(outcomes.len(), 2);
        assert_eq!(outcomes[0].viewport_id(), first);
        assert!(matches!(
            outcomes[0].outcome(),
            egui::PaintOutcome::Failed(egui::PaintFailure::SwapBuffers(error))
                if error == "first swap failed"
        ));
        assert_eq!(outcomes[1].viewport_id(), second);
        assert_eq!(outcomes[1].outcome(), &egui::PaintOutcome::Swapped);
    }
}

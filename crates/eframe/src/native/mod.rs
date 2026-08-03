mod app_icon;
mod epi_integration;
mod event_loop_context;
mod native_effect_sink;
pub use native_effect_sink::{NativeEffectSink, NativeEffectSubmitError};
mod native_viewport_create_sink;
pub use native_viewport_create_sink::{NativeViewportCreateSink, NativeViewportCreateSubmitError};
pub mod hosted_cycle;
mod native_pointer_probe;
mod native_window_probe;
mod native_work_area_authority;
mod native_work_area_probe;
mod platform_ingress_owner;
pub(crate) mod platform_provider;
pub mod run;
#[cfg(feature = "native-test-support")]
mod test_support;

#[cfg(feature = "native-test-support")]
pub use test_support::{
    NativeTestDriver, NativeTestPointerAction, NativeTestPointerEvent, NativeTestPointerLocation,
    NativeTestScrollDelta, NativeTestWindowScroll,
};

#[cfg(target_os = "macos")]
pub(crate) mod macos;

/// File storage which can be used by native backends.
#[cfg(feature = "persistence")]
pub mod file_storage;

pub(crate) mod winit_integration;

#[cfg(any(feature = "glow", feature = "wgpu_no_default_features"))]
#[derive(Clone)]
pub(crate) struct PresentationResults {
    hook: Option<crate::PresentationResultHook>,
    coordinator: platform_provider::SharedNativePlatformCoordinator,
    queued: std::sync::Arc<egui::mutex::Mutex<Vec<egui::PresentationResult>>>,
}

#[cfg(any(feature = "glow", feature = "wgpu_no_default_features"))]
impl PresentationResults {
    pub(crate) fn new(
        hook: Option<crate::PresentationResultHook>,
        coordinator: platform_provider::SharedNativePlatformCoordinator,
    ) -> Self {
        Self {
            hook,
            coordinator,
            queued: Default::default(),
        }
    }

    pub(crate) fn begin(
        &self,
        binding: crate::NativeViewportBinding,
    ) -> Result<platform_provider::NativePresentationTicket, crate::NativePlatformError> {
        self.coordinator.lock().begin_presentation(binding)
    }

    fn enqueue(
        &self,
        ticket: platform_provider::NativePresentationTicket,
        result: egui::PresentationResult,
    ) {
        if let Err(error) = self
            .coordinator
            .lock()
            .record_presentation_result(ticket, result.clone())
        {
            log::error!("native presentation result rejected before hook dispatch: {error}");
            return;
        }
        if self.hook.is_some() {
            self.queued.lock().push(result);
        }
    }

    /// Invoke hooks only from an explicit coordinator boundary where renderer and viewport
    /// borrows have already been released.
    #[expect(
        clippy::disallowed_methods,
        reason = "native result hooks are isolated so one panicking consumer cannot block later results"
    )]
    pub(crate) fn drain(&self) {
        let queued = std::mem::take(&mut *self.queued.lock());
        let Some(hook) = &self.hook else {
            return;
        };

        for result in queued {
            if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| hook(result))).is_err() {
                log::error!("presentation result hook panicked; ignoring the hook failure");
            }
        }
    }
}

#[cfg(any(feature = "glow", feature = "wgpu_no_default_features"))]
pub(crate) struct PendingPresentation {
    results: PresentationResults,
    viewport_id: egui::ViewportId,
    token: Option<egui::UserData>,
    ticket: Option<platform_provider::NativePresentationTicket>,
    pointer_hit_graph_candidate: Option<egui::PointerHitGraphCandidate>,
}

#[cfg(any(feature = "glow", feature = "wgpu_no_default_features"))]
impl PendingPresentation {
    pub(crate) fn new(
        results: PresentationResults,
        viewport_id: egui::ViewportId,
        binding: Option<crate::NativeViewportBinding>,
        token: Option<egui::UserData>,
    ) -> Self {
        let ticket = token.as_ref().and_then(|_| {
            let Some(binding) = binding else {
                log::error!(
                    "presentation token for viewport {viewport_id:?} has no exact native binding"
                );
                return None;
            };
            match results.begin(binding) {
                Ok(ticket) => Some(ticket),
                Err(error) => {
                    log::error!(
                        "presentation token for viewport {viewport_id:?} could not begin: {error}"
                    );
                    None
                }
            }
        });
        Self {
            results,
            viewport_id,
            token,
            ticket,
            pointer_hit_graph_candidate: None,
        }
    }

    pub(crate) fn with_pointer_hit_graph_candidate(
        mut self,
        candidate: Option<egui::PointerHitGraphCandidate>,
    ) -> Self {
        self.pointer_hit_graph_candidate = candidate;
        self
    }

    pub(crate) fn complete(mut self, outcome: egui::PaintOutcome) {
        self.enqueue(outcome);
    }

    fn enqueue(&mut self, outcome: egui::PaintOutcome) {
        let Some(token) = self.token.take() else {
            if let Some(candidate) = self.pointer_hit_graph_candidate.take() {
                candidate.settle_for(self.viewport_id, &outcome);
            }
            return;
        };
        let Some(ticket) = self.ticket.take() else {
            log::error!(
                "presentation token for viewport {:?} had no exact native ticket",
                self.viewport_id
            );
            if let Some(candidate) = self.pointer_hit_graph_candidate.take() {
                candidate.settle_for(
                    self.viewport_id,
                    &egui::PaintOutcome::Failed(egui::PaintFailure::CoordinatorAborted),
                );
            }
            return;
        };
        self.results.enqueue(
            ticket,
            egui::PresentationResult::new(
                self.viewport_id,
                token,
                outcome,
                self.pointer_hit_graph_candidate.take(),
            ),
        );
    }
}

#[cfg(any(feature = "glow", feature = "wgpu_no_default_features"))]
impl Drop for PendingPresentation {
    fn drop(&mut self) {
        self.enqueue(egui::PaintOutcome::Failed(
            egui::PaintFailure::CoordinatorAborted,
        ));
    }
}

#[cfg(feature = "glow")]
mod glow_hosted_cycle;

#[cfg(feature = "glow")]
mod glow_integration;

#[cfg(feature = "wgpu_no_default_features")]
mod wgpu_hosted_cycle;

#[cfg(feature = "wgpu_no_default_features")]
mod wgpu_integration;

#[cfg(all(test, any(feature = "glow", feature = "wgpu_no_default_features")))]
mod tests {
    use std::sync::{Arc, atomic::AtomicUsize};

    use super::{
        PendingPresentation, PresentationResults,
        hosted_cycle::pointer_hit_graph_candidate_for_hosted_output,
        platform_provider::NativePlatformCoordinator,
    };

    fn candidate_context() -> (egui::Context, egui::PointerHitGraphCandidate) {
        let context = egui::Context::default();
        let output = context.run_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(120.0, 80.0),
                )),
                ..Default::default()
            },
            |ui| {
                let _ = ui.button("target");
            },
        );
        let Some(candidate) = output.pointer_hit_graph_candidate else {
            panic!("completed pass must emit a hit graph candidate");
        };
        (context, candidate)
    }

    fn next_pointer_hit(
        context: &egui::Context,
    ) -> egui::PointerReceiverAuthority<egui::PointerHit> {
        let output = context.run_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(120.0, 80.0),
                )),
                events: vec![egui::Event::PointerMoved(egui::pos2(10.0, 10.0)).into()],
                ..Default::default()
            },
            |ui| {
                let _ = ui.button("target");
            },
        );
        let Some(record) = output.pointer_receiver_journal.records.into_iter().next() else {
            panic!("expected one pointer receiver record");
        };
        record.hit
    }

    fn result_log() -> (
        Arc<egui::mutex::Mutex<Vec<egui::PresentationResult>>>,
        PresentationResults,
        crate::NativeViewportBinding,
    ) {
        let results = Arc::new(egui::mutex::Mutex::new(Vec::new()));
        let hook_results = Arc::clone(&results);
        let hook = Arc::new(move |result| {
            hook_results.lock().push(result);
        });
        let coordinator = Arc::new(egui::mutex::Mutex::new(NativePlatformCoordinator::default()));
        let binding = coordinator
            .lock()
            .register_viewport(egui::ViewportId::ROOT)
            .unwrap();
        (
            results,
            PresentationResults::new(Some(hook), coordinator),
            binding,
        )
    }

    fn silent_results() -> (PresentationResults, crate::NativeViewportBinding) {
        let coordinator = Arc::new(egui::mutex::Mutex::new(NativePlatformCoordinator::default()));
        let binding = coordinator
            .lock()
            .register_viewport(egui::ViewportId::ROOT)
            .unwrap();
        (PresentationResults::new(None, coordinator), binding)
    }

    #[test]
    fn pending_presentation_dispatches_exactly_one_explicit_outcome() {
        let (results, presentation_results, binding) = result_log();
        PendingPresentation::new(
            presentation_results.clone(),
            egui::ViewportId::ROOT,
            Some(binding),
            Some(egui::UserData::new(7_u64)),
        )
        .complete(egui::PaintOutcome::SubmittedToSwapchain);

        assert!(results.lock().is_empty());
        presentation_results.drain();
        let results = results.lock();
        assert_eq!(results.len(), 1);
        assert_eq!(
            results[0].outcome(),
            &egui::PaintOutcome::SubmittedToSwapchain
        );
        assert_eq!(results[0].token().downcast_ref::<u64>(), Some(&7));
    }

    #[test]
    fn native_staging_output_cannot_install_live_pointer_authority() {
        let (_context, candidate) = candidate_context();

        let retained = pointer_hit_graph_candidate_for_hosted_output(
            egui::ViewportId::ROOT,
            Some(candidate.clone()),
            true,
        );

        assert!(retained.is_none());
        assert!(!candidate.settle(&egui::PaintOutcome::SubmittedToSwapchain));
    }

    #[test]
    fn successful_result_carries_the_exact_presented_hit_graph() {
        let (_context, candidate) = candidate_context();
        let expected_snapshot = candidate.snapshot().clone();
        let (results, presentation_results, binding) = result_log();
        PendingPresentation::new(
            presentation_results.clone(),
            egui::ViewportId::ROOT,
            Some(binding),
            Some(egui::UserData::new(8_u64)),
        )
        .with_pointer_hit_graph_candidate(Some(candidate))
        .complete(egui::PaintOutcome::Swapped);

        presentation_results.drain();
        let results = results.lock();
        let Some(result) = results.first() else {
            panic!("successful renderer outcome must produce one result");
        };
        assert_eq!(
            result.presented_pointer_hit_graph(),
            Some(&expected_snapshot)
        );
        assert_eq!(result.viewport_id(), egui::ViewportId::ROOT);
    }

    #[test]
    fn successful_outcome_without_application_token_promotes_hit_graph() {
        let (context, candidate) = candidate_context();
        let (presentation_results, binding) = silent_results();
        PendingPresentation::new(
            presentation_results,
            egui::ViewportId::ROOT,
            Some(binding),
            None,
        )
        .with_pointer_hit_graph_candidate(Some(candidate))
        .complete(egui::PaintOutcome::SubmittedToSwapchain);

        assert!(matches!(
            next_pointer_hit(&context),
            egui::PointerReceiverAuthority::Known(_)
        ));
    }

    #[test]
    fn failed_outcome_without_application_token_terminally_rejects_hit_graph() {
        let (context, candidate) = candidate_context();
        let retained_candidate = candidate.clone();
        let (presentation_results, binding) = silent_results();
        PendingPresentation::new(
            presentation_results,
            egui::ViewportId::ROOT,
            Some(binding),
            None,
        )
        .with_pointer_hit_graph_candidate(Some(candidate))
        .complete(egui::PaintOutcome::Failed(
            egui::PaintFailure::RendererUnavailable,
        ));

        assert!(!retained_candidate.settle(&egui::PaintOutcome::Swapped));
        assert!(matches!(
            next_pointer_hit(&context),
            egui::PointerReceiverAuthority::Unknown(
                egui::PointerReceiverUnavailableReason::PreviousPassNotPresented
            )
        ));
    }

    #[test]
    fn mismatched_viewport_without_application_token_terminally_rejects_hit_graph() {
        let (context, candidate) = candidate_context();
        let retained_candidate = candidate.clone();
        let (presentation_results, binding) = silent_results();
        PendingPresentation::new(
            presentation_results,
            egui::ViewportId::from_hash_of("mismatched viewport"),
            Some(binding),
            None,
        )
        .with_pointer_hit_graph_candidate(Some(candidate))
        .complete(egui::PaintOutcome::Swapped);

        assert!(!retained_candidate.settle(&egui::PaintOutcome::Swapped));
        assert!(matches!(
            next_pointer_hit(&context),
            egui::PointerReceiverAuthority::Unknown(
                egui::PointerReceiverUnavailableReason::PreviousPassNotPresented
            )
        ));
    }

    #[test]
    fn dropped_presentation_without_application_token_rejects_hit_graph() {
        let (context, candidate) = candidate_context();
        let retained_candidate = candidate.clone();
        let (presentation_results, binding) = silent_results();
        drop(
            PendingPresentation::new(
                presentation_results,
                egui::ViewportId::ROOT,
                Some(binding),
                None,
            )
            .with_pointer_hit_graph_candidate(Some(candidate)),
        );

        assert!(!retained_candidate.settle(&egui::PaintOutcome::Swapped));
        assert!(matches!(
            next_pointer_hit(&context),
            egui::PointerReceiverAuthority::Unknown(
                egui::PointerReceiverUnavailableReason::PreviousPassNotPresented
            )
        ));
    }

    #[test]
    fn pending_presentation_reports_coordinator_abort_on_drop() {
        let (results, presentation_results, binding) = result_log();
        drop(PendingPresentation::new(
            presentation_results.clone(),
            egui::ViewportId::ROOT,
            Some(binding),
            Some(egui::UserData::new(9_u64)),
        ));

        assert!(results.lock().is_empty());
        presentation_results.drain();
        let results = results.lock();
        assert_eq!(results.len(), 1);
        assert_eq!(
            results[0].outcome(),
            &egui::PaintOutcome::Failed(egui::PaintFailure::CoordinatorAborted)
        );
    }

    #[test]
    fn dropped_presentation_quiesces_its_exact_retired_binding() {
        let coordinator = Arc::new(egui::mutex::Mutex::new(NativePlatformCoordinator::default()));
        let binding = coordinator
            .lock()
            .register_viewport(egui::ViewportId::ROOT)
            .unwrap();
        let presentation_results = PresentationResults::new(None, Arc::clone(&coordinator));
        let pending = PendingPresentation::new(
            presentation_results,
            egui::ViewportId::ROOT,
            Some(binding),
            Some(egui::UserData::new("retired-drop")),
        );
        coordinator.lock().retire_viewport(binding).unwrap();

        drop(pending);

        let mut coordinator = coordinator.lock();
        coordinator
            .record_platform_facts(
                crate::NativeAuthority::unknown(crate::NativeUnavailableReason::Unsupported),
                crate::NativeAuthority::unknown(crate::NativeUnavailableReason::Unsupported),
                crate::NativeAuthority::unknown(crate::NativeUnavailableReason::Unsupported),
            )
            .unwrap();
        let ingress = coordinator.freeze_host_ingress().unwrap();
        assert_eq!(ingress.presentation_results().len(), 1);
        assert_eq!(ingress.retirement_quiescences().len(), 1);
        assert_eq!(ingress.retirement_quiescences()[0].binding(), binding);
    }

    #[test]
    #[expect(
        clippy::disallowed_methods,
        reason = "this regression test verifies the Drop path during Rust unwinding"
    )]
    fn pending_presentation_reports_coordinator_abort_while_unwinding() {
        let (results, presentation_results, binding) = result_log();
        let unwind = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _pending = PendingPresentation::new(
                presentation_results.clone(),
                egui::ViewportId::ROOT,
                Some(binding),
                Some(egui::UserData::new(11_u64)),
            );
            panic!("test native coordinator unwind");
        }));

        assert!(unwind.is_err());
        assert!(results.lock().is_empty());
        presentation_results.drain();
        let results = results.lock();
        assert_eq!(results.len(), 1);
        assert_eq!(
            results[0].outcome(),
            &egui::PaintOutcome::Failed(egui::PaintFailure::CoordinatorAborted)
        );
        assert_eq!(results[0].token().downcast_ref::<u64>(), Some(&11));
    }

    #[test]
    #[expect(
        clippy::disallowed_methods,
        reason = "this regression test verifies an outcome settled before later unwinding"
    )]
    fn settled_renderer_outcome_survives_later_unwinding() {
        let (results, presentation_results, binding) = result_log();
        let unwind = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            PendingPresentation::new(
                presentation_results.clone(),
                egui::ViewportId::ROOT,
                Some(binding),
                Some(egui::UserData::new(13_u64)),
            )
            .complete(egui::PaintOutcome::Swapped);
            panic!("test post-render unwind");
        }));

        assert!(unwind.is_err());
        presentation_results.drain();
        let results = results.lock();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].outcome(), &egui::PaintOutcome::Swapped);
    }

    #[test]
    fn panicking_hook_is_isolated_and_does_not_block_later_results() {
        let calls = Arc::new(AtomicUsize::new(0));
        let hook_calls = Arc::clone(&calls);
        let hook = Arc::new(move |_result| {
            hook_calls.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            panic!("test hook panic");
        });
        let coordinator = Arc::new(egui::mutex::Mutex::new(NativePlatformCoordinator::default()));
        let binding = coordinator
            .lock()
            .register_viewport(egui::ViewportId::ROOT)
            .unwrap();
        let presentation_results = PresentationResults::new(Some(hook), coordinator);

        for token in [17_u64, 19] {
            PendingPresentation::new(
                presentation_results.clone(),
                egui::ViewportId::ROOT,
                Some(binding),
                Some(egui::UserData::new(token)),
            )
            .complete(egui::PaintOutcome::SubmittedToSwapchain);
        }

        presentation_results.drain();
        assert_eq!(calls.load(std::sync::atomic::Ordering::Relaxed), 2);
    }
}

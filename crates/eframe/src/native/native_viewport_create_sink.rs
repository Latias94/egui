//! Cycle-scoped scheduling for native viewports that have no binding yet.

#![allow(
    dead_code,
    reason = "the native event-loop driver is connected in the following integration commit"
)]

use std::{collections::BTreeSet, sync::Arc};

use super::platform_provider::{
    NativePlatformError, NativeViewportBinding, NativeViewportCreateCorrelation,
    NativeViewportCreateDispatchOutcome, NativeViewportCreateRequest,
    NativeViewportCreateRequestId, SharedNativePlatformCoordinator,
};

#[derive(Debug)]
struct NativeViewportCreateSinkState {
    open: bool,
    requests: Vec<NativeViewportCreateRequest>,
}

/// A cycle-scoped typed queue for creating deferred native viewports.
///
/// A request names an exact live parent and a logical viewport that has no
/// native binding. Acceptance into this queue does not register an incarnation.
/// The backend records a terminal scheduling result after seal; a later native
/// roster observation is the only path that can mint the new binding.
#[derive(Clone)]
pub struct NativeViewportCreateSink {
    coordinator: SharedNativePlatformCoordinator,
    roster: Arc<BTreeSet<NativeViewportBinding>>,
    state: Arc<egui::mutex::Mutex<NativeViewportCreateSinkState>>,
}

impl std::fmt::Debug for NativeViewportCreateSink {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let state = self.state.lock();
        formatter
            .debug_struct("NativeViewportCreateSink")
            .field("roster", &self.roster)
            .field("open", &state.open)
            .field("pending_requests", &state.requests.len())
            .finish_non_exhaustive()
    }
}

impl NativeViewportCreateSink {
    pub(super) fn new(
        coordinator: SharedNativePlatformCoordinator,
        roster: impl IntoIterator<Item = NativeViewportBinding>,
    ) -> Self {
        Self {
            coordinator,
            roster: Arc::new(roster.into_iter().collect()),
            state: Arc::new(egui::mutex::Mutex::new(NativeViewportCreateSinkState {
                open: true,
                requests: Vec::new(),
            })),
        }
    }

    /// Queue one deferred viewport for post-seal event-loop scheduling.
    ///
    /// # Errors
    ///
    /// Returns an error when the cycle is closed, the exact parent is outside
    /// the frozen roster, the logical viewport already has a binding, or the
    /// coordinator rejects the creation lane.
    pub fn submit(
        &self,
        parent: NativeViewportBinding,
        viewport_id: egui::ViewportId,
        builder: egui::ViewportBuilder,
        viewport_ui_cb: Arc<egui::DeferredViewportUiCallback>,
        correlation: NativeViewportCreateCorrelation,
    ) -> Result<NativeViewportCreateRequestId, NativeViewportCreateSubmitError> {
        let mut state = self.state.lock();
        if !state.open {
            return Err(NativeViewportCreateSubmitError::CycleClosed);
        }
        if !self.roster.contains(&parent) {
            return Err(NativeViewportCreateSubmitError::ParentBindingUnavailable { parent });
        }
        if self
            .roster
            .iter()
            .any(|binding| binding.viewport_id() == viewport_id)
        {
            return Err(NativeViewportCreateSubmitError::ViewportAlreadyBound { viewport_id });
        }
        let id = self
            .coordinator
            .lock()
            .issue_viewport_create(parent, viewport_id, correlation.clone())
            .map_err(NativeViewportCreateSubmitError::Platform)?;
        state.requests.push(NativeViewportCreateRequest {
            id,
            parent,
            viewport_id,
            builder,
            viewport_ui_cb,
            correlation,
        });
        Ok(id)
    }

    pub(super) fn close_and_drain(&self) -> Vec<NativeViewportCreateRequest> {
        let mut state = self.state.lock();
        state.open = false;
        std::mem::take(&mut state.requests)
    }

    pub(super) fn accept_schedule(&self, request: &NativeViewportCreateRequest) -> bool {
        let mut coordinator = self.coordinator.lock();
        let Err(schedule_error) = coordinator.accept_viewport_create_schedule(request) else {
            return true;
        };
        let terminal = coordinator.report_viewport_create_terminal(
            request,
            NativeViewportCreateDispatchOutcome::Rejected,
        );
        match terminal {
            Ok(()) => log::error!(
                "native viewport schedule was rejected before materialization: {schedule_error}"
            ),
            Err(terminal_error) => log::error!(
                "failed to reject native viewport schedule ({schedule_error}); terminal result also failed: {terminal_error}"
            ),
        }
        false
    }

    pub(super) fn report_terminal(
        &self,
        request: &NativeViewportCreateRequest,
        outcome: NativeViewportCreateDispatchOutcome,
    ) {
        if let Err(error) = self
            .coordinator
            .lock()
            .report_viewport_create_terminal(request, outcome)
        {
            log::error!("failed to record native viewport creation result: {error}");
        }
    }

    pub(super) fn cancel_request(&self, request: &NativeViewportCreateRequest) {
        if let Err(error) = self
            .coordinator
            .lock()
            .cancel_viewport_create_issue(request)
        {
            log::error!("failed to cancel aborted native viewport creation request: {error}");
        }
    }

    pub(super) fn close_and_cancel(&self) {
        for request in self.close_and_drain() {
            self.cancel_request(&request);
        }
    }
}

/// Why a hosted cycle could not accept a native viewport creation request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NativeViewportCreateSubmitError {
    /// The exact hosted cycle has already sealed or aborted.
    CycleClosed,
    /// The exact parent lifetime is outside this cycle's native roster.
    ParentBindingUnavailable {
        /// The rejected exact parent lifetime.
        parent: NativeViewportBinding,
    },
    /// The logical viewport already has an exact native lifetime.
    ViewportAlreadyBound {
        /// The rejected logical viewport.
        viewport_id: egui::ViewportId,
    },
    /// The native coordinator rejected the request.
    Platform(NativePlatformError),
}

impl std::fmt::Display for NativeViewportCreateSubmitError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::CycleClosed => formatter.write_str("native viewport creation cycle is closed"),
            Self::ParentBindingUnavailable { parent } => write!(
                formatter,
                "native viewport parent {:?} is outside this creation cycle",
                parent.viewport_id()
            ),
            Self::ViewportAlreadyBound { viewport_id } => write!(
                formatter,
                "native viewport {viewport_id:?} already has an active binding"
            ),
            Self::Platform(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for NativeViewportCreateSubmitError {}

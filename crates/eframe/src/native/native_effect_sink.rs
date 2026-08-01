//! Cycle-scoped submission of exact native window effects.

#![allow(
    dead_code,
    reason = "the native event-loop driver is connected in the following integration commit"
)]

use std::{collections::BTreeSet, sync::Arc};

use super::platform_provider::{
    NativeEffectCorrelation, NativeEffectRequest, NativeEffectRequestId, NativePlatformError,
    NativeViewportBinding, NativeWindowEffect, SharedNativePlatformCoordinator,
};

#[derive(Debug)]
struct NativeEffectSinkState {
    open: bool,
    requests: Vec<NativeEffectRequest>,
}

/// A cycle-scoped typed queue for native window effects.
///
/// Submitting an effect reserves its exact binding and property lane in the
/// native coordinator. The native backend dispatches submitted operations only
/// after the hosted output transaction seals. A successful submission or
/// dispatch is never an acknowledgement that the platform applied the effect;
/// only a later correlated property observation can prove that.
#[derive(Clone)]
pub struct NativeEffectSink {
    coordinator: SharedNativePlatformCoordinator,
    roster: Arc<BTreeSet<NativeViewportBinding>>,
    state: Arc<egui::mutex::Mutex<NativeEffectSinkState>>,
}

impl std::fmt::Debug for NativeEffectSink {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let state = self.state.lock();
        formatter
            .debug_struct("NativeEffectSink")
            .field("roster", &self.roster)
            .field("open", &state.open)
            .field("pending_requests", &state.requests.len())
            .finish_non_exhaustive()
    }
}

impl NativeEffectSink {
    pub(super) fn new(
        coordinator: SharedNativePlatformCoordinator,
        roster: impl IntoIterator<Item = NativeViewportBinding>,
    ) -> Self {
        Self {
            coordinator,
            roster: Arc::new(roster.into_iter().collect()),
            state: Arc::new(egui::mutex::Mutex::new(NativeEffectSinkState {
                open: true,
                requests: Vec::new(),
            })),
        }
    }

    /// Reserves and queues one exact native operation for post-seal dispatch.
    ///
    /// The returned request identity correlates the later immediate dispatch
    /// result. It does not prove that the backend dispatched or applied the
    /// operation.
    ///
    /// # Errors
    ///
    /// Returns an error when this cycle has sealed, the binding is outside this
    /// cycle's exact roster, or the coordinator rejects the property lane.
    pub fn submit(
        &self,
        binding: NativeViewportBinding,
        effect: NativeWindowEffect,
        correlation: NativeEffectCorrelation,
    ) -> Result<NativeEffectRequestId, NativeEffectSubmitError> {
        let mut state = self.state.lock();
        if !state.open {
            return Err(NativeEffectSubmitError::CycleClosed);
        }
        if !self.roster.contains(&binding) {
            return Err(NativeEffectSubmitError::BindingUnavailable { binding });
        }
        let request = self
            .coordinator
            .lock()
            .issue_effect(binding, effect, correlation)
            .map_err(NativeEffectSubmitError::Platform)?;
        let request_id = request.id();
        state.requests.push(request);
        Ok(request_id)
    }

    pub(super) fn close_and_drain(&self) -> Vec<NativeEffectRequest> {
        let mut state = self.state.lock();
        state.open = false;
        std::mem::take(&mut state.requests)
    }

    pub(super) fn close_and_cancel(&self) {
        for request in self.close_and_drain() {
            if let Err(error) = self.coordinator.lock().cancel_effect_issue(&request) {
                log::error!("failed to cancel aborted native effect request: {error}");
            }
        }
    }
}

/// Why a hosted cycle could not accept a native effect submission.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NativeEffectSubmitError {
    /// The exact hosted cycle has already sealed or aborted.
    CycleClosed,
    /// The submitted lifetime is outside this cycle's exact native roster.
    BindingUnavailable {
        /// The rejected exact native viewport lifetime.
        binding: NativeViewportBinding,
    },
    /// The native coordinator rejected the request.
    Platform(NativePlatformError),
}

impl std::fmt::Display for NativeEffectSubmitError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::CycleClosed => formatter.write_str("native effect cycle is closed"),
            Self::BindingUnavailable { binding } => write!(
                formatter,
                "native viewport {:?} is outside this effect cycle",
                binding.viewport_id()
            ),
            Self::Platform(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for NativeEffectSubmitError {}

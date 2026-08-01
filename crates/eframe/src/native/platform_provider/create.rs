//! Typed scheduling facts for native viewports that do not have a binding yet.

use std::sync::Arc;

use super::authority::{NativeViewportBinding, NativeViewportCreateRequestId};

/// Opaque application correlation retained through native viewport scheduling.
#[derive(Clone, Debug, PartialEq)]
pub struct NativeViewportCreateCorrelation(egui::UserData);

impl NativeViewportCreateCorrelation {
    /// Attach opaque application state to one creation request.
    pub const fn new(value: egui::UserData) -> Self {
        Self(value)
    }

    /// Borrow the original opaque correlation value.
    pub const fn user_data(&self) -> &egui::UserData {
        &self.0
    }
}

/// The terminal event-loop outcome for a viewport creation request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NativeViewportCreateDispatchOutcome {
    /// The backend created the native window and registered its exact binding.
    Materialized,
    /// The request conflicted with the sealed output roster or lost its parent.
    Rejected,
    /// This backend cannot create a native viewport for the request.
    Unsupported,
    /// The backend accepted the request but failed to create its native resources.
    Failed,
}

/// A creation request retained by the backend until native materialization terminates.
pub(crate) struct NativeViewportCreateRequest {
    pub(crate) id: NativeViewportCreateRequestId,
    pub(crate) parent: NativeViewportBinding,
    pub(crate) viewport_id: egui::ViewportId,
    pub(crate) builder: egui::ViewportBuilder,
    pub(crate) viewport_ui_cb: Arc<egui::DeferredViewportUiCallback>,
    pub(crate) correlation: NativeViewportCreateCorrelation,
}

impl std::fmt::Debug for NativeViewportCreateRequest {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("NativeViewportCreateRequest")
            .field("id", &self.id)
            .field("parent", &self.parent)
            .field("viewport_id", &self.viewport_id)
            .field("builder", &self.builder)
            .field("correlation", &self.correlation)
            .finish_non_exhaustive()
    }
}

/// The ordered terminal materialization fact for one viewport creation request.
#[derive(Clone, Debug, PartialEq)]
pub struct NativeViewportCreateResult {
    pub(crate) request_id: NativeViewportCreateRequestId,
    pub(crate) parent: NativeViewportBinding,
    pub(crate) viewport_id: egui::ViewportId,
    pub(crate) outcome: NativeViewportCreateDispatchOutcome,
    pub(crate) correlation: NativeViewportCreateCorrelation,
}

impl NativeViewportCreateResult {
    /// Return the coordinator-minted request identity.
    pub const fn request_id(&self) -> NativeViewportCreateRequestId {
        self.request_id
    }

    /// Return the exact parent lifetime that authorized the request.
    pub const fn parent(&self) -> NativeViewportBinding {
        self.parent
    }

    /// Return the logical viewport that did not yet have a native binding.
    pub const fn viewport_id(&self) -> egui::ViewportId {
        self.viewport_id
    }

    /// Return the event-loop scheduling outcome.
    pub const fn outcome(&self) -> NativeViewportCreateDispatchOutcome {
        self.outcome
    }

    /// Borrow the original opaque correlation value.
    pub const fn correlation(&self) -> &NativeViewportCreateCorrelation {
        &self.correlation
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(super) struct PendingViewportCreate {
    pub(super) id: NativeViewportCreateRequestId,
    pub(super) parent: NativeViewportBinding,
    pub(super) correlation: NativeViewportCreateCorrelation,
    pub(super) scheduled: bool,
}

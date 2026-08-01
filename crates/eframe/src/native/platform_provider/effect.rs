//! Native effect requests, dispatch results, and causal observations.

use super::authority::{
    NativeAuthority, NativeEffectProperty, NativeEffectRequestId, NativeObservationGeneration,
    NativePhysicalRect, NativeUnavailableReason, NativeViewportBinding,
};

/// An opaque correlation value preserved through effect acknowledgement.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct NativeEffectCorrelation(egui::UserData);

impl NativeEffectCorrelation {
    /// Wrap an application-owned opaque correlation value.
    pub fn new(value: egui::UserData) -> Self {
        Self(value)
    }

    /// Return the untouched opaque correlation value.
    pub fn user_data(&self) -> &egui::UserData {
        &self.0
    }
}

#[derive(Clone, Debug, PartialEq)]
enum NativeEffectAcknowledgementState {
    Known(Option<NativeEffectCorrelation>),
    Unknown(NativeUnavailableReason),
}

/// Whether an observation acknowledges a previously dispatched native effect.
#[derive(Clone, Debug, PartialEq)]
pub struct NativeEffectAcknowledgement(NativeEffectAcknowledgementState);

impl NativeEffectAcknowledgement {
    pub(super) fn baseline() -> Self {
        Self(NativeEffectAcknowledgementState::Known(None))
    }

    pub(super) fn applied(correlation: NativeEffectCorrelation) -> Self {
        Self(NativeEffectAcknowledgementState::Known(Some(correlation)))
    }

    pub(super) fn unknown(reason: NativeUnavailableReason) -> Self {
        Self(NativeEffectAcknowledgementState::Unknown(reason))
    }

    /// Return the correlation of the acknowledged request, if this proves one.
    pub fn correlation(&self) -> Option<&NativeEffectCorrelation> {
        match &self.0 {
            NativeEffectAcknowledgementState::Known(correlation) => correlation.as_ref(),
            NativeEffectAcknowledgementState::Unknown(_) => None,
        }
    }

    /// Return why acknowledgement authority was unavailable.
    pub fn unavailable_reason(&self) -> Option<NativeUnavailableReason> {
        match self.0 {
            NativeEffectAcknowledgementState::Known(_) => None,
            NativeEffectAcknowledgementState::Unknown(reason) => Some(reason),
        }
    }
}

/// One generation-bound observation of a native property.
#[derive(Clone, Debug, PartialEq)]
pub struct NativePropertyObservation<T> {
    pub(super) binding: NativeViewportBinding,
    pub(super) property: NativeEffectProperty,
    pub(super) generation: NativeObservationGeneration,
    pub(super) value: NativeAuthority<T>,
    pub(super) acknowledgement: NativeEffectAcknowledgement,
}

impl<T> NativePropertyObservation<T> {
    /// Return the exact viewport lifetime observed.
    pub const fn binding(&self) -> NativeViewportBinding {
        self.binding
    }

    /// Return the property lane observed.
    pub const fn property(&self) -> NativeEffectProperty {
        self.property
    }

    /// Return this property's observation generation.
    pub const fn generation(&self) -> NativeObservationGeneration {
        self.generation
    }

    /// Return the observed authority.
    pub const fn value(&self) -> &NativeAuthority<T> {
        &self.value
    }

    /// Return the effect acknowledgement carried by this observation.
    pub const fn acknowledgement(&self) -> &NativeEffectAcknowledgement {
        &self.acknowledgement
    }
}

/// A native operation that may be dispatched by an event-loop integration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NativeWindowEffect {
    /// Set native visibility.
    SetVisible(bool),
    /// Set native outer geometry.
    SetOuterRect(NativePhysicalRect),
    /// Ask the platform to focus this viewport.
    RequestFocus,
    /// Enable or disable native pointer pass-through.
    SetPointerPassThrough(bool),
    /// Cancel a pending native close request.
    CancelClose,
    /// Destroy the native viewport.
    Destroy,
}

impl NativeWindowEffect {
    pub(super) const fn property(&self) -> NativeEffectProperty {
        match self {
            Self::SetVisible(_) => NativeEffectProperty::Presentation,
            Self::SetOuterRect(_) => NativeEffectProperty::Geometry,
            Self::RequestFocus => NativeEffectProperty::Focus,
            Self::SetPointerPassThrough(_) => NativeEffectProperty::PointerInput,
            Self::CancelClose => NativeEffectProperty::Close,
            Self::Destroy => NativeEffectProperty::Lifecycle,
        }
    }
}

/// A native effect request fenced against the latest property observation.
#[derive(Debug, PartialEq, Eq)]
pub struct NativeEffectRequest {
    pub(super) id: NativeEffectRequestId,
    pub(super) binding: NativeViewportBinding,
    pub(super) property: NativeEffectProperty,
    pub(super) observation_fence: NativeObservationGeneration,
    pub(super) correlation: NativeEffectCorrelation,
    pub(super) effect: NativeWindowEffect,
}

impl NativeEffectRequest {
    /// Return the request identity.
    pub const fn id(&self) -> NativeEffectRequestId {
        self.id
    }

    /// Return the exact target viewport lifetime.
    pub const fn binding(&self) -> NativeViewportBinding {
        self.binding
    }

    /// Return the affected property lane.
    pub const fn property(&self) -> NativeEffectProperty {
        self.property
    }

    /// Return the last observation generation preceding dispatch.
    pub const fn observation_fence(&self) -> NativeObservationGeneration {
        self.observation_fence
    }

    /// Return the opaque request correlation.
    pub const fn correlation(&self) -> &NativeEffectCorrelation {
        &self.correlation
    }

    /// Return the operation to dispatch.
    pub const fn effect(&self) -> &NativeWindowEffect {
        &self.effect
    }
}

/// The immediate result of asking a native backend to dispatch an effect.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NativeEffectDispatchOutcome {
    /// The operation was dispatched, but application is not yet proven.
    Dispatched,
    /// The backend synchronously rejected the operation.
    Rejected,
    /// The backend does not support the operation.
    Unsupported,
    /// The backend cannot determine whether dispatch occurred.
    Indeterminate,
}

/// The recorded immediate result of dispatching a native effect.
#[derive(Clone, Debug, PartialEq)]
pub struct NativeEffectResult {
    pub(super) request_id: NativeEffectRequestId,
    pub(super) binding: NativeViewportBinding,
    pub(super) property: NativeEffectProperty,
    pub(super) outcome: NativeEffectDispatchOutcome,
    pub(super) correlation: NativeEffectCorrelation,
}

impl NativeEffectResult {
    /// Return the request identity.
    pub const fn request_id(&self) -> NativeEffectRequestId {
        self.request_id
    }

    /// Return the exact target viewport lifetime.
    pub const fn binding(&self) -> NativeViewportBinding {
        self.binding
    }

    /// Return the affected property lane.
    pub const fn property(&self) -> NativeEffectProperty {
        self.property
    }

    /// Return the immediate dispatch outcome.
    pub const fn outcome(&self) -> NativeEffectDispatchOutcome {
        self.outcome
    }

    /// Return the opaque correlation supplied with the original request.
    pub const fn correlation(&self) -> &NativeEffectCorrelation {
        &self.correlation
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum PendingEffectPhase {
    Issued,
    Dispatched,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct PendingEffect {
    pub(super) id: NativeEffectRequestId,
    pub(super) correlation: NativeEffectCorrelation,
    pub(super) fence: NativeObservationGeneration,
    pub(super) phase: PendingEffectPhase,
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum ObservationAcknowledgement<'a> {
    Baseline,
    Applied(&'a NativeEffectRequest),
    Unknown(NativeUnavailableReason),
}

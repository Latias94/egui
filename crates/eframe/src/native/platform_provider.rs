//! Graph-agnostic native platform facts and causal authority.
//!
//! This module deliberately knows nothing about application topology or native window handles.
//! The event-loop integration owns those details and feeds only typed facts into this state owner.

#![expect(
    dead_code,
    reason = "the provider contract is staged before the native runtime integration"
)]
#![expect(
    unused_imports,
    reason = "the provider exports are staged before the native runtime integration"
)]

mod accessibility;
mod authority;
mod coordinator;
mod create;
mod effect;
mod ingress;
mod keyboard;
mod pointer;
mod presentation;
mod scroll;
mod snapshot;
mod work_area;

pub use accessibility::{NativeAccessibilityAction, NativeAccessibilityEdge};
pub use authority::{
    NativeAuthority, NativeCaptureOwner, NativeCloseState, NativeEffectProperty,
    NativeEffectRequestId, NativeFocusedWindow, NativeHoveredWindow, NativeObservationGeneration,
    NativePhysicalPoint, NativePhysicalRect, NativePlatformError, NativePlatformGeneration,
    NativePointerDeliveryOwner, NativePointerDeviceId, NativePointerId, NativePointerInputState,
    NativePointerSequence, NativePresentationState, NativeUnavailableReason, NativeViewportBinding,
    NativeViewportCreateRequestId, NativeViewportIncarnation,
};
#[cfg(test)]
pub(crate) use coordinator::NativePlatformCoordinator;
pub(crate) use coordinator::{NativePointerEdgeFacts, SharedNativePlatformCoordinator};
pub(crate) use create::NativeViewportCreateRequest;
pub use create::{
    NativeViewportCreateCorrelation, NativeViewportCreateDispatchOutcome,
    NativeViewportCreateResult,
};
pub(crate) use effect::ObservationAcknowledgement;
pub use effect::{
    NativeEffectAcknowledgement, NativeEffectCorrelation, NativeEffectDispatchOutcome,
    NativeEffectRequest, NativeEffectResult, NativePropertyObservation, NativeWindowEffect,
};
pub use ingress::{
    NativeEventEnvelopeReceipt, NativeHostIngress, NativeIngressEvent, NativeIngressJournal,
    NativeIngressOrdinal, NativeIngressRecord,
};
pub use keyboard::{NativeKey, NativeKeyEdge, NativeKeyEdgeKind};
pub use pointer::{
    NativePointerButton, NativePointerCoordinateCapture, NativePointerEdge, NativePointerEdgeKind,
    NativePointerIdentity, NativePointerJournal, NativePointerSource,
};
pub(crate) use presentation::NativePresentationTicket;
pub use presentation::{
    NativePresentationResult, NativePresentationSerial, NativeRetirementQuiesced,
    NativeRetirementTombstone,
};
pub use scroll::{
    NativeFiniteScrollVector, NativeScrollCancelReason, NativeScrollDelta, NativeScrollEdge,
    NativeScrollModifiers, NativeScrollMomentum, NativeScrollPhase, NativeScrollSequenceToken,
};
pub use snapshot::{
    NativeBackendCapabilities, NativeBackendCapability, NativePlatformFacts,
    NativePlatformSnapshot, NativeWindowGeometry, NativeWindowSnapshot,
};
#[expect(unused_imports, reason = "staged snapshot authority is not wired yet")]
pub use snapshot::{NativeGlobalObservation, NativePlatformSnapshotGeneration};
pub use work_area::{
    NativeWorkArea, NativeWorkAreaGeneration, NativeWorkAreaRosterObservation, NativeWorkAreaRoute,
    NativeWorkAreaToken,
};

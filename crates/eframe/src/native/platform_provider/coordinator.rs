//! Pure state owner for native authority, ledgers, and freeze boundaries.

use std::collections::{BTreeMap, BTreeSet};

use crate::native::{
    coordinator_wake::NativeCoordinatorWake,
    platform_ingress_owner::NativeBindingIngressOwnerQuiescence,
};

use super::{
    accessibility::NativeAccessibilityEdge,
    authority::{
        NativeAuthority, NativeCaptureOwner, NativeCloseState, NativeEffectProperty,
        NativeEffectRequestId, NativeFocusedWindow, NativeHoveredWindow,
        NativeObservationGeneration, NativePhysicalPoint, NativePhysicalRect, NativePlatformError,
        NativePlatformGeneration, NativePointerDeliveryOwner, NativePointerInputState,
        NativePointerSequence, NativePresentationState, NativeUnavailableReason,
        NativeViewportBinding, NativeViewportCreateRequestId, NativeViewportIncarnation,
    },
    create::{
        NativeViewportCreateCorrelation, NativeViewportCreateDispatchOutcome,
        NativeViewportCreateRequest, NativeViewportCreateResult, PendingViewportCreate,
    },
    effect::{
        NativeEffectAcknowledgement, NativeEffectCorrelation, NativeEffectDispatchOutcome,
        NativeEffectRequest, NativeEffectResult, NativePropertyObservation, NativeWindowEffect,
        ObservationAcknowledgement, PendingEffect, PendingEffectPhase,
    },
    ingress::{
        NativeHostIngress, NativeIngressJournal, NativeIngressOrdinal, NativeIngressRecord,
        NativeIngressRecordKind, NativeNoScrollDerivativeReason, NativeScrollDerivativeDisposition,
        derive_binding_ingress_quiescences, derive_effect_results, derive_pointer_edges,
        derive_presentation_results, derive_retirement_tombstones, derive_viewport_create_results,
        pointer_watermark,
    },
    keyboard::NativeKeyEdge,
    pointer::{
        NativePointerCoordinateCapture, NativePointerEdge, NativePointerEdgeKind,
        NativePointerIdentity, NativePointerJournal, NativePointerSource,
        NativePointerStreamCancelReason,
    },
    presentation::{
        NativeBindingIngressQuiesced, NativePresentationSerial, NativePresentationTicket,
        NativeRetirementTombstone,
    },
    scroll::NativeScrollEdge,
    snapshot::{
        NativeBackendCapabilities, NativeGlobalObservation, NativePlatformFacts,
        NativePlatformSnapshot, NativePlatformSnapshotGeneration, NativeWindowGeometry,
        NativeWindowSnapshot, PendingGlobalFacts,
    },
    work_area::{NativeWorkAreaRosterObservation, NativeWorkAreaRoute},
};

/// State owner for graph-agnostic native facts and causal ledgers.
///
/// `wake` is a non-authoritative liveness sidecar: it exposes only that a new
/// record exists and never participates in validation, ordering, or snapshots.
#[derive(Debug, Default)]
pub(crate) struct NativePlatformCoordinator {
    wake: NativeCoordinatorWake,
    active: BTreeMap<egui::ViewportId, NativeViewportBinding>,
    minted: BTreeSet<NativeViewportBinding>,
    tombstones: BTreeMap<NativeViewportBinding, NativeRetirementTombstone>,
    retired_binding_ingress: BTreeMap<NativeViewportBinding, RetiredBindingIngressState>,
    inventory_generation: u64,
    snapshot_generation: u64,
    next_incarnation: u64,
    next_presentation_serial: u64,
    presentation_lanes: BTreeMap<NativeViewportBinding, NativePresentationLane>,
    ingress_ordinal: u64,
    ingress_watermark: u64,
    pending_ingress_records: Vec<NativeIngressRecord>,
    pointer_sequence: u64,
    pointer_watermark: u64,
    host_ingress_settlement: HostIngressSettlementState,
    observation_generations:
        BTreeMap<(NativeViewportBinding, NativeEffectProperty), NativeObservationGeneration>,
    next_effect_id: u64,
    pending_effects: BTreeMap<(NativeViewportBinding, NativeEffectProperty), Vec<PendingEffect>>,
    next_viewport_create_id: u64,
    pending_viewport_creates: BTreeMap<egui::ViewportId, PendingViewportCreate>,
    pending_window_snapshots: BTreeMap<NativeViewportBinding, NativeWindowSnapshot>,
    pending_global_facts: Option<PendingGlobalFacts>,
}

#[derive(Debug, Default)]
enum HostIngressSettlementState {
    #[default]
    Idle,
    Prepared(PreparedHostIngressSettlement),
    Poisoned,
}

#[derive(Debug, PartialEq, Eq)]
struct PreparedHostIngressSettlement {
    key: NativeHostIngressSettlementKey,
    snapshot_generation: u64,
    ingress_through: u64,
    pointer_through: u64,
    quiesced_bindings: BTreeSet<NativeViewportBinding>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct NativeHostIngressSettlementKey(NativeIngressOrdinal);

#[derive(Debug)]
pub(crate) struct PreparedNativeHostIngress {
    ingress: NativeHostIngress,
    settlement: NativeHostIngressSettlementKey,
}

impl PreparedNativeHostIngress {
    pub(crate) fn into_parts(self) -> (NativeHostIngress, NativeHostIngressSettlementKey) {
        (self.ingress, self.settlement)
    }
}

#[derive(Debug, Default)]
struct NativePresentationLane {
    outstanding: BTreeSet<NativePresentationSerial>,
    last_started: Option<NativePresentationSerial>,
    retired: bool,
}

#[derive(Debug)]
struct RetiredBindingIngressState {
    retirement_generation: NativePlatformGeneration,
    last_started_presentation: Option<NativePresentationSerial>,
    owner_quiesced: bool,
}

pub(crate) type SharedNativePlatformCoordinator =
    std::sync::Arc<egui::mutex::Mutex<NativePlatformCoordinator>>;

pub(crate) struct NativePointerEdgeFacts {
    source: NativePointerSource,
    delivery_owner: NativeAuthority<NativePointerDeliveryOwner>,
    identity: NativePointerIdentity,
    kind: NativePointerEdgeKind,
    position: NativeAuthority<NativePhysicalPoint>,
    hovered: NativeAuthority<NativeHoveredWindow>,
    hovered_coordinates: NativeAuthority<NativePointerCoordinateCapture>,
    delivery_coordinates: NativeAuthority<NativePointerCoordinateCapture>,
    capture: NativeAuthority<NativeCaptureOwner>,
    work_area: NativeAuthority<NativeWorkAreaRoute>,
}

impl NativePointerEdgeFacts {
    pub(crate) fn new(
        source: NativePointerSource,
        delivery_owner: NativeAuthority<NativePointerDeliveryOwner>,
        identity: NativePointerIdentity,
        kind: NativePointerEdgeKind,
        position: NativeAuthority<NativePhysicalPoint>,
        hovered: NativeAuthority<NativeHoveredWindow>,
        capture: NativeAuthority<NativeCaptureOwner>,
    ) -> Self {
        Self {
            source,
            delivery_owner,
            identity,
            kind,
            position,
            hovered,
            hovered_coordinates: NativeAuthority::unknown(NativeUnavailableReason::NotObserved),
            delivery_coordinates: NativeAuthority::unknown(NativeUnavailableReason::NotObserved),
            capture,
            work_area: NativeAuthority::unknown(NativeUnavailableReason::NotObserved),
        }
    }

    pub(crate) fn with_hovered_coordinates(
        mut self,
        hovered_coordinates: NativeAuthority<NativePointerCoordinateCapture>,
    ) -> Self {
        self.hovered_coordinates = hovered_coordinates;
        self
    }

    pub(crate) fn with_delivery_coordinates(
        mut self,
        delivery_coordinates: NativeAuthority<NativePointerCoordinateCapture>,
    ) -> Self {
        self.delivery_coordinates = delivery_coordinates;
        self
    }

    pub(crate) fn with_work_area(
        mut self,
        work_area: NativeAuthority<NativeWorkAreaRoute>,
    ) -> Self {
        self.work_area = work_area;
        self
    }
}

impl NativePlatformCoordinator {
    pub(in crate::native) fn with_wake(wake: NativeCoordinatorWake) -> Self {
        Self {
            wake,
            ..Self::default()
        }
    }

    pub(crate) fn register_viewport(
        &mut self,
        viewport_id: egui::ViewportId,
    ) -> Result<NativeViewportBinding, NativePlatformError> {
        if self.active.contains_key(&viewport_id) {
            return Err(NativePlatformError::ViewportAlreadyRegistered);
        }

        let incarnation = self
            .next_incarnation
            .checked_add(1)
            .map(NativeViewportIncarnation::new)
            .ok_or(NativePlatformError::CounterExhausted)?;
        let inventory_generation = self.next_inventory_generation()?;
        let materialized = self
            .pending_viewport_creates
            .get(&viewport_id)
            .filter(|pending| pending.scheduled)
            .cloned();
        let materialized_ordinal = materialized
            .as_ref()
            .map(|_| self.next_ingress_ordinal())
            .transpose()?;
        let quiescence_parent = materialized.as_ref().and_then(|pending| {
            self.binding_ingress_can_quiesce_after_create(pending.parent, viewport_id)
                .then_some(pending.parent)
        });
        let quiescence_ordinal = quiescence_parent
            .map(|_| {
                materialized_ordinal
                    .map_or(self.ingress_ordinal, NativeIngressOrdinal::get)
                    .checked_add(1)
                    .map(NativeIngressOrdinal::new)
                    .ok_or(NativePlatformError::CounterExhausted)
            })
            .transpose()?;

        self.next_incarnation = incarnation.get();
        self.inventory_generation = inventory_generation.get();
        let binding = NativeViewportBinding::new(viewport_id, incarnation);
        self.active.insert(viewport_id, binding);
        self.minted.insert(binding);
        self.invalidate_staged_platform_snapshot();
        if let (Some(pending), Some(ordinal)) = (materialized, materialized_ordinal) {
            self.pending_viewport_creates.remove(&viewport_id);
            self.commit_ingress_record(
                ordinal,
                None,
                NativeIngressRecordKind::ViewportCreateResult(NativeViewportCreateResult {
                    request_id: pending.id,
                    parent: pending.parent,
                    viewport_id,
                    outcome: NativeViewportCreateDispatchOutcome::Materialized,
                    correlation: pending.correlation,
                }),
            );
        }
        if let (Some(parent), Some(ordinal)) = (quiescence_parent, quiescence_ordinal) {
            self.commit_binding_ingress_quiescence(parent, ordinal);
        }
        Ok(binding)
    }

    pub(crate) fn retire_viewport(
        &mut self,
        binding: NativeViewportBinding,
    ) -> Result<NativeRetirementTombstone, NativePlatformError> {
        self.retire_viewport_inner(binding, None, ObservationAcknowledgement::Baseline)
    }

    pub(crate) fn retire_viewport_for_backend(
        &mut self,
        binding: NativeViewportBinding,
        backend_event_sequence: egui::BackendEventSequence,
    ) -> Result<NativeRetirementTombstone, NativePlatformError> {
        self.retire_viewport_inner(
            binding,
            Some(backend_event_sequence),
            ObservationAcknowledgement::Baseline,
        )
    }

    pub(crate) fn retire_viewport_after_effect(
        &mut self,
        binding: NativeViewportBinding,
        request: &NativeEffectRequest,
    ) -> Result<NativeRetirementTombstone, NativePlatformError> {
        self.retire_viewport_inner(binding, None, ObservationAcknowledgement::Applied(request))
    }

    fn retire_viewport_inner(
        &mut self,
        binding: NativeViewportBinding,
        backend_event_sequence: Option<egui::BackendEventSequence>,
        acknowledgement: ObservationAcknowledgement<'_>,
    ) -> Result<NativeRetirementTombstone, NativePlatformError> {
        self.require_active(binding)?;
        let lifecycle_key = (binding, NativeEffectProperty::Lifecycle);
        self.validate_observation_acknowledgement(lifecycle_key, acknowledgement)?;
        let generation = self.next_inventory_generation()?;
        let ordinal = self.next_ingress_ordinal()?;
        let presentation_lane = self.presentation_lanes.get(&binding);
        let presentation_is_quiescent = presentation_lane
            .map(|lane| lane.outstanding.is_empty())
            .unwrap_or(true);
        let last_started_presentation = presentation_lane.and_then(|lane| lane.last_started);
        let acknowledgement = match acknowledgement {
            ObservationAcknowledgement::Baseline => NativeEffectAcknowledgement::baseline(),
            ObservationAcknowledgement::Unknown(reason) => {
                NativeEffectAcknowledgement::unknown(reason)
            }
            ObservationAcknowledgement::Applied(request) => {
                let pending = self.remove_pending_effect(lifecycle_key, request)?;
                NativeEffectAcknowledgement::applied(pending.correlation)
            }
        };
        self.inventory_generation = generation.get();
        self.active.remove(&binding.viewport_id);
        let tombstone = NativeRetirementTombstone {
            binding,
            generation,
            acknowledgement,
        };
        self.tombstones.insert(binding, tombstone.clone());
        self.retired_binding_ingress.insert(
            binding,
            RetiredBindingIngressState {
                retirement_generation: generation,
                last_started_presentation,
                owner_quiesced: false,
            },
        );
        if presentation_is_quiescent {
            self.presentation_lanes.remove(&binding);
        } else {
            self.presentation_lanes.entry(binding).or_default().retired = true;
        }
        self.commit_ingress_record(
            ordinal,
            backend_event_sequence,
            NativeIngressRecordKind::Retirement(tombstone.clone()),
        );
        self.pending_effects
            .retain(|(pending_binding, _), _| *pending_binding != binding);
        self.invalidate_staged_platform_snapshot();
        Ok(tombstone)
    }

    pub(in crate::native) fn confirm_binding_ingress_quiescence(
        &mut self,
        proof: NativeBindingIngressOwnerQuiescence,
    ) -> Result<(), NativePlatformError> {
        let binding = proof.into_binding();
        if self.active.values().any(|active| *active == binding)
            || self
                .pending_effects
                .keys()
                .any(|(pending_binding, _)| *pending_binding == binding)
            || self.pending_window_snapshots.contains_key(&binding)
        {
            return Err(NativePlatformError::BindingIngressNotQuiescent);
        }
        let presentation_lane = self.presentation_lanes.get(&binding);
        if presentation_lane.is_some_and(|lane| !lane.retired) {
            return Err(NativePlatformError::BindingIngressNotQuiescent);
        }
        let presentation_is_quiescent =
            presentation_lane.is_none_or(|lane| lane.outstanding.is_empty());
        let create_lane_is_quiescent = !self.binding_has_pending_viewport_create(binding, None);
        let quiescence_ordinal = (presentation_is_quiescent && create_lane_is_quiescent)
            .then(|| self.next_ingress_ordinal())
            .transpose()?;
        let state = self
            .retired_binding_ingress
            .get_mut(&binding)
            .ok_or(NativePlatformError::BindingIngressAlreadyQuiesced)?;
        if state.owner_quiesced {
            return Err(NativePlatformError::BindingIngressAlreadyQuiesced);
        }
        state.owner_quiesced = true;

        if let Some(ordinal) = quiescence_ordinal {
            self.commit_binding_ingress_quiescence(binding, ordinal);
        }
        Ok(())
    }

    fn binding_has_pending_viewport_create(
        &self,
        binding: NativeViewportBinding,
        completing_viewport: Option<egui::ViewportId>,
    ) -> bool {
        self.pending_viewport_creates
            .iter()
            .any(|(viewport, pending)| {
                pending.parent == binding && Some(*viewport) != completing_viewport
            })
    }

    fn binding_ingress_can_quiesce_after_create(
        &self,
        binding: NativeViewportBinding,
        completing_viewport: egui::ViewportId,
    ) -> bool {
        self.retired_binding_ingress
            .get(&binding)
            .is_some_and(|state| state.owner_quiesced)
            && !self.presentation_lanes.contains_key(&binding)
            && !self.binding_has_pending_viewport_create(binding, Some(completing_viewport))
    }

    fn commit_binding_ingress_quiescence(
        &mut self,
        binding: NativeViewportBinding,
        ordinal: NativeIngressOrdinal,
    ) {
        let state = self
            .retired_binding_ingress
            .remove(&binding)
            .expect("validated retired binding ingress state must remain present");
        debug_assert!(state.owner_quiesced);
        self.presentation_lanes.remove(&binding);
        self.commit_ingress_record(
            ordinal,
            None,
            NativeIngressRecordKind::BindingIngressQuiesced(NativeBindingIngressQuiesced {
                binding,
                retirement_generation: state.retirement_generation,
                last_started_presentation: state.last_started_presentation,
            }),
        );
    }

    pub(crate) fn active_binding(
        &self,
        viewport_id: egui::ViewportId,
    ) -> Option<NativeViewportBinding> {
        self.active.get(&viewport_id).copied()
    }

    pub(crate) fn retirement_tombstone(
        &self,
        binding: NativeViewportBinding,
    ) -> Option<NativeRetirementTombstone> {
        self.tombstones.get(&binding).cloned()
    }

    pub(crate) fn record_platform_facts(
        &mut self,
        focused: NativeAuthority<NativeFocusedWindow>,
        hovered: NativeAuthority<NativeHoveredWindow>,
        capture: NativeAuthority<NativeCaptureOwner>,
    ) -> Result<(), NativePlatformError> {
        self.record_platform_facts_with_work_areas(
            focused,
            hovered,
            capture,
            NativeWorkAreaRosterObservation::new(
                super::work_area::NativeWorkAreaGeneration::new(0),
                NativeAuthority::unknown(NativeUnavailableReason::NotObserved),
            ),
        )
    }

    pub(crate) fn record_platform_facts_with_work_areas(
        &mut self,
        focused: NativeAuthority<NativeFocusedWindow>,
        hovered: NativeAuthority<NativeHoveredWindow>,
        capture: NativeAuthority<NativeCaptureOwner>,
        work_areas: NativeWorkAreaRosterObservation,
    ) -> Result<(), NativePlatformError> {
        self.record_platform_facts_with_capabilities(
            focused,
            hovered,
            capture,
            NativeBackendCapabilities::default(),
            work_areas,
        )
    }

    pub(crate) fn record_platform_facts_with_capabilities(
        &mut self,
        focused: NativeAuthority<NativeFocusedWindow>,
        hovered: NativeAuthority<NativeHoveredWindow>,
        capture: NativeAuthority<NativeCaptureOwner>,
        capabilities: NativeBackendCapabilities,
        work_areas: NativeWorkAreaRosterObservation,
    ) -> Result<(), NativePlatformError> {
        self.validate_global_facts(&focused, &hovered, &capture)?;
        self.pending_global_facts = Some(PendingGlobalFacts {
            focused,
            hovered,
            capture,
            capabilities,
            work_areas,
        });
        Ok(())
    }

    pub(crate) fn window_geometry(
        &self,
        binding: NativeViewportBinding,
        content_rect: NativeAuthority<NativePhysicalRect>,
        outer_rect: NativeAuthority<NativePhysicalRect>,
        native_scale_factor: NativeAuthority<f64>,
        presentation_scale_factor: NativeAuthority<f64>,
        work_area: NativeAuthority<NativePhysicalRect>,
    ) -> Result<NativeWindowGeometry, NativePlatformError> {
        self.require_active(binding)?;
        Ok(NativeWindowGeometry {
            binding,
            content_rect,
            outer_rect,
            native_scale_factor,
            presentation_scale_factor,
            work_area,
        })
    }

    pub(crate) fn record_pointer_edge(
        &mut self,
        source: NativePointerSource,
        identity: NativePointerIdentity,
        kind: NativePointerEdgeKind,
        position: NativeAuthority<NativePhysicalPoint>,
        hovered: NativeAuthority<NativeHoveredWindow>,
        capture: NativeAuthority<NativeCaptureOwner>,
    ) -> Result<NativePointerSequence, NativePlatformError> {
        let delivery_owner = NativeAuthority::known(match source {
            NativePointerSource::Viewport(binding) => NativePointerDeliveryOwner::Viewport(binding),
            NativePointerSource::Foreign => NativePointerDeliveryOwner::Foreign,
            NativePointerSource::None => NativePointerDeliveryOwner::None,
        });
        self.record_pointer_edge_inner(
            None,
            NativeScrollDerivativeDisposition::ExplicitNoDerivative(
                NativeNoScrollDerivativeReason::JournalOnly,
            ),
            NativePointerEdgeFacts::new(
                source,
                delivery_owner,
                identity,
                kind,
                position,
                hovered,
                capture,
            ),
        )
    }

    pub(crate) fn record_synthetic_scroll_cancel(
        &mut self,
        source: NativePointerSource,
        identity: NativePointerIdentity,
        scroll: NativeScrollEdge,
    ) -> Result<NativePointerSequence, NativePlatformError> {
        let delivery_owner = NativeAuthority::known(match source {
            NativePointerSource::Viewport(binding) => NativePointerDeliveryOwner::Viewport(binding),
            NativePointerSource::Foreign => NativePointerDeliveryOwner::Foreign,
            NativePointerSource::None => NativePointerDeliveryOwner::None,
        });
        self.record_pointer_edge_inner(
            None,
            NativeScrollDerivativeDisposition::ExplicitNoDerivative(
                NativeNoScrollDerivativeReason::JournalOnly,
            ),
            NativePointerEdgeFacts::new(
                source,
                delivery_owner,
                identity,
                NativePointerEdgeKind::Scrolled(scroll),
                NativeAuthority::unknown(NativeUnavailableReason::Retired),
                NativeAuthority::unknown(NativeUnavailableReason::Retired),
                NativeAuthority::unknown(NativeUnavailableReason::Retired),
            ),
        )
    }

    pub(crate) fn record_synthetic_pointer_cancel(
        &mut self,
        identity: NativePointerIdentity,
        reason: NativePointerStreamCancelReason,
    ) -> Result<NativePointerSequence, NativePlatformError> {
        self.record_pointer_edge_inner(
            None,
            NativeScrollDerivativeDisposition::ExplicitNoDerivative(
                NativeNoScrollDerivativeReason::JournalOnly,
            ),
            NativePointerEdgeFacts::new(
                NativePointerSource::None,
                NativeAuthority::known(NativePointerDeliveryOwner::None),
                identity,
                NativePointerEdgeKind::StreamCancelled(reason),
                NativeAuthority::unknown(NativeUnavailableReason::Retired),
                NativeAuthority::unknown(NativeUnavailableReason::Retired),
                NativeAuthority::unknown(NativeUnavailableReason::Retired),
            ),
        )
    }

    pub(crate) fn record_pointer_edge_for_backend(
        &mut self,
        backend_event_sequence: egui::BackendEventSequence,
        facts: NativePointerEdgeFacts,
    ) -> Result<NativePointerSequence, NativePlatformError> {
        self.record_pointer_edge_inner(
            Some(backend_event_sequence),
            NativeScrollDerivativeDisposition::RequiredDerivative,
            facts,
        )
    }

    pub(crate) fn record_pointer_edge_without_derivative_for_backend(
        &mut self,
        backend_event_sequence: egui::BackendEventSequence,
        facts: NativePointerEdgeFacts,
    ) -> Result<NativePointerSequence, NativePlatformError> {
        self.record_pointer_edge_inner(
            Some(backend_event_sequence),
            NativeScrollDerivativeDisposition::ExplicitNoDerivative(
                NativeNoScrollDerivativeReason::JournalOnly,
            ),
            facts,
        )
    }

    pub(crate) fn record_key_edge_for_backend(
        &mut self,
        backend_event_sequence: egui::BackendEventSequence,
        edge: NativeKeyEdge,
    ) -> Result<(), NativePlatformError> {
        self.require_active(edge.binding())?;
        let ordinal = self.next_ingress_ordinal()?;
        self.commit_ingress_record(
            ordinal,
            Some(backend_event_sequence),
            NativeIngressRecordKind::KeyEdge(edge),
        );
        Ok(())
    }

    pub(crate) fn record_accessibility_edge_for_backend(
        &mut self,
        backend_event_sequence: egui::BackendEventSequence,
        edge: NativeAccessibilityEdge,
    ) -> Result<(), NativePlatformError> {
        self.require_active(edge.binding())?;
        let ordinal = self.next_ingress_ordinal()?;
        self.commit_ingress_record(
            ordinal,
            Some(backend_event_sequence),
            NativeIngressRecordKind::AccessibilityEdge(edge),
        );
        Ok(())
    }

    fn record_pointer_edge_inner(
        &mut self,
        backend_event_sequence: Option<egui::BackendEventSequence>,
        scroll_derivative: NativeScrollDerivativeDisposition,
        facts: NativePointerEdgeFacts,
    ) -> Result<NativePointerSequence, NativePlatformError> {
        self.validate_pointer_source(facts.source)?;
        if let Some(NativePointerDeliveryOwner::Viewport(binding)) = facts.delivery_owner.value() {
            self.require_active(*binding)?;
        }
        if let Some(NativeHoveredWindow::Viewport(binding)) = facts.hovered.value() {
            self.require_active(*binding)?;
        }
        if let Some(NativeCaptureOwner::Viewport(binding)) = facts.capture.value() {
            self.require_active(*binding)?;
        }
        let next_pointer_sequence = self
            .pointer_sequence
            .checked_add(1)
            .ok_or(NativePlatformError::CounterExhausted)?;
        let ordinal = self.next_ingress_ordinal()?;
        let sequence = NativePointerSequence::new(next_pointer_sequence);
        let edge = NativePointerEdge {
            sequence,
            source: facts.source,
            delivery_owner: facts.delivery_owner,
            identity: facts.identity,
            kind: facts.kind,
            position: facts.position,
            hovered: facts.hovered,
            hovered_coordinates: facts.hovered_coordinates,
            delivery_coordinates: facts.delivery_coordinates,
            capture: facts.capture,
            work_area: facts.work_area,
        };
        self.pointer_sequence = next_pointer_sequence;
        let kind = if matches!(edge.kind(), NativePointerEdgeKind::Scrolled(_)) {
            NativeIngressRecordKind::ScrollEdge {
                edge,
                derivative: scroll_derivative,
            }
        } else {
            NativeIngressRecordKind::PointerEdge(edge)
        };
        self.commit_ingress_record(ordinal, backend_event_sequence, kind);
        Ok(sequence)
    }

    pub(crate) fn record_close_observation_for_backend(
        &mut self,
        backend_event_sequence: egui::BackendEventSequence,
        binding: NativeViewportBinding,
        state: NativeCloseState,
        acknowledgement: ObservationAcknowledgement<'_>,
    ) -> Result<NativePropertyObservation<NativeCloseState>, NativePlatformError> {
        self.record_close_observation_inner(
            Some(backend_event_sequence),
            binding,
            state,
            acknowledgement,
        )
    }

    pub(crate) fn record_close_observation_after_effect(
        &mut self,
        binding: NativeViewportBinding,
        state: NativeCloseState,
        request: &NativeEffectRequest,
    ) -> Result<NativePropertyObservation<NativeCloseState>, NativePlatformError> {
        self.record_close_observation_inner(
            None,
            binding,
            state,
            ObservationAcknowledgement::Applied(request),
        )
    }

    fn record_close_observation_inner(
        &mut self,
        backend_event_sequence: Option<egui::BackendEventSequence>,
        binding: NativeViewportBinding,
        state: NativeCloseState,
        acknowledgement: ObservationAcknowledgement<'_>,
    ) -> Result<NativePropertyObservation<NativeCloseState>, NativePlatformError> {
        let ordinal = self.next_ingress_ordinal()?;
        let observation = self.observe_property(
            binding,
            NativeEffectProperty::Close,
            NativeAuthority::known(state),
            acknowledgement,
        )?;
        self.commit_ingress_record(
            ordinal,
            backend_event_sequence,
            NativeIngressRecordKind::CloseObservation(observation.clone()),
        );
        Ok(observation)
    }

    pub(crate) fn issue_effect(
        &mut self,
        binding: NativeViewportBinding,
        effect: NativeWindowEffect,
        correlation: NativeEffectCorrelation,
    ) -> Result<NativeEffectRequest, NativePlatformError> {
        self.require_active(binding)?;
        let property = effect.property();
        let key = (binding, property);
        self.next_effect_id = self
            .next_effect_id
            .checked_add(1)
            .ok_or(NativePlatformError::CounterExhausted)?;
        let id = NativeEffectRequestId::new(self.next_effect_id);
        let fence = self.current_observation_generation(key);
        self.pending_effects
            .entry(key)
            .or_default()
            .push(PendingEffect {
                id,
                correlation: correlation.clone(),
                fence,
                phase: PendingEffectPhase::Issued,
            });
        Ok(NativeEffectRequest {
            id,
            binding,
            property,
            observation_fence: fence,
            correlation,
            effect,
        })
    }

    pub(crate) fn report_effect_dispatch(
        &mut self,
        request: &NativeEffectRequest,
        outcome: NativeEffectDispatchOutcome,
    ) -> Result<(), NativePlatformError> {
        self.require_active(request.binding)?;
        let key = (request.binding, request.property);
        let pending = self.pending_effect(key, request)?;
        if pending.phase != PendingEffectPhase::Issued {
            return Err(NativePlatformError::EffectDispatchAlreadyRecorded);
        }
        let ordinal = self.next_ingress_ordinal()?;

        if outcome == NativeEffectDispatchOutcome::Dispatched {
            self.pending_effects
                .get_mut(&key)
                .and_then(|lane| lane.iter_mut().find(|pending| pending.id == request.id))
                .ok_or(NativePlatformError::UnknownEffectRequest)?
                .phase = PendingEffectPhase::Dispatched;
        } else {
            self.remove_pending_effect(key, request)?;
        }

        let result = NativeEffectResult {
            request_id: request.id,
            binding: request.binding,
            property: request.property,
            outcome,
            correlation: request.correlation.clone(),
        };
        self.commit_ingress_record(ordinal, None, NativeIngressRecordKind::EffectResult(result));
        Ok(())
    }

    /// Cancels an effect reservation before its owning hosted transaction commits.
    ///
    /// Cancellation is deliberately silent: the application has not published the
    /// semantic state that emitted this request, so reporting a terminal result
    /// would create an observation for an effect that does not exist.
    pub(crate) fn cancel_effect_issue(
        &mut self,
        request: &NativeEffectRequest,
    ) -> Result<(), NativePlatformError> {
        self.require_active(request.binding)?;
        let key = (request.binding, request.property);
        let pending = self.pending_effect(key, request)?;
        if pending.phase != PendingEffectPhase::Issued {
            return Err(NativePlatformError::EffectDispatchAlreadyRecorded);
        }
        self.remove_pending_effect(key, request)?;
        Ok(())
    }

    /// Records that a later operation on the same property made this dispatched request
    /// impossible to acknowledge exactly.
    ///
    /// A backend may serialize several requests for one native property before it can obtain a
    /// fresh authoritative observation. The older request was dispatched, but the following
    /// snapshot can only prove the newest observable state. Preserve that distinction instead of
    /// associating the later observation with an arbitrary request in the lane.
    pub(crate) fn report_effect_acknowledgement_lost(
        &mut self,
        request: &NativeEffectRequest,
    ) -> Result<(), NativePlatformError> {
        self.require_active(request.binding)?;
        let key = (request.binding, request.property);
        let pending = self.pending_effect(key, request)?;
        if pending.phase != PendingEffectPhase::Dispatched {
            return Err(NativePlatformError::EffectNotDispatched);
        }
        let ordinal = self.next_ingress_ordinal()?;
        let pending = self.remove_pending_effect(key, request)?;
        let result = NativeEffectResult {
            request_id: request.id,
            binding: request.binding,
            property: request.property,
            outcome: NativeEffectDispatchOutcome::Indeterminate,
            correlation: pending.correlation,
        };
        self.commit_ingress_record(ordinal, None, NativeIngressRecordKind::EffectResult(result));
        Ok(())
    }

    pub(crate) fn issue_viewport_create(
        &mut self,
        parent: NativeViewportBinding,
        viewport_id: egui::ViewportId,
        correlation: NativeViewportCreateCorrelation,
    ) -> Result<NativeViewportCreateRequestId, NativePlatformError> {
        self.require_active(parent)?;
        if self.active.contains_key(&viewport_id) {
            return Err(NativePlatformError::ViewportAlreadyRegistered);
        }
        if self.pending_viewport_creates.contains_key(&viewport_id) {
            return Err(NativePlatformError::ViewportCreateLaneBusy);
        }

        self.next_viewport_create_id = self
            .next_viewport_create_id
            .checked_add(1)
            .ok_or(NativePlatformError::CounterExhausted)?;
        let id = NativeViewportCreateRequestId::new(self.next_viewport_create_id);
        self.pending_viewport_creates.insert(
            viewport_id,
            PendingViewportCreate {
                id,
                parent,
                correlation,
                scheduled: false,
            },
        );
        Ok(id)
    }

    pub(crate) fn accept_viewport_create_schedule(
        &mut self,
        request: &NativeViewportCreateRequest,
    ) -> Result<(), NativePlatformError> {
        let pending = self
            .pending_viewport_creates
            .get(&request.viewport_id)
            .ok_or(NativePlatformError::UnknownViewportCreateRequest)?;
        if pending.id != request.id
            || pending.parent != request.parent
            || pending.correlation != request.correlation
        {
            return Err(NativePlatformError::ViewportCreateRequestMismatch);
        }
        self.require_active(request.parent)?;
        self.pending_viewport_creates
            .get_mut(&request.viewport_id)
            .expect("validated viewport creation request must remain pending")
            .scheduled = true;
        Ok(())
    }

    /// Cancels an unscheduled viewport reservation owned by an aborted host cycle.
    ///
    /// No result is emitted because the corresponding application transaction did
    /// not commit and therefore never exposed the logical create request.
    pub(crate) fn cancel_viewport_create_issue(
        &mut self,
        request: &NativeViewportCreateRequest,
    ) -> Result<(), NativePlatformError> {
        let pending = self
            .pending_viewport_creates
            .get(&request.viewport_id)
            .ok_or(NativePlatformError::UnknownViewportCreateRequest)?;
        if pending.id != request.id
            || pending.parent != request.parent
            || pending.correlation != request.correlation
        {
            return Err(NativePlatformError::ViewportCreateRequestMismatch);
        }
        if pending.scheduled {
            return Err(NativePlatformError::ViewportCreateRequestMismatch);
        }
        let quiescence_ordinal = self
            .binding_ingress_can_quiesce_after_create(request.parent, request.viewport_id)
            .then(|| self.next_ingress_ordinal())
            .transpose()?;
        self.pending_viewport_creates.remove(&request.viewport_id);
        if let Some(ordinal) = quiescence_ordinal {
            self.commit_binding_ingress_quiescence(request.parent, ordinal);
        }
        Ok(())
    }

    pub(crate) fn report_viewport_create_terminal(
        &mut self,
        request: &NativeViewportCreateRequest,
        outcome: NativeViewportCreateDispatchOutcome,
    ) -> Result<(), NativePlatformError> {
        debug_assert_ne!(outcome, NativeViewportCreateDispatchOutcome::Materialized);
        let pending = self
            .pending_viewport_creates
            .get(&request.viewport_id)
            .ok_or(NativePlatformError::UnknownViewportCreateRequest)?;
        if pending.id != request.id
            || pending.parent != request.parent
            || pending.correlation != request.correlation
        {
            return Err(NativePlatformError::ViewportCreateRequestMismatch);
        }
        let ordinal = self.next_ingress_ordinal()?;
        let quiescence_ordinal = self
            .binding_ingress_can_quiesce_after_create(request.parent, request.viewport_id)
            .then(|| {
                ordinal
                    .get()
                    .checked_add(1)
                    .map(NativeIngressOrdinal::new)
                    .ok_or(NativePlatformError::CounterExhausted)
            })
            .transpose()?;
        self.pending_viewport_creates.remove(&request.viewport_id);
        self.commit_ingress_record(
            ordinal,
            None,
            NativeIngressRecordKind::ViewportCreateResult(NativeViewportCreateResult {
                request_id: request.id,
                parent: request.parent,
                viewport_id: request.viewport_id,
                outcome,
                correlation: request.correlation.clone(),
            }),
        );
        if let Some(quiescence_ordinal) = quiescence_ordinal {
            self.commit_binding_ingress_quiescence(request.parent, quiescence_ordinal);
        }
        Ok(())
    }

    pub(crate) fn fail_scheduled_viewport_create(
        &mut self,
        viewport_id: egui::ViewportId,
    ) -> Result<bool, NativePlatformError> {
        let Some(pending) = self
            .pending_viewport_creates
            .get(&viewport_id)
            .filter(|pending| pending.scheduled)
            .cloned()
        else {
            return Ok(false);
        };
        let ordinal = self.next_ingress_ordinal()?;
        let quiescence_ordinal = self
            .binding_ingress_can_quiesce_after_create(pending.parent, viewport_id)
            .then(|| {
                ordinal
                    .get()
                    .checked_add(1)
                    .map(NativeIngressOrdinal::new)
                    .ok_or(NativePlatformError::CounterExhausted)
            })
            .transpose()?;
        self.pending_viewport_creates.remove(&viewport_id);
        self.commit_ingress_record(
            ordinal,
            None,
            NativeIngressRecordKind::ViewportCreateResult(NativeViewportCreateResult {
                request_id: pending.id,
                parent: pending.parent,
                viewport_id,
                outcome: NativeViewportCreateDispatchOutcome::Failed,
                correlation: pending.correlation,
            }),
        );
        if let Some(quiescence_ordinal) = quiescence_ordinal {
            self.commit_binding_ingress_quiescence(pending.parent, quiescence_ordinal);
        }
        Ok(true)
    }

    pub(crate) fn observe_property<T>(
        &mut self,
        binding: NativeViewportBinding,
        property: NativeEffectProperty,
        value: NativeAuthority<T>,
        acknowledgement: ObservationAcknowledgement<'_>,
    ) -> Result<NativePropertyObservation<T>, NativePlatformError> {
        self.require_active(binding)?;
        let key = (binding, property);
        self.validate_observation_acknowledgement(key, acknowledgement)?;
        let generation = self.advance_observation_generation(key)?;
        let acknowledgement = match acknowledgement {
            ObservationAcknowledgement::Baseline => NativeEffectAcknowledgement::baseline(),
            ObservationAcknowledgement::Unknown(reason) => {
                NativeEffectAcknowledgement::unknown(reason)
            }
            ObservationAcknowledgement::Applied(request) => {
                let pending = self.remove_pending_effect(key, request)?;
                NativeEffectAcknowledgement::applied(pending.correlation)
            }
        };

        Ok(NativePropertyObservation {
            binding,
            property,
            generation,
            value,
            acknowledgement,
        })
    }

    fn validate_observation_acknowledgement(
        &self,
        key: (NativeViewportBinding, NativeEffectProperty),
        acknowledgement: ObservationAcknowledgement<'_>,
    ) -> Result<(), NativePlatformError> {
        let ObservationAcknowledgement::Applied(request) = acknowledgement else {
            return Ok(());
        };
        let pending = self.pending_effect(key, request)?;
        if pending.phase != PendingEffectPhase::Dispatched {
            return Err(NativePlatformError::EffectNotDispatched);
        }
        Ok(())
    }

    fn pending_effect<'a>(
        &'a self,
        key: (NativeViewportBinding, NativeEffectProperty),
        request: &NativeEffectRequest,
    ) -> Result<&'a PendingEffect, NativePlatformError> {
        if request.binding != key.0 || request.property != key.1 {
            return Err(NativePlatformError::EffectRequestMismatch);
        }
        let lane = self
            .pending_effects
            .get(&key)
            .ok_or(NativePlatformError::UnknownEffectRequest)?;
        let pending = lane
            .iter()
            .find(|pending| pending.id == request.id)
            .ok_or(NativePlatformError::EffectRequestMismatch)?;
        if pending.fence != request.observation_fence || pending.correlation != request.correlation
        {
            return Err(NativePlatformError::EffectRequestMismatch);
        }
        Ok(pending)
    }

    fn remove_pending_effect(
        &mut self,
        key: (NativeViewportBinding, NativeEffectProperty),
        request: &NativeEffectRequest,
    ) -> Result<PendingEffect, NativePlatformError> {
        self.pending_effect(key, request)?;
        let (pending, lane_empty) = {
            let lane = self
                .pending_effects
                .get_mut(&key)
                .ok_or(NativePlatformError::UnknownEffectRequest)?;
            let index = lane
                .iter()
                .position(|pending| pending.id == request.id)
                .ok_or(NativePlatformError::EffectRequestMismatch)?;
            let pending = lane.remove(index);
            (pending, lane.is_empty())
        };
        if lane_empty {
            self.pending_effects.remove(&key);
        }
        Ok(pending)
    }

    pub(crate) fn begin_presentation(
        &mut self,
        binding: NativeViewportBinding,
    ) -> Result<NativePresentationTicket, NativePlatformError> {
        self.require_active(binding)?;
        let serial = self
            .next_presentation_serial
            .checked_add(1)
            .map(NativePresentationSerial::new)
            .ok_or(NativePlatformError::CounterExhausted)?;
        let lane = self.presentation_lanes.entry(binding).or_default();
        lane.outstanding.insert(serial);
        lane.last_started = Some(serial);
        self.next_presentation_serial = serial.get();
        Ok(NativePresentationTicket { binding, serial })
    }

    pub(crate) fn record_presentation_result(
        &mut self,
        ticket: NativePresentationTicket,
        result: egui::PresentationResult,
    ) -> Result<(), NativePlatformError> {
        self.record_presentation_result_with_follow_up(ticket, result, None)
    }

    pub(crate) fn record_presentation_result_with_follow_up(
        &mut self,
        ticket: NativePresentationTicket,
        result: egui::PresentationResult,
        follow_up: Option<crate::HostedPresentationFollowUp>,
    ) -> Result<(), NativePlatformError> {
        let binding = ticket.binding;
        let serial = ticket.serial;
        let lane = self
            .presentation_lanes
            .get(&binding)
            .ok_or(NativePlatformError::UnknownPresentationTicket)?;
        if !lane.outstanding.contains(&serial) {
            return Err(NativePlatformError::UnknownPresentationTicket);
        }

        let viewport_matches = result.viewport_id() == binding.viewport_id;
        let retires_queue = lane.retired && lane.outstanding.len() == 1;
        let binding_ingress_quiescence = if retires_queue {
            self.retired_binding_ingress
                .get(&binding)
                .filter(|state| {
                    state.owner_quiesced && !self.binding_has_pending_viewport_create(binding, None)
                })
                .map(|state| (state.retirement_generation, state.last_started_presentation))
        } else {
            None
        };
        let result_ordinal = viewport_matches
            .then(|| self.next_ingress_ordinal())
            .transpose()?;
        let quiescence_ordinal = binding_ingress_quiescence
            .is_some()
            .then(|| {
                result_ordinal
                    .map_or(self.ingress_ordinal, NativeIngressOrdinal::get)
                    .checked_add(1)
                    .map(NativeIngressOrdinal::new)
                    .ok_or(NativePlatformError::CounterExhausted)
            })
            .transpose()?;
        let result = viewport_matches
            .then(|| ticket.complete(result))
            .transpose()?;

        let lane = self
            .presentation_lanes
            .get_mut(&binding)
            .ok_or(NativePlatformError::UnknownPresentationTicket)?;
        if !lane.outstanding.remove(&serial) {
            return Err(NativePlatformError::UnknownPresentationTicket);
        }
        if retires_queue {
            self.presentation_lanes.remove(&binding);
        }

        if let (Some(ordinal), Some(result)) = (result_ordinal, result) {
            let should_wake =
                follow_up.is_some_and(|follow_up| follow_up.matches(result.result().outcome()));
            self.commit_ingress_record(
                ordinal,
                None,
                NativeIngressRecordKind::PresentationResult(result),
            );
            if should_wake {
                self.wake.notify_record_available();
            }
        }
        if let (Some(ordinal), Some((retirement_generation, last_started_presentation))) =
            (quiescence_ordinal, binding_ingress_quiescence)
        {
            debug_assert_eq!(
                self.retired_binding_ingress
                    .get(&binding)
                    .map(|state| (state.retirement_generation, state.last_started_presentation,)),
                Some((retirement_generation, last_started_presentation)),
            );
            self.commit_binding_ingress_quiescence(binding, ordinal);
        }

        if viewport_matches {
            Ok(())
        } else {
            Err(NativePlatformError::PresentationViewportMismatch)
        }
    }

    pub(crate) fn window_snapshot(
        &self,
        binding: NativeViewportBinding,
        geometry: NativePropertyObservation<NativeWindowGeometry>,
        presentation: NativePropertyObservation<NativePresentationState>,
        pointer_input: NativePropertyObservation<NativePointerInputState>,
        focus: NativePropertyObservation<bool>,
        close: NativePropertyObservation<NativeCloseState>,
    ) -> Result<NativeWindowSnapshot, NativePlatformError> {
        self.require_active(binding)?;
        let snapshot = NativeWindowSnapshot {
            binding,
            platform_generation: NativePlatformGeneration::new(self.inventory_generation),
            geometry,
            presentation,
            pointer_input,
            focus,
            close,
        };
        self.validate_window_snapshot(&snapshot)?;
        Ok(snapshot)
    }

    pub(crate) fn record_window_snapshot(
        &mut self,
        snapshot: NativeWindowSnapshot,
    ) -> Result<(), NativePlatformError> {
        self.require_active(snapshot.binding)?;
        self.validate_window_snapshot(&snapshot)?;
        self.pending_window_snapshots
            .insert(snapshot.binding, snapshot);
        Ok(())
    }

    pub(crate) fn prepare_host_ingress(
        &mut self,
    ) -> Result<PreparedNativeHostIngress, NativePlatformError> {
        match self.host_ingress_settlement {
            HostIngressSettlementState::Idle => {}
            HostIngressSettlementState::Prepared(_) => {
                return Err(NativePlatformError::HostIngressInFlight);
            }
            HostIngressSettlementState::Poisoned => {
                return Err(NativePlatformError::HostIngressPoisoned);
            }
        }
        let Some(global) = self.pending_global_facts.as_ref() else {
            return Err(NativePlatformError::IncompletePlatformRoster);
        };
        self.validate_global_facts(&global.focused, &global.hovered, &global.capture)?;

        let inventory: Vec<_> = self.active.values().copied().collect();
        if self.pending_window_snapshots.len() != inventory.len()
            || !inventory
                .iter()
                .copied()
                .eq(self.pending_window_snapshots.keys().copied())
        {
            return Err(NativePlatformError::IncompletePlatformRoster);
        }
        for snapshot in self.pending_window_snapshots.values() {
            self.validate_window_snapshot(snapshot)?;
        }
        let snapshot_generation = self.next_snapshot_generation()?;
        let snapshot_ordinal = self.next_ingress_ordinal()?;

        self.wake.begin_consume();

        let global = self
            .pending_global_facts
            .take()
            .ok_or(NativePlatformError::IncompletePlatformRoster)?;
        let windows = std::mem::take(&mut self.pending_window_snapshots)
            .into_values()
            .collect();
        let platform = NativePlatformSnapshot {
            facts: NativePlatformFacts {
                inventory_generation: NativePlatformGeneration::new(self.inventory_generation),
                snapshot_generation,
                inventory,
                focused: NativeGlobalObservation::new(snapshot_generation, global.focused),
                hovered: NativeGlobalObservation::new(snapshot_generation, global.hovered),
                capture: NativeGlobalObservation::new(snapshot_generation, global.capture),
                capabilities: global.capabilities,
                work_areas: global.work_areas,
            },
            windows,
        };

        self.commit_ingress_record(
            snapshot_ordinal,
            None,
            NativeIngressRecordKind::PlatformSnapshot(snapshot_generation),
        );
        let records = std::mem::take(&mut self.pending_ingress_records);
        let previous_pointer = NativePointerSequence::new(self.pointer_watermark);
        let through_pointer = pointer_watermark(&records, previous_pointer);
        let pointer_journal = NativePointerJournal {
            previous: previous_pointer,
            through: through_pointer,
            edges: derive_pointer_edges(&records),
        };
        let effect_results = derive_effect_results(&records);
        let presentation_results = derive_presentation_results(&records);
        let retirement_tombstones = derive_retirement_tombstones(&records);
        let binding_ingress_quiescences = derive_binding_ingress_quiescences(&records);
        let quiesced_bindings = binding_ingress_quiescences
            .iter()
            .map(|quiesced| quiesced.binding())
            .collect();
        let viewport_create_results = derive_viewport_create_results(&records);
        let ordered = NativeIngressJournal::new(
            NativeIngressOrdinal::new(self.ingress_watermark),
            NativeIngressOrdinal::new(self.ingress_ordinal),
            records,
        );

        let ingress = NativeHostIngress {
            platform,
            ordered,
            pointer_journal,
            effect_results,
            presentation_results,
            retirement_tombstones,
            binding_ingress_quiescences,
            viewport_create_results,
        };
        let settlement = NativeHostIngressSettlementKey(ingress.ordered().through());
        self.host_ingress_settlement =
            HostIngressSettlementState::Prepared(PreparedHostIngressSettlement {
                key: settlement,
                snapshot_generation: snapshot_generation.get(),
                ingress_through: ingress.ordered().through().get(),
                pointer_through: through_pointer.get(),
                quiesced_bindings,
            });
        Ok(PreparedNativeHostIngress {
            ingress,
            settlement,
        })
    }

    pub(crate) fn commit_host_ingress(
        &mut self,
        key: NativeHostIngressSettlementKey,
    ) -> Result<(), NativePlatformError> {
        let (snapshot_generation, ingress_through, pointer_through, quiesced_bindings) = {
            let prepared = self.prepared_host_ingress_settlement(key)?;
            (
                prepared.snapshot_generation,
                prepared.ingress_through,
                prepared.pointer_through,
                prepared.quiesced_bindings.clone(),
            )
        };
        self.snapshot_generation = snapshot_generation;
        self.ingress_watermark = ingress_through;
        self.pointer_watermark = pointer_through;
        self.compact_committed_binding_ingress(&quiesced_bindings);
        self.host_ingress_settlement = HostIngressSettlementState::Idle;
        Ok(())
    }

    pub(crate) fn validate_host_ingress_settlement(
        &self,
        key: NativeHostIngressSettlementKey,
    ) -> Result<(), NativePlatformError> {
        self.prepared_host_ingress_settlement(key).map(|_| ())
    }

    fn prepared_host_ingress_settlement(
        &self,
        key: NativeHostIngressSettlementKey,
    ) -> Result<&PreparedHostIngressSettlement, NativePlatformError> {
        let prepared = match &self.host_ingress_settlement {
            HostIngressSettlementState::Prepared(prepared) => prepared,
            HostIngressSettlementState::Poisoned => {
                return Err(NativePlatformError::HostIngressPoisoned);
            }
            HostIngressSettlementState::Idle => {
                return Err(NativePlatformError::HostIngressSettlementMismatch);
            }
        };
        if prepared.key != key {
            return Err(NativePlatformError::HostIngressSettlementMismatch);
        }
        self.validate_binding_ingress_compaction(&prepared.quiesced_bindings)?;
        Ok(prepared)
    }

    pub(crate) fn abort_host_ingress(
        &mut self,
        key: NativeHostIngressSettlementKey,
    ) -> Result<(), NativePlatformError> {
        match &self.host_ingress_settlement {
            HostIngressSettlementState::Prepared(prepared) if prepared.key == key => {
                self.host_ingress_settlement = HostIngressSettlementState::Poisoned;
                Ok(())
            }
            HostIngressSettlementState::Poisoned => Err(NativePlatformError::HostIngressPoisoned),
            HostIngressSettlementState::Idle | HostIngressSettlementState::Prepared(_) => {
                Err(NativePlatformError::HostIngressSettlementMismatch)
            }
        }
    }

    fn validate_binding_ingress_compaction(
        &self,
        bindings: &BTreeSet<NativeViewportBinding>,
    ) -> Result<(), NativePlatformError> {
        let valid = bindings.iter().all(|binding| {
            !self.active.values().any(|active| active == binding)
                && !self.presentation_lanes.contains_key(binding)
                && !self
                    .pending_effects
                    .keys()
                    .any(|(pending_binding, _)| pending_binding == binding)
                && !self.pending_window_snapshots.contains_key(binding)
                && !self.retired_binding_ingress.contains_key(binding)
                && self.minted.contains(binding)
                && self.tombstones.contains_key(binding)
        });
        if valid {
            Ok(())
        } else {
            Err(NativePlatformError::BindingIngressNotQuiescent)
        }
    }

    fn compact_committed_binding_ingress(&mut self, bindings: &BTreeSet<NativeViewportBinding>) {
        for binding in bindings {
            self.minted.remove(binding);
            self.tombstones.remove(binding);
        }
        self.observation_generations
            .retain(|(observed_binding, _), _| !bindings.contains(observed_binding));
    }

    #[cfg(test)]
    pub(crate) fn freeze_host_ingress(&mut self) -> Result<NativeHostIngress, NativePlatformError> {
        let prepared = self.prepare_host_ingress()?;
        let (ingress, settlement) = prepared.into_parts();
        self.commit_host_ingress(settlement)?;
        Ok(ingress)
    }

    fn validate_pointer_source(
        &self,
        source: NativePointerSource,
    ) -> Result<(), NativePlatformError> {
        if let NativePointerSource::Viewport(binding) = source {
            self.require_active(binding)?;
        }
        Ok(())
    }

    fn validate_global_facts(
        &self,
        focused: &NativeAuthority<NativeFocusedWindow>,
        hovered: &NativeAuthority<NativeHoveredWindow>,
        capture: &NativeAuthority<NativeCaptureOwner>,
    ) -> Result<(), NativePlatformError> {
        if let Some(NativeFocusedWindow::Viewport(binding)) = focused.value() {
            self.require_active(*binding)?;
        }
        if let Some(NativeHoveredWindow::Viewport(binding)) = hovered.value() {
            self.require_active(*binding)?;
        }
        if let Some(NativeCaptureOwner::Viewport(binding)) = capture.value() {
            self.require_active(*binding)?;
        }
        Ok(())
    }

    fn validate_window_snapshot(
        &self,
        snapshot: &NativeWindowSnapshot,
    ) -> Result<(), NativePlatformError> {
        let lanes_match = snapshot.geometry.binding == snapshot.binding
            && snapshot.platform_generation.get() == self.inventory_generation
            && snapshot.geometry.property == NativeEffectProperty::Geometry
            && snapshot.geometry.generation
                == self.current_observation_generation((
                    snapshot.binding,
                    NativeEffectProperty::Geometry,
                ))
            && snapshot.presentation.binding == snapshot.binding
            && snapshot.presentation.property == NativeEffectProperty::Presentation
            && snapshot.presentation.generation
                == self.current_observation_generation((
                    snapshot.binding,
                    NativeEffectProperty::Presentation,
                ))
            && snapshot.pointer_input.binding == snapshot.binding
            && snapshot.pointer_input.property == NativeEffectProperty::PointerInput
            && snapshot.pointer_input.generation
                == self.current_observation_generation((
                    snapshot.binding,
                    NativeEffectProperty::PointerInput,
                ))
            && snapshot.focus.binding == snapshot.binding
            && snapshot.focus.property == NativeEffectProperty::Focus
            && snapshot.focus.generation
                == self.current_observation_generation((
                    snapshot.binding,
                    NativeEffectProperty::Focus,
                ))
            && snapshot.close.binding == snapshot.binding
            && snapshot.close.property == NativeEffectProperty::Close
            && snapshot.close.generation
                == self.current_observation_generation((
                    snapshot.binding,
                    NativeEffectProperty::Close,
                ));
        let geometry_matches = snapshot
            .geometry
            .value
            .value()
            .is_none_or(|geometry| geometry.binding == snapshot.binding);
        if lanes_match && geometry_matches {
            Ok(())
        } else {
            Err(NativePlatformError::WindowSnapshotMismatch)
        }
    }

    fn invalidate_staged_platform_snapshot(&mut self) {
        self.pending_window_snapshots.clear();
        self.pending_global_facts = None;
    }

    fn require_active(&self, binding: NativeViewportBinding) -> Result<(), NativePlatformError> {
        if self.active.get(&binding.viewport_id) == Some(&binding) {
            return Ok(());
        }
        if self.tombstones.contains_key(&binding) {
            return Err(NativePlatformError::RetiredBinding);
        }
        if self.minted.contains(&binding) {
            return Err(NativePlatformError::RetiredBinding);
        }
        Err(NativePlatformError::UnknownBinding)
    }

    fn current_observation_generation(
        &self,
        key: (NativeViewportBinding, NativeEffectProperty),
    ) -> NativeObservationGeneration {
        self.observation_generations
            .get(&key)
            .copied()
            .unwrap_or_else(|| NativeObservationGeneration::new(0))
    }

    fn advance_observation_generation(
        &mut self,
        key: (NativeViewportBinding, NativeEffectProperty),
    ) -> Result<NativeObservationGeneration, NativePlatformError> {
        let next = self
            .current_observation_generation(key)
            .get()
            .checked_add(1)
            .ok_or(NativePlatformError::CounterExhausted)?;
        let next = NativeObservationGeneration::new(next);
        self.observation_generations.insert(key, next);
        Ok(next)
    }

    fn advance_incarnation(&mut self) -> Result<NativeViewportIncarnation, NativePlatformError> {
        self.next_incarnation = self
            .next_incarnation
            .checked_add(1)
            .ok_or(NativePlatformError::CounterExhausted)?;
        Ok(NativeViewportIncarnation::new(self.next_incarnation))
    }

    fn advance_inventory_generation(
        &mut self,
    ) -> Result<NativePlatformGeneration, NativePlatformError> {
        let generation = self.next_inventory_generation()?;
        self.inventory_generation = generation.get();
        Ok(generation)
    }

    fn next_inventory_generation(&self) -> Result<NativePlatformGeneration, NativePlatformError> {
        self.inventory_generation
            .checked_add(1)
            .map(NativePlatformGeneration::new)
            .ok_or(NativePlatformError::CounterExhausted)
    }

    fn next_ingress_ordinal(&self) -> Result<NativeIngressOrdinal, NativePlatformError> {
        self.ingress_ordinal
            .checked_add(1)
            .map(NativeIngressOrdinal::new)
            .ok_or(NativePlatformError::CounterExhausted)
    }

    fn commit_ingress_record(
        &mut self,
        ordinal: NativeIngressOrdinal,
        backend_event_sequence: Option<egui::BackendEventSequence>,
        kind: NativeIngressRecordKind,
    ) {
        debug_assert_eq!(
            ordinal.get(),
            self.ingress_ordinal + 1,
            "native ingress records must commit the preflighted next ordinal"
        );
        self.ingress_ordinal = ordinal.get();
        let should_wake = !matches!(
            &kind,
            NativeIngressRecordKind::PlatformSnapshot(_)
                | NativeIngressRecordKind::PresentationResult(_)
        );
        self.pending_ingress_records.push(NativeIngressRecord::new(
            ordinal,
            backend_event_sequence,
            kind,
        ));
        if should_wake {
            self.wake.notify_record_available();
        }
    }

    fn next_snapshot_generation(
        &self,
    ) -> Result<NativePlatformSnapshotGeneration, NativePlatformError> {
        self.snapshot_generation
            .checked_add(1)
            .map(NativePlatformSnapshotGeneration::new)
            .ok_or(NativePlatformError::CounterExhausted)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::native::platform_provider::{
        NativeAccessibilityAction, NativeAccessibilityEdge, NativeBackendCapability,
        NativeFiniteScrollVector, NativeIngressEvent, NativeKey, NativeKeyEdgeKind,
        NativePointerButton, NativePointerDeviceId, NativePointerId,
        NativePointerStreamCancelReason, NativeScrollCancelReason, NativeScrollDelta,
        NativeScrollEdge, NativeScrollPhase, NativeScrollSequenceToken, NativeUnavailableReason,
        NativeWorkAreaGeneration,
    };

    fn viewport(name: &str) -> egui::ViewportId {
        egui::ViewportId::from_hash_of(name)
    }

    fn unknown_point() -> NativeAuthority<NativePhysicalPoint> {
        NativeAuthority::unknown(NativeUnavailableReason::Unsupported)
    }

    fn unknown_hover() -> NativeAuthority<NativeHoveredWindow> {
        NativeAuthority::unknown(NativeUnavailableReason::Unsupported)
    }

    fn unknown_capture() -> NativeAuthority<NativeCaptureOwner> {
        NativeAuthority::unknown(NativeUnavailableReason::Unsupported)
    }

    fn native_delivery(
        binding: NativeViewportBinding,
    ) -> NativeAuthority<NativePointerDeliveryOwner> {
        NativeAuthority::known(NativePointerDeliveryOwner::Viewport(binding))
    }

    fn pointer(device: u64, pointer: u64) -> NativePointerIdentity {
        NativePointerIdentity::new(
            NativePointerDeviceId::new(device),
            NativePointerId::new(pointer),
        )
    }

    fn record_unknown_platform_facts(coordinator: &mut NativePlatformCoordinator) {
        coordinator
            .record_platform_facts(
                NativeAuthority::unknown(NativeUnavailableReason::Unsupported),
                unknown_hover(),
                unknown_capture(),
            )
            .unwrap();
    }

    fn complete_window_snapshot(
        coordinator: &mut NativePlatformCoordinator,
        binding: NativeViewportBinding,
    ) -> NativeWindowSnapshot {
        let geometry = coordinator
            .window_geometry(
                binding,
                NativeAuthority::known(NativePhysicalRect::new(
                    NativePhysicalPoint::new(10, 20),
                    NativePhysicalPoint::new(810, 620),
                )),
                NativeAuthority::known(NativePhysicalRect::new(
                    NativePhysicalPoint::new(5, 10),
                    NativePhysicalPoint::new(815, 650),
                )),
                NativeAuthority::known(2.0),
                NativeAuthority::known(2.0),
                NativeAuthority::unknown(NativeUnavailableReason::Unsupported),
            )
            .unwrap();
        let geometry = coordinator
            .observe_property(
                binding,
                NativeEffectProperty::Geometry,
                NativeAuthority::known(geometry),
                ObservationAcknowledgement::Baseline,
            )
            .unwrap();
        let presentation = coordinator
            .observe_property(
                binding,
                NativeEffectProperty::Presentation,
                NativeAuthority::known(NativePresentationState::Visible),
                ObservationAcknowledgement::Baseline,
            )
            .unwrap();
        let pointer_input = coordinator
            .observe_property(
                binding,
                NativeEffectProperty::PointerInput,
                NativeAuthority::known(NativePointerInputState::ReceivesInput),
                ObservationAcknowledgement::Baseline,
            )
            .unwrap();
        let focus = coordinator
            .observe_property(
                binding,
                NativeEffectProperty::Focus,
                NativeAuthority::known(false),
                ObservationAcknowledgement::Baseline,
            )
            .unwrap();
        let close = coordinator
            .observe_property(
                binding,
                NativeEffectProperty::Close,
                NativeAuthority::known(NativeCloseState::LiveClear),
                ObservationAcknowledgement::Baseline,
            )
            .unwrap();
        coordinator
            .window_snapshot(binding, geometry, presentation, pointer_input, focus, close)
            .unwrap()
    }

    fn record_complete_window_snapshot(
        coordinator: &mut NativePlatformCoordinator,
        binding: NativeViewportBinding,
    ) {
        let snapshot = complete_window_snapshot(coordinator, binding);
        coordinator.record_window_snapshot(snapshot).unwrap();
    }

    fn freeze_single_binding(
        coordinator: &mut NativePlatformCoordinator,
        binding: NativeViewportBinding,
    ) -> NativeHostIngress {
        record_complete_window_snapshot(coordinator, binding);
        record_unknown_platform_facts(coordinator);
        coordinator.freeze_host_ingress().unwrap()
    }

    fn freeze_empty_roster(coordinator: &mut NativePlatformCoordinator) -> NativeHostIngress {
        record_unknown_platform_facts(coordinator);
        coordinator.freeze_host_ingress().unwrap()
    }

    fn confirm_binding_ingress_quiescence(
        coordinator: &mut NativePlatformCoordinator,
        binding: NativeViewportBinding,
    ) {
        coordinator
            .confirm_binding_ingress_quiescence(
                NativeBindingIngressOwnerQuiescence::for_coordinator_test(binding),
            )
            .unwrap();
    }

    #[test]
    fn backend_capabilities_are_frozen_with_the_atomic_platform_snapshot() {
        let mut coordinator = NativePlatformCoordinator::default();
        let binding = coordinator
            .register_viewport(viewport("capability-root"))
            .unwrap();
        record_complete_window_snapshot(&mut coordinator, binding);
        let capabilities = NativeBackendCapabilities::new(
            NativeBackendCapability::Supported,
            NativeBackendCapability::Unsupported,
            NativeBackendCapability::Unsupported,
        );
        coordinator
            .record_platform_facts_with_capabilities(
                NativeAuthority::unknown(NativeUnavailableReason::Unsupported),
                unknown_hover(),
                unknown_capture(),
                capabilities,
                NativeWorkAreaRosterObservation::new(
                    NativeWorkAreaGeneration::new(1),
                    NativeAuthority::unknown(NativeUnavailableReason::Unsupported),
                ),
            )
            .unwrap();

        let ingress = coordinator.freeze_host_ingress().unwrap();

        assert_eq!(ingress.platform().capabilities(), capabilities);
    }

    #[test]
    fn prepared_ingress_advances_committed_watermarks_only_at_settlement() {
        let mut coordinator = NativePlatformCoordinator::default();
        let binding = coordinator.register_viewport(viewport("prepared")).unwrap();
        let pointer_sequence = coordinator
            .record_pointer_edge(
                NativePointerSource::Viewport(binding),
                pointer(1, 1),
                NativePointerEdgeKind::Moved,
                NativeAuthority::known(NativePhysicalPoint::new(10, 20)),
                unknown_hover(),
                unknown_capture(),
            )
            .unwrap();
        record_complete_window_snapshot(&mut coordinator, binding);
        record_unknown_platform_facts(&mut coordinator);

        let prepared = coordinator.prepare_host_ingress().unwrap();
        let (ingress, settlement) = prepared.into_parts();

        assert_eq!(coordinator.snapshot_generation, 0);
        assert_eq!(coordinator.ingress_watermark, 0);
        assert_eq!(coordinator.pointer_watermark, 0);
        assert_eq!(ingress.pointer_journal().through(), pointer_sequence);

        coordinator.commit_host_ingress(settlement).unwrap();

        assert_eq!(
            coordinator.snapshot_generation,
            ingress.platform().snapshot_generation().get()
        );
        assert_eq!(
            coordinator.ingress_watermark,
            ingress.ordered().through().get()
        );
        assert_eq!(coordinator.pointer_watermark, pointer_sequence.get());
    }

    #[test]
    fn ingress_commit_does_not_consume_records_that_arrived_after_prepare() {
        let mut coordinator = NativePlatformCoordinator::default();
        let binding = coordinator
            .register_viewport(viewport("post-prepare"))
            .unwrap();
        record_complete_window_snapshot(&mut coordinator, binding);
        record_unknown_platform_facts(&mut coordinator);
        let first = coordinator.prepare_host_ingress().unwrap();
        let (first_ingress, settlement) = first.into_parts();

        let later_pointer = coordinator
            .record_pointer_edge(
                NativePointerSource::Viewport(binding),
                pointer(1, 1),
                NativePointerEdgeKind::Moved,
                NativeAuthority::known(NativePhysicalPoint::new(30, 40)),
                unknown_hover(),
                unknown_capture(),
            )
            .unwrap();
        coordinator.commit_host_ingress(settlement).unwrap();
        record_complete_window_snapshot(&mut coordinator, binding);
        record_unknown_platform_facts(&mut coordinator);

        let second = coordinator.prepare_host_ingress().unwrap();
        let (second_ingress, second_settlement) = second.into_parts();

        assert_eq!(
            second_ingress.ordered().previous(),
            first_ingress.ordered().through()
        );
        assert_eq!(second_ingress.pointer_journal().through(), later_pointer);
        coordinator.commit_host_ingress(second_settlement).unwrap();
    }

    #[test]
    fn aborted_ingress_poison_prevents_a_successor_batch() {
        let mut coordinator = NativePlatformCoordinator::default();
        record_unknown_platform_facts(&mut coordinator);
        let prepared = coordinator.prepare_host_ingress().unwrap();
        let (_, settlement) = prepared.into_parts();

        coordinator.abort_host_ingress(settlement).unwrap();

        assert_eq!(
            coordinator.prepare_host_ingress().unwrap_err(),
            NativePlatformError::HostIngressPoisoned
        );
    }

    #[test]
    fn retirement_tombstone_prevents_viewport_incarnation_aba() {
        let mut coordinator = NativePlatformCoordinator::default();
        let viewport_id = viewport("reused");
        let first = coordinator.register_viewport(viewport_id).unwrap();
        let retirement_sequence = egui::BackendEventSequence::new(77);
        let tombstone = coordinator
            .retire_viewport_for_backend(first, retirement_sequence)
            .unwrap();
        let second = coordinator.register_viewport(viewport_id).unwrap();

        assert_ne!(first, second);
        assert_eq!(tombstone.binding(), first);
        assert_eq!(coordinator.retirement_tombstone(first), Some(tombstone));
        assert_eq!(coordinator.active_binding(viewport_id), Some(second));
        assert!(matches!(
            coordinator.begin_presentation(first),
            Err(NativePlatformError::RetiredBinding)
        ));
    }

    #[test]
    fn pointer_journal_is_contiguous_across_drains() {
        let mut coordinator = NativePlatformCoordinator::default();
        let binding = coordinator.register_viewport(viewport("pointer")).unwrap();

        let first = coordinator
            .record_pointer_edge(
                NativePointerSource::Viewport(binding),
                pointer(1, 1),
                NativePointerEdgeKind::Moved,
                NativeAuthority::known(NativePhysicalPoint::new(10, 20)),
                unknown_hover(),
                unknown_capture(),
            )
            .unwrap();
        let second = coordinator
            .record_pointer_edge(
                NativePointerSource::Viewport(binding),
                pointer(1, 1),
                NativePointerEdgeKind::ButtonPressed(NativePointerButton::Primary),
                unknown_point(),
                unknown_hover(),
                unknown_capture(),
            )
            .unwrap();
        let first_ingress = freeze_single_binding(&mut coordinator, binding);
        let first_batch = first_ingress.pointer_journal();

        assert_eq!(first.get(), 1);
        assert_eq!(second.get(), 2);
        assert_eq!(first_batch.previous().get(), 0);
        assert_eq!(first_batch.through(), second);
        assert_eq!(first_batch.edges().len(), 2);

        let empty_ingress = freeze_single_binding(&mut coordinator, binding);
        let empty_batch = empty_ingress.pointer_journal();
        assert_eq!(empty_batch.previous(), second);
        assert_eq!(empty_batch.through(), second);
        assert!(empty_batch.edges().is_empty());

        let third = coordinator
            .record_pointer_edge(
                NativePointerSource::Viewport(binding),
                pointer(1, 1),
                NativePointerEdgeKind::ButtonReleased(NativePointerButton::Primary),
                unknown_point(),
                unknown_hover(),
                unknown_capture(),
            )
            .unwrap();
        let second_ingress = freeze_single_binding(&mut coordinator, binding);
        let second_batch = second_ingress.pointer_journal();
        assert_eq!(third.get(), 3);
        assert_eq!(second_batch.previous(), second);
        assert_eq!(second_batch.through(), third);
        assert_eq!(second_batch.edges()[0].sequence(), third);
    }

    #[test]
    fn pointer_edge_retains_its_event_time_coordinate_capture() {
        let mut coordinator = NativePlatformCoordinator::default();
        let binding = coordinator
            .register_viewport(viewport("event-time-coordinate"))
            .unwrap();
        let capture = NativePointerCoordinateCapture::new(
            binding,
            NativePhysicalPoint::new(100, 200),
            2.0,
            2.5,
        );

        coordinator
            .record_pointer_edge_for_backend(
                egui::BackendEventSequence::new(1),
                NativePointerEdgeFacts::new(
                    NativePointerSource::Viewport(binding),
                    native_delivery(binding),
                    pointer(1, 1),
                    NativePointerEdgeKind::Moved,
                    NativeAuthority::known(NativePhysicalPoint::new(120, 240)),
                    NativeAuthority::known(NativeHoveredWindow::Viewport(binding)),
                    unknown_capture(),
                )
                .with_hovered_coordinates(NativeAuthority::known(capture)),
            )
            .unwrap();

        let ingress = freeze_single_binding(&mut coordinator, binding);
        assert_eq!(
            ingress.pointer_journal().edges()[0]
                .hovered_coordinates()
                .value(),
            Some(&capture)
        );
        assert_eq!(capture.native_scale_factor(), 2.0);
        assert_eq!(capture.presentation_scale_factor(), 2.5);
    }

    #[test]
    fn terminal_pointer_edges_survive_the_frozen_ingress_boundary() {
        let mut coordinator = NativePlatformCoordinator::default();
        let binding = coordinator
            .register_viewport(viewport("terminal-touch"))
            .unwrap();

        let terminal_kinds = [
            NativePointerEdgeKind::ContactEnded(NativePointerButton::Primary),
            NativePointerEdgeKind::StreamEnded,
            NativePointerEdgeKind::StreamCancelled(NativePointerStreamCancelReason::DeviceRemoved),
        ];
        for (index, kind) in terminal_kinds.into_iter().enumerate() {
            coordinator
                .record_pointer_edge_for_backend(
                    egui::BackendEventSequence::new(u128::try_from(index + 1).unwrap()),
                    NativePointerEdgeFacts::new(
                        NativePointerSource::Viewport(binding),
                        native_delivery(binding),
                        pointer(4, u64::try_from(index + 9).unwrap()),
                        kind,
                        unknown_point(),
                        unknown_hover(),
                        unknown_capture(),
                    ),
                )
                .unwrap();
        }

        let ingress = freeze_single_binding(&mut coordinator, binding);
        assert!(
            ingress
                .pointer_journal()
                .edges()
                .iter()
                .map(NativePointerEdge::kind)
                .eq(terminal_kinds)
        );
        assert!(
            ingress
                .pointer_journal()
                .edges()
                .iter()
                .all(NativePointerEdge::ends_stream)
        );
        assert!(!NativePointerEdgeKind::Moved.ends_stream());
        assert!(!NativePointerEdgeKind::ButtonReleased(NativePointerButton::Primary).ends_stream());
    }

    #[test]
    fn native_ingress_preserves_release_before_effect_and_presentation() {
        let mut coordinator = NativePlatformCoordinator::default();
        let binding = coordinator
            .register_viewport(viewport("ordered-release"))
            .unwrap();
        let backend_sequence = egui::BackendEventSequence::new(41);
        let release = coordinator
            .record_pointer_edge_for_backend(
                backend_sequence,
                NativePointerEdgeFacts::new(
                    NativePointerSource::Viewport(binding),
                    native_delivery(binding),
                    pointer(1, 1),
                    NativePointerEdgeKind::ButtonReleased(NativePointerButton::Primary),
                    unknown_point(),
                    unknown_hover(),
                    unknown_capture(),
                ),
            )
            .unwrap();
        let request = coordinator
            .issue_effect(
                binding,
                NativeWindowEffect::RequestFocus,
                NativeEffectCorrelation::new(egui::UserData::new("ordered-focus")),
            )
            .unwrap();
        coordinator
            .report_effect_dispatch(&request, NativeEffectDispatchOutcome::Dispatched)
            .unwrap();
        let ticket = coordinator.begin_presentation(binding).unwrap();
        coordinator
            .record_presentation_result(
                ticket,
                egui::PresentationResult::new(
                    binding.viewport_id(),
                    egui::UserData::new("ordered-frame"),
                    egui::PaintOutcome::SubmittedToSwapchain,
                    None,
                ),
            )
            .unwrap();

        let ingress = freeze_single_binding(&mut coordinator, binding);
        let records = ingress.ordered().records();

        assert_eq!(ingress.ordered().previous().get(), 0);
        assert_eq!(ingress.ordered().through().get(), 4);
        assert!(matches!(
            records[0].event(),
            NativeIngressEvent::PointerEdge(edge) if edge.sequence() == release
        ));
        assert_eq!(records[0].backend_event_sequence(), Some(backend_sequence));
        assert!(matches!(
            records[1].event(),
            NativeIngressEvent::EffectResult(result) if result.request_id() == request.id()
        ));
        assert!(matches!(
            records[2].event(),
            NativeIngressEvent::PresentationResult(result)
                if result.binding() == binding
        ));
        assert!(matches!(
            records[3].event(),
            NativeIngressEvent::PlatformSnapshot(generation)
                if generation == ingress.platform().snapshot_generation()
        ));
        assert!(
            records
                .iter()
                .map(|record| record.ordinal().get())
                .eq([1, 2, 3, 4])
        );
        assert_eq!(ingress.pointer_journal().edges()[0].sequence(), release);
        assert_eq!(ingress.effect_results()[0].request_id(), request.id());
        assert_eq!(ingress.presentation_results()[0].binding(), binding);
    }

    #[test]
    fn native_ingress_preserves_pointer_before_close_request() {
        let mut coordinator = NativePlatformCoordinator::default();
        let binding = coordinator
            .register_viewport(viewport("ordered-close"))
            .unwrap();
        let pointer_sequence = egui::BackendEventSequence::new(41);
        let close_sequence = egui::BackendEventSequence::new(42);
        coordinator
            .record_pointer_edge_for_backend(
                pointer_sequence,
                NativePointerEdgeFacts::new(
                    NativePointerSource::Viewport(binding),
                    native_delivery(binding),
                    pointer(1, 1),
                    NativePointerEdgeKind::ButtonReleased(NativePointerButton::Primary),
                    unknown_point(),
                    unknown_hover(),
                    unknown_capture(),
                ),
            )
            .unwrap();
        let close = coordinator
            .record_close_observation_for_backend(
                close_sequence,
                binding,
                NativeCloseState::LiveRequested,
                ObservationAcknowledgement::Baseline,
            )
            .unwrap();

        let ingress = freeze_single_binding(&mut coordinator, binding);
        let records = ingress.ordered().records();

        assert!(matches!(
            records[0].event(),
            NativeIngressEvent::PointerEdge(_)
        ));
        assert_eq!(records[0].backend_event_sequence(), Some(pointer_sequence));
        assert!(matches!(
            records[1].event(),
            NativeIngressEvent::CloseObservation(observation)
                if observation == &close
                    && observation.value().value() == Some(&NativeCloseState::LiveRequested)
        ));
        assert_eq!(records[1].backend_event_sequence(), Some(close_sequence));
        assert!(matches!(
            records[2].event(),
            NativeIngressEvent::PlatformSnapshot(_)
        ));
    }

    #[test]
    fn retired_binding_cannot_publish_close_for_replacement_incarnation() {
        let mut coordinator = NativePlatformCoordinator::default();
        let viewport = viewport("close-aba");
        let retired = coordinator.register_viewport(viewport).unwrap();
        coordinator.retire_viewport(retired).unwrap();
        let replacement = coordinator.register_viewport(viewport).unwrap();

        assert_ne!(retired, replacement);
        assert!(matches!(
            coordinator.record_close_observation_for_backend(
                egui::BackendEventSequence::new(7),
                retired,
                NativeCloseState::LiveRequested,
                ObservationAcknowledgement::Baseline,
            ),
            Err(NativePlatformError::RetiredBinding)
        ));
    }

    #[test]
    fn cancel_close_dispatch_precedes_its_exact_live_clear_observation() {
        let mut coordinator = NativePlatformCoordinator::default();
        let binding = coordinator
            .register_viewport(viewport("cancel-close-order"))
            .unwrap();
        let request = coordinator
            .issue_effect(
                binding,
                NativeWindowEffect::CancelClose,
                NativeEffectCorrelation::new(egui::UserData::new("cancel-close")),
            )
            .unwrap();
        coordinator
            .report_effect_dispatch(&request, NativeEffectDispatchOutcome::Dispatched)
            .unwrap();
        let clear = coordinator
            .record_close_observation_after_effect(binding, NativeCloseState::LiveClear, &request)
            .unwrap();

        let ingress = freeze_single_binding(&mut coordinator, binding);
        let records = ingress.ordered().records();
        assert!(matches!(
            records[0].event(),
            NativeIngressEvent::EffectResult(result) if result.request_id() == request.id()
        ));
        assert!(matches!(
            records[1].event(),
            NativeIngressEvent::CloseObservation(observation)
                if observation == &clear
                    && observation.value().value() == Some(&NativeCloseState::LiveClear)
                    && observation.acknowledgement().correlation()
                        == Some(request.correlation())
        ));
        assert!(matches!(
            records[2].event(),
            NativeIngressEvent::PlatformSnapshot(_)
        ));
    }

    #[test]
    fn native_ingress_preserves_release_a_before_press_b() {
        let mut coordinator = NativePlatformCoordinator::default();
        let first = coordinator
            .register_viewport(viewport("release-a"))
            .unwrap();
        let second = coordinator.register_viewport(viewport("press-b")).unwrap();
        let identity = pointer(8, 1);
        let release_sequence = egui::BackendEventSequence::new(50);
        let press_sequence = egui::BackendEventSequence::new(51);

        coordinator
            .record_pointer_edge_for_backend(
                release_sequence,
                NativePointerEdgeFacts::new(
                    NativePointerSource::Viewport(first),
                    native_delivery(first),
                    identity,
                    NativePointerEdgeKind::ButtonReleased(NativePointerButton::Primary),
                    unknown_point(),
                    unknown_hover(),
                    unknown_capture(),
                ),
            )
            .unwrap();
        coordinator
            .record_pointer_edge_for_backend(
                press_sequence,
                NativePointerEdgeFacts::new(
                    NativePointerSource::Viewport(second),
                    native_delivery(second),
                    identity,
                    NativePointerEdgeKind::ButtonPressed(NativePointerButton::Primary),
                    unknown_point(),
                    unknown_hover(),
                    unknown_capture(),
                ),
            )
            .unwrap();
        record_complete_window_snapshot(&mut coordinator, first);
        record_complete_window_snapshot(&mut coordinator, second);
        record_unknown_platform_facts(&mut coordinator);

        let ingress = coordinator.freeze_host_ingress().unwrap();
        let records = ingress.ordered().records();
        assert!(matches!(
            records[0].event(),
            NativeIngressEvent::PointerEdge(edge)
                if edge.source() == NativePointerSource::Viewport(first)
                    && edge.kind()
                        == NativePointerEdgeKind::ButtonReleased(NativePointerButton::Primary)
        ));
        assert_eq!(records[0].backend_event_sequence(), Some(release_sequence));
        assert!(matches!(
            records[1].event(),
            NativeIngressEvent::PointerEdge(edge)
                if edge.source() == NativePointerSource::Viewport(second)
                    && edge.kind()
                        == NativePointerEdgeKind::ButtonPressed(NativePointerButton::Primary)
        ));
        assert_eq!(records[1].backend_event_sequence(), Some(press_sequence));
    }

    #[test]
    fn native_ingress_preserves_pointer_release_before_escape_press() {
        let mut coordinator = NativePlatformCoordinator::default();
        let binding = coordinator
            .register_viewport(viewport("release-before-escape"))
            .unwrap();
        let release_sequence = egui::BackendEventSequence::new(70);
        let escape_sequence = egui::BackendEventSequence::new(71);

        coordinator
            .record_pointer_edge_for_backend(
                release_sequence,
                NativePointerEdgeFacts::new(
                    NativePointerSource::Viewport(binding),
                    native_delivery(binding),
                    pointer(10, 1),
                    NativePointerEdgeKind::ButtonReleased(NativePointerButton::Primary),
                    unknown_point(),
                    unknown_hover(),
                    unknown_capture(),
                ),
            )
            .unwrap();
        coordinator
            .record_key_edge_for_backend(
                escape_sequence,
                NativeKeyEdge::new(binding, NativeKey::Escape, NativeKeyEdgeKind::Pressed),
            )
            .unwrap();

        let ingress = freeze_single_binding(&mut coordinator, binding);
        let records = ingress.ordered().records();
        assert!(matches!(
            records[0].event(),
            NativeIngressEvent::PointerEdge(edge)
                if edge.kind()
                    == NativePointerEdgeKind::ButtonReleased(NativePointerButton::Primary)
        ));
        assert_eq!(records[0].backend_event_sequence(), Some(release_sequence));
        assert!(matches!(
            records[1].event(),
            NativeIngressEvent::KeyEdge(edge)
                if edge.binding() == binding
                    && edge.key() == NativeKey::Escape
                    && edge.kind() == NativeKeyEdgeKind::Pressed
        ));
        assert_eq!(records[1].backend_event_sequence(), Some(escape_sequence));
        assert!(matches!(
            records[2].event(),
            NativeIngressEvent::PlatformSnapshot(_)
        ));
    }

    #[test]
    fn native_ingress_keeps_identical_scroll_shapes_in_backend_order() {
        let mut coordinator = NativePlatformCoordinator::default();
        let binding = coordinator
            .register_viewport(viewport("ordered-scroll"))
            .unwrap();
        let first_sequence = egui::BackendEventSequence::new(80);
        let key_sequence = egui::BackendEventSequence::new(81);
        let second_sequence = egui::BackendEventSequence::new(82);
        let scroll = NativeScrollEdge::new(
            NativePointerDeviceId::new(12),
            None,
            NativeScrollPhase::Discrete,
            Some(NativeScrollDelta::Lines(
                NativeFiniteScrollVector::new(0.0, 1.0).unwrap(),
            )),
            NativeAuthority::unknown(NativeUnavailableReason::NotObserved),
            NativeAuthority::unknown(NativeUnavailableReason::NotObserved),
        )
        .unwrap();

        for sequence in [first_sequence, second_sequence] {
            if sequence == second_sequence {
                coordinator
                    .record_key_edge_for_backend(
                        key_sequence,
                        NativeKeyEdge::new(binding, NativeKey::Enter, NativeKeyEdgeKind::Repeated),
                    )
                    .unwrap();
            }
            coordinator
                .record_pointer_edge_for_backend(
                    sequence,
                    NativePointerEdgeFacts::new(
                        NativePointerSource::Viewport(binding),
                        native_delivery(binding),
                        pointer(12, 1),
                        NativePointerEdgeKind::Scrolled(scroll),
                        unknown_point(),
                        unknown_hover(),
                        unknown_capture(),
                    ),
                )
                .unwrap();
        }

        let ingress = freeze_single_binding(&mut coordinator, binding);
        let records = ingress.ordered().records();
        assert_eq!(records[0].backend_event_sequence(), Some(first_sequence));
        assert!(matches!(
            records[0].event(),
            NativeIngressEvent::PointerEdge(edge)
                if edge.kind() == NativePointerEdgeKind::Scrolled(scroll)
        ));
        assert_eq!(records[1].backend_event_sequence(), Some(key_sequence));
        assert!(matches!(records[1].event(), NativeIngressEvent::KeyEdge(_)));
        assert_eq!(records[2].backend_event_sequence(), Some(second_sequence));
        assert!(matches!(
            records[2].event(),
            NativeIngressEvent::PointerEdge(edge)
                if edge.kind() == NativePointerEdgeKind::Scrolled(scroll)
        ));
        assert!(records[0].ordinal() < records[1].ordinal());
        assert!(records[1].ordinal() < records[2].ordinal());
    }

    #[test]
    fn accessibility_actions_share_the_pointer_and_keyboard_total_order() {
        let mut coordinator = NativePlatformCoordinator::default();
        let binding = coordinator
            .register_viewport(viewport("accessibility-order"))
            .unwrap();
        coordinator
            .record_pointer_edge_for_backend(
                egui::BackendEventSequence::new(74),
                NativePointerEdgeFacts::new(
                    NativePointerSource::Viewport(binding),
                    native_delivery(binding),
                    pointer(11, 1),
                    NativePointerEdgeKind::ButtonReleased(NativePointerButton::Primary),
                    unknown_point(),
                    unknown_hover(),
                    unknown_capture(),
                ),
            )
            .unwrap();
        let request = egui::accesskit::ActionRequest {
            action: egui::accesskit::Action::Click,
            target_tree: egui::accesskit::TreeId::ROOT,
            target_node: egui::accesskit::NodeId(47),
            data: None,
        };
        let accessibility =
            NativeAccessibilityEdge::from_accesskit_request(binding, &request, None).unwrap();
        coordinator
            .record_accessibility_edge_for_backend(
                egui::BackendEventSequence::new(75),
                accessibility,
            )
            .unwrap();
        coordinator
            .record_key_edge_for_backend(
                egui::BackendEventSequence::new(76),
                NativeKeyEdge::new(binding, NativeKey::Escape, NativeKeyEdgeKind::Pressed),
            )
            .unwrap();

        let ingress = freeze_single_binding(&mut coordinator, binding);
        let records = ingress.ordered().records();
        assert!(matches!(
            records[0].event(),
            NativeIngressEvent::PointerEdge(_)
        ));
        assert!(matches!(
            records[1].event(),
            NativeIngressEvent::AccessibilityEdge(edge)
                if edge.binding() == binding
                    && edge.target() == egui::accesskit::NodeId(47)
                    && edge.action() == NativeAccessibilityAction::Click
        ));
        assert!(matches!(records[2].event(), NativeIngressEvent::KeyEdge(_)));
        assert_eq!(
            records[..3]
                .iter()
                .map(|record| record.backend_event_sequence().unwrap().get())
                .collect::<Vec<_>>(),
            [74, 75, 76]
        );
    }

    #[test]
    fn escape_repeat_and_release_remain_ordered_physical_edges() {
        let mut coordinator = NativePlatformCoordinator::default();
        let binding = coordinator
            .register_viewport(viewport("escape-edge-kinds"))
            .unwrap();
        for (sequence, kind) in [
            (80, NativeKeyEdgeKind::Pressed),
            (81, NativeKeyEdgeKind::Repeated),
            (82, NativeKeyEdgeKind::Released),
        ] {
            coordinator
                .record_key_edge_for_backend(
                    egui::BackendEventSequence::new(sequence),
                    NativeKeyEdge::new(binding, NativeKey::Escape, kind),
                )
                .unwrap();
        }

        let ingress = freeze_single_binding(&mut coordinator, binding);
        let records = ingress.ordered().records();
        assert_eq!(
            records[..3]
                .iter()
                .map(|record| match record.event() {
                    NativeIngressEvent::KeyEdge(edge) => edge.kind(),
                    other => panic!("expected a key edge, got {other:?}"),
                })
                .collect::<Vec<_>>(),
            [
                NativeKeyEdgeKind::Pressed,
                NativeKeyEdgeKind::Repeated,
                NativeKeyEdgeKind::Released,
            ]
        );
    }

    #[test]
    fn key_edges_retain_their_exact_binding_across_viewport_replacement() {
        let mut coordinator = NativePlatformCoordinator::default();
        let viewport_id = viewport("key-binding-aba");
        let first = coordinator.register_viewport(viewport_id).unwrap();
        coordinator
            .record_key_edge_for_backend(
                egui::BackendEventSequence::new(90),
                NativeKeyEdge::new(first, NativeKey::Escape, NativeKeyEdgeKind::Pressed),
            )
            .unwrap();
        coordinator.retire_viewport(first).unwrap();
        confirm_binding_ingress_quiescence(&mut coordinator, first);
        let second = coordinator.register_viewport(viewport_id).unwrap();

        assert_ne!(first, second);
        assert_eq!(
            coordinator.record_key_edge_for_backend(
                egui::BackendEventSequence::new(91),
                NativeKeyEdge::new(first, NativeKey::Escape, NativeKeyEdgeKind::Released),
            ),
            Err(NativePlatformError::RetiredBinding)
        );
        coordinator
            .record_key_edge_for_backend(
                egui::BackendEventSequence::new(92),
                NativeKeyEdge::new(second, NativeKey::Escape, NativeKeyEdgeKind::Pressed),
            )
            .unwrap();

        let ingress = freeze_single_binding(&mut coordinator, second);
        let records = ingress.ordered().records();
        assert!(matches!(
            records[0].event(),
            NativeIngressEvent::KeyEdge(edge) if edge.binding() == first
        ));
        assert!(matches!(
            records[1].event(),
            NativeIngressEvent::Retirement(tombstone) if tombstone.binding() == first
        ));
        assert!(matches!(
            records[2].event(),
            NativeIngressEvent::BindingIngressQuiesced(quiesced) if quiesced.binding() == first
        ));
        assert!(matches!(
            records[3].event(),
            NativeIngressEvent::KeyEdge(edge) if edge.binding() == second
        ));
        assert_eq!(
            records[3].backend_event_sequence(),
            Some(egui::BackendEventSequence::new(92))
        );
    }

    #[test]
    fn native_ingress_validates_an_affine_envelope_claim() {
        let mut coordinator = NativePlatformCoordinator::default();
        let binding = coordinator
            .register_viewport(egui::ViewportId::ROOT)
            .unwrap();
        let backend_sequence = egui::BackendEventSequence::new(63);
        coordinator
            .record_pointer_edge_for_backend(
                backend_sequence,
                NativePointerEdgeFacts::new(
                    NativePointerSource::Viewport(binding),
                    native_delivery(binding),
                    pointer(9, 1),
                    NativePointerEdgeKind::Moved,
                    unknown_point(),
                    unknown_hover(),
                    unknown_capture(),
                ),
            )
            .unwrap();
        let mut derivation = egui::BackendEventDerivation::known(backend_sequence);
        let mut raw_input = egui::RawInput::default();
        raw_input
            .events
            .push(derivation.envelope(egui::Event::PointerGone));
        let replay_input = raw_input.clone();
        let claim_event = |input: egui::RawInput| {
            let context = egui::Context::default();
            let mut claim = None;
            let _ = context.run_ui(input, |ui| {
                claim = ui.input_mut(|input| input.claim_event_envelope(|_| true));
            });
            claim.expect("the immutable envelope is claimable")
        };

        let ingress = freeze_single_binding(&mut coordinator, binding);
        let cycle = crate::HostedViewportCycle::with_native_ingress([raw_input], ingress)
            .expect("the root input and provider inventory form one exact cycle");
        let replacement = NativeViewportBinding::new(
            egui::ViewportId::ROOT,
            NativeViewportIncarnation::new(
                binding
                    .incarnation()
                    .get()
                    .checked_add(1)
                    .expect("the fixture incarnation advances"),
            ),
        );
        assert!(
            cycle
                .validate_native_event_envelope_claim(
                    replacement,
                    claim_event(replay_input.clone())
                )
                .is_none(),
            "a logical viewport match cannot authorize another native incarnation"
        );
        let receipt = cycle
            .validate_native_event_envelope_claim(binding, claim_event(replay_input.clone()))
            .expect("the provider journal validates its own backend correlation");
        assert_eq!(receipt.raw_event_index(), 0);
        assert_eq!(receipt.derivative_ordinal(), 0);
        assert_eq!(
            receipt.record().backend_event_sequence(),
            Some(backend_sequence)
        );

        assert!(
            cycle
                .validate_native_event_envelope_claim(binding, claim_event(replay_input))
                .is_none(),
            "one native cycle consumes each exact envelope at most once"
        );

        let mut forged = egui::BackendEventDerivation::known(backend_sequence);
        let forged_input = egui::RawInput {
            events: vec![forged.envelope(egui::Event::PointerGone)],
            ..egui::RawInput::default()
        };
        assert!(
            cycle
                .validate_native_event_envelope_claim(binding, claim_event(forged_input))
                .is_none(),
            "a copied public sequence without the cycle envelope identity is not provider authority"
        );
    }

    #[test]
    fn dock_owned_scroll_derivative_is_removed_before_the_egui_pass() {
        let mut coordinator = NativePlatformCoordinator::default();
        let binding = coordinator
            .register_viewport(egui::ViewportId::ROOT)
            .unwrap();
        let backend_sequence = egui::BackendEventSequence::new(64);
        let scroll = NativeScrollEdge::new(
            NativePointerDeviceId::new(9),
            None,
            NativeScrollPhase::Discrete,
            Some(NativeScrollDelta::Lines(
                NativeFiniteScrollVector::new(0.0, 1.0).unwrap(),
            )),
            NativeAuthority::unknown(NativeUnavailableReason::NotObserved),
            NativeAuthority::unknown(NativeUnavailableReason::NotObserved),
        )
        .unwrap();
        let pointer_sequence = coordinator
            .record_pointer_edge_for_backend(
                backend_sequence,
                NativePointerEdgeFacts::new(
                    NativePointerSource::Viewport(binding),
                    native_delivery(binding),
                    pointer(9, 1),
                    NativePointerEdgeKind::Scrolled(scroll),
                    unknown_point(),
                    unknown_hover(),
                    unknown_capture(),
                ),
            )
            .unwrap();
        let mut derivation = egui::BackendEventDerivation::known(backend_sequence);
        let raw_input = egui::RawInput {
            viewport_id: egui::ViewportId::ROOT,
            events: vec![derivation.envelope(egui::Event::MouseWheel {
                unit: egui::MouseWheelUnit::Line,
                delta: egui::vec2(0.0, 1.0),
                phase: egui::TouchPhase::Move,
                modifiers: egui::Modifiers::NONE,
            })],
            ..Default::default()
        };
        let ingress = freeze_single_binding(&mut coordinator, binding);
        let cycle = crate::HostedViewportCycle::with_native_ingress([raw_input], ingress)
            .expect("the wheel derivative and native journal form one exact cycle");

        assert!(cycle.claim_native_scroll_derivative(pointer_sequence));
        assert!(
            !cycle.claim_native_scroll_derivative(pointer_sequence),
            "the exact wheel derivative is affine"
        );
        cycle
            .run(
                [egui::ViewportId::ROOT],
                |_| Ok(()),
                |input| {
                    assert!(
                        input.raw_input().events.is_empty(),
                        "a core-owned wheel derivative must not reach egui WheelState"
                    );
                    Ok(())
                },
                |_| Ok(()),
            )
            .expect("the filtered hosted cycle remains complete");
    }

    #[test]
    fn phaseful_scroll_derivative_may_arrive_on_a_later_callback_viewport() {
        let mut coordinator = NativePlatformCoordinator::default();
        let source_viewport = egui::ViewportId::ROOT;
        let callback_viewport = viewport("scroll-callback");
        let source = coordinator.register_viewport(source_viewport).unwrap();
        let callback = coordinator.register_viewport(callback_viewport).unwrap();
        let backend_sequence = egui::BackendEventSequence::new(641);
        let scroll = NativeScrollEdge::new(
            NativePointerDeviceId::new(9),
            Some(NativeScrollSequenceToken::new(1)),
            NativeScrollPhase::Update,
            Some(NativeScrollDelta::Lines(
                NativeFiniteScrollVector::new(0.0, 1.0).unwrap(),
            )),
            NativeAuthority::unknown(NativeUnavailableReason::NotObserved),
            NativeAuthority::unknown(NativeUnavailableReason::NotObserved),
        )
        .unwrap();
        let pointer_sequence = coordinator
            .record_pointer_edge_for_backend(
                backend_sequence,
                NativePointerEdgeFacts::new(
                    NativePointerSource::Viewport(source),
                    native_delivery(callback),
                    pointer(9, 1),
                    NativePointerEdgeKind::Scrolled(scroll),
                    unknown_point(),
                    unknown_hover(),
                    unknown_capture(),
                ),
            )
            .unwrap();
        let source_input = egui::RawInput {
            viewport_id: source_viewport,
            ..Default::default()
        };
        let mut derivation = egui::BackendEventDerivation::known(backend_sequence);
        let callback_input = egui::RawInput {
            viewport_id: callback_viewport,
            events: vec![derivation.envelope(egui::Event::MouseWheel {
                unit: egui::MouseWheelUnit::Line,
                delta: egui::vec2(0.0, 1.0),
                phase: egui::TouchPhase::Move,
                modifiers: egui::Modifiers::NONE,
            })],
            ..Default::default()
        };
        record_complete_window_snapshot(&mut coordinator, source);
        record_complete_window_snapshot(&mut coordinator, callback);
        record_unknown_platform_facts(&mut coordinator);
        let ingress = coordinator.freeze_host_ingress().unwrap();
        let cycle = crate::HostedViewportCycle::with_native_ingress(
            [source_input, callback_input],
            ingress,
        )
        .expect("both native callback inputs form one exact hosted cycle");

        assert!(cycle.claim_native_scroll_derivative(pointer_sequence));
        cycle
            .run(
                [source_viewport, callback_viewport],
                |_| Ok(()),
                |input| {
                    assert!(input.raw_input().events.is_empty());
                    Ok(())
                },
                |_| Ok(()),
            )
            .expect("the cross-viewport derivative is removed before either egui pass");
    }

    #[test]
    fn required_scroll_derivative_missing_from_raw_input_fails_closed() {
        let mut coordinator = NativePlatformCoordinator::default();
        let binding = coordinator
            .register_viewport(egui::ViewportId::ROOT)
            .unwrap();
        let backend_sequence = egui::BackendEventSequence::new(65);
        let scroll = NativeScrollEdge::new(
            NativePointerDeviceId::new(9),
            None,
            NativeScrollPhase::Discrete,
            Some(NativeScrollDelta::Lines(
                NativeFiniteScrollVector::new(0.0, 1.0).unwrap(),
            )),
            NativeAuthority::unknown(NativeUnavailableReason::NotObserved),
            NativeAuthority::known(crate::NativeScrollModifiers::default()),
        )
        .unwrap();
        let pointer_sequence = coordinator
            .record_pointer_edge_for_backend(
                backend_sequence,
                NativePointerEdgeFacts::new(
                    NativePointerSource::Viewport(binding),
                    native_delivery(binding),
                    pointer(9, 1),
                    NativePointerEdgeKind::Scrolled(scroll),
                    unknown_point(),
                    unknown_hover(),
                    unknown_capture(),
                ),
            )
            .unwrap();
        let raw_input = egui::RawInput {
            viewport_id: egui::ViewportId::ROOT,
            ..Default::default()
        };
        let ingress = freeze_single_binding(&mut coordinator, binding);
        let cycle = crate::HostedViewportCycle::with_native_ingress([raw_input], ingress)
            .expect("a required wheel derivative still forms one exact native cycle");

        assert!(
            !cycle.claim_native_scroll_derivative(pointer_sequence),
            "absence cannot prove that a required framework derivative never existed"
        );
    }

    #[test]
    fn explicit_journal_only_scroll_proves_no_derivative_affinely() {
        let mut coordinator = NativePlatformCoordinator::default();
        let binding = coordinator
            .register_viewport(egui::ViewportId::ROOT)
            .unwrap();
        let backend_sequence = egui::BackendEventSequence::new(66);
        let scroll = NativeScrollEdge::new(
            NativePointerDeviceId::new(9),
            None,
            NativeScrollPhase::Discrete,
            Some(NativeScrollDelta::Lines(
                NativeFiniteScrollVector::new(0.0, 1.0).unwrap(),
            )),
            NativeAuthority::unknown(NativeUnavailableReason::NotObserved),
            NativeAuthority::known(crate::NativeScrollModifiers::default()),
        )
        .unwrap();
        let pointer_sequence = coordinator
            .record_pointer_edge_without_derivative_for_backend(
                backend_sequence,
                NativePointerEdgeFacts::new(
                    NativePointerSource::Viewport(binding),
                    native_delivery(binding),
                    pointer(9, 1),
                    NativePointerEdgeKind::Scrolled(scroll),
                    unknown_point(),
                    unknown_hover(),
                    unknown_capture(),
                ),
            )
            .unwrap();
        let raw_input = egui::RawInput {
            viewport_id: egui::ViewportId::ROOT,
            ..Default::default()
        };
        let ingress = freeze_single_binding(&mut coordinator, binding);
        let cycle = crate::HostedViewportCycle::with_native_ingress([raw_input], ingress)
            .expect("an explicitly journal-only scroll forms one exact native cycle");

        assert!(cycle.claim_native_scroll_derivative(pointer_sequence));
        assert!(
            !cycle.claim_native_scroll_derivative(pointer_sequence),
            "the explicit absence proof is affine"
        );
    }

    #[test]
    fn terminal_scroll_derivative_does_not_require_source_raw_input() {
        let mut coordinator = NativePlatformCoordinator::default();
        let root = coordinator
            .register_viewport(egui::ViewportId::ROOT)
            .unwrap();
        let source_viewport = viewport("retired-scroll-owner");
        let source = coordinator.register_viewport(source_viewport).unwrap();
        let backend_sequence = egui::BackendEventSequence::new(661);
        let scroll = NativeScrollEdge::new(
            NativePointerDeviceId::new(9),
            Some(NativeScrollSequenceToken::new(1)),
            NativeScrollPhase::Cancel(NativeScrollCancelReason::BindingRetired),
            None,
            NativeAuthority::unknown(NativeUnavailableReason::Retired),
            NativeAuthority::unknown(NativeUnavailableReason::Retired),
        )
        .unwrap();
        let pointer_sequence = coordinator
            .record_pointer_edge_without_derivative_for_backend(
                backend_sequence,
                NativePointerEdgeFacts::new(
                    NativePointerSource::Viewport(source),
                    native_delivery(source),
                    pointer(9, 1),
                    NativePointerEdgeKind::Scrolled(scroll),
                    unknown_point(),
                    unknown_hover(),
                    unknown_capture(),
                ),
            )
            .unwrap();
        let root_input = egui::RawInput {
            viewport_id: egui::ViewportId::ROOT,
            ..Default::default()
        };
        record_complete_window_snapshot(&mut coordinator, root);
        record_complete_window_snapshot(&mut coordinator, source);
        record_unknown_platform_facts(&mut coordinator);
        let ingress = coordinator.freeze_host_ingress().unwrap();
        let cycle = crate::HostedViewportCycle::with_native_ingress([root_input], ingress)
            .expect("a retired source may be absent from the hosted RawInput roster");

        assert!(cycle.claim_native_scroll_derivative(pointer_sequence));
    }

    #[test]
    fn terminal_scroll_derivative_accepts_bindingless_device_removal() {
        let mut coordinator = NativePlatformCoordinator::default();
        let root = coordinator
            .register_viewport(egui::ViewportId::ROOT)
            .unwrap();
        let backend_sequence = egui::BackendEventSequence::new(662);
        let scroll = NativeScrollEdge::new(
            NativePointerDeviceId::new(9),
            Some(NativeScrollSequenceToken::new(1)),
            NativeScrollPhase::Cancel(NativeScrollCancelReason::DeviceRemoved),
            None,
            NativeAuthority::unknown(NativeUnavailableReason::Retired),
            NativeAuthority::unknown(NativeUnavailableReason::Retired),
        )
        .unwrap();
        let pointer_sequence = coordinator
            .record_pointer_edge_without_derivative_for_backend(
                backend_sequence,
                NativePointerEdgeFacts::new(
                    NativePointerSource::None,
                    NativeAuthority::unknown(NativeUnavailableReason::Retired),
                    pointer(9, 1),
                    NativePointerEdgeKind::Scrolled(scroll),
                    unknown_point(),
                    unknown_hover(),
                    unknown_capture(),
                ),
            )
            .unwrap();
        let root_input = egui::RawInput {
            viewport_id: egui::ViewportId::ROOT,
            ..Default::default()
        };
        let ingress = freeze_single_binding(&mut coordinator, root);
        let cycle = crate::HostedViewportCycle::with_native_ingress([root_input], ingress)
            .expect("a bindingless device terminal still forms one exact cycle");

        assert!(cycle.claim_native_scroll_derivative(pointer_sequence));
    }

    #[test]
    fn viewport_create_result_waits_for_native_binding_materialization() {
        let mut coordinator = NativePlatformCoordinator::default();
        let parent = coordinator
            .register_viewport(viewport("create-parent"))
            .unwrap();
        let target = viewport("create-target");
        let correlation =
            NativeViewportCreateCorrelation::new(egui::UserData::new("create-correlation"));
        let request_id = coordinator
            .issue_viewport_create(parent, target, correlation.clone())
            .unwrap();
        let request = NativeViewportCreateRequest {
            id: request_id,
            parent,
            viewport_id: target,
            builder: egui::ViewportBuilder::default().with_visible(false),
            viewport_ui_cb: std::sync::Arc::new(|_| {}),
            correlation,
        };

        assert_eq!(coordinator.active_binding(target), None);
        coordinator
            .accept_viewport_create_schedule(&request)
            .unwrap();
        assert_eq!(coordinator.active_binding(target), None);

        let ingress = freeze_single_binding(&mut coordinator, parent);
        assert_eq!(ingress.platform().inventory(), &[parent]);
        assert!(ingress.viewport_create_results().is_empty());

        let target_binding = coordinator.register_viewport(target).unwrap();
        record_complete_window_snapshot(&mut coordinator, parent);
        record_complete_window_snapshot(&mut coordinator, target_binding);
        record_unknown_platform_facts(&mut coordinator);
        let ingress = coordinator.freeze_host_ingress().unwrap();
        assert_eq!(ingress.platform().inventory(), &[parent, target_binding]);
        assert_eq!(ingress.viewport_create_results().len(), 1);
        assert_eq!(
            ingress.viewport_create_results()[0].outcome(),
            NativeViewportCreateDispatchOutcome::Materialized
        );
        assert_eq!(ingress.viewport_create_results()[0].viewport_id(), target);
        assert!(matches!(
            ingress.ordered().records()[0].event(),
            NativeIngressEvent::ViewportCreateResult(result)
                if result.request_id() == request_id
        ));
    }

    #[test]
    fn scheduled_viewport_create_failure_is_terminal_without_a_binding() {
        let mut coordinator = NativePlatformCoordinator::default();
        let parent = coordinator
            .register_viewport(viewport("failed-create-parent"))
            .unwrap();
        let target = viewport("failed-create-target");
        let correlation =
            NativeViewportCreateCorrelation::new(egui::UserData::new("failed-create"));
        let request_id = coordinator
            .issue_viewport_create(parent, target, correlation.clone())
            .unwrap();
        let request = NativeViewportCreateRequest {
            id: request_id,
            parent,
            viewport_id: target,
            builder: egui::ViewportBuilder::default().with_visible(false),
            viewport_ui_cb: std::sync::Arc::new(|_| {}),
            correlation,
        };
        coordinator
            .accept_viewport_create_schedule(&request)
            .unwrap();

        assert!(coordinator.fail_scheduled_viewport_create(target).unwrap());
        assert!(!coordinator.fail_scheduled_viewport_create(target).unwrap());
        assert_eq!(coordinator.active_binding(target), None);

        let ingress = freeze_single_binding(&mut coordinator, parent);
        assert_eq!(ingress.platform().inventory(), &[parent]);
        assert_eq!(ingress.viewport_create_results().len(), 1);
        assert_eq!(
            ingress.viewport_create_results()[0].outcome(),
            NativeViewportCreateDispatchOutcome::Failed
        );
        assert_eq!(ingress.viewport_create_results()[0].viewport_id(), target);
    }

    #[test]
    fn cancelled_viewport_create_is_silent_and_retryable() {
        let mut coordinator = NativePlatformCoordinator::default();
        let parent = coordinator
            .register_viewport(viewport("cancelled-create-parent"))
            .unwrap();
        let target = viewport("cancelled-create-target");
        let ingress_before = coordinator.ingress_ordinal;
        let correlation =
            NativeViewportCreateCorrelation::new(egui::UserData::new("aborted-create"));
        let request_id = coordinator
            .issue_viewport_create(parent, target, correlation.clone())
            .unwrap();
        let request = NativeViewportCreateRequest {
            id: request_id,
            parent,
            viewport_id: target,
            builder: egui::ViewportBuilder::default().with_visible(false),
            viewport_ui_cb: std::sync::Arc::new(|_| {}),
            correlation,
        };

        coordinator.cancel_viewport_create_issue(&request).unwrap();

        assert_eq!(coordinator.ingress_ordinal, ingress_before);
        assert!(!coordinator.pending_viewport_creates.contains_key(&target));
        coordinator
            .issue_viewport_create(
                parent,
                target,
                NativeViewportCreateCorrelation::new(egui::UserData::new("retry-create")),
            )
            .expect("an aborted host cycle must release the viewport-create lane");
    }

    #[test]
    fn retired_parent_rejects_a_create_before_schedule_acceptance() {
        let mut coordinator = NativePlatformCoordinator::default();
        let parent = coordinator
            .register_viewport(viewport("retired-create-parent"))
            .unwrap();
        let target = viewport("retired-create-target");
        let correlation =
            NativeViewportCreateCorrelation::new(egui::UserData::new("retired-create"));
        let request_id = coordinator
            .issue_viewport_create(parent, target, correlation.clone())
            .unwrap();
        let request = NativeViewportCreateRequest {
            id: request_id,
            parent,
            viewport_id: target,
            builder: egui::ViewportBuilder::default().with_visible(false),
            viewport_ui_cb: std::sync::Arc::new(|_| {}),
            correlation,
        };
        coordinator.retire_viewport(parent).unwrap();
        confirm_binding_ingress_quiescence(&mut coordinator, parent);

        let retired = freeze_empty_roster(&mut coordinator);
        assert_eq!(retired.retirement_tombstones().len(), 1);
        assert!(retired.binding_ingress_quiescences().is_empty());

        assert_eq!(
            coordinator.accept_viewport_create_schedule(&request),
            Err(NativePlatformError::RetiredBinding)
        );
        coordinator
            .report_viewport_create_terminal(
                &request,
                NativeViewportCreateDispatchOutcome::Rejected,
            )
            .unwrap();

        let ingress = freeze_empty_roster(&mut coordinator);
        assert_eq!(ingress.viewport_create_results().len(), 1);
        assert_eq!(
            ingress.viewport_create_results()[0].outcome(),
            NativeViewportCreateDispatchOutcome::Rejected
        );
        assert_eq!(ingress.binding_ingress_quiescences().len(), 1);
        assert_eq!(ingress.binding_ingress_quiescences()[0].binding(), parent);
        assert!(matches!(
            ingress.ordered().records()[0].event(),
            NativeIngressEvent::ViewportCreateResult(result)
                if result.parent() == parent
        ));
        assert!(matches!(
            ingress.ordered().records()[1].event(),
            NativeIngressEvent::BindingIngressQuiesced(quiesced)
                if quiesced.binding() == parent
        ));
        assert_eq!(coordinator.active_binding(target), None);
    }

    #[test]
    fn scheduled_create_delays_parent_quiescence_until_materialization() {
        let mut coordinator = NativePlatformCoordinator::default();
        let parent = coordinator
            .register_viewport(viewport("materialized-retired-parent"))
            .unwrap();
        let target = viewport("materialized-after-parent-retirement");
        let correlation =
            NativeViewportCreateCorrelation::new(egui::UserData::new("materialized-late"));
        let request_id = coordinator
            .issue_viewport_create(parent, target, correlation.clone())
            .unwrap();
        let request = NativeViewportCreateRequest {
            id: request_id,
            parent,
            viewport_id: target,
            builder: egui::ViewportBuilder::default().with_visible(false),
            viewport_ui_cb: std::sync::Arc::new(|_| {}),
            correlation,
        };
        coordinator
            .accept_viewport_create_schedule(&request)
            .unwrap();
        coordinator.retire_viewport(parent).unwrap();
        confirm_binding_ingress_quiescence(&mut coordinator, parent);

        let retired = freeze_empty_roster(&mut coordinator);
        assert!(retired.binding_ingress_quiescences().is_empty());

        let target_binding = coordinator.register_viewport(target).unwrap();
        let materialized = freeze_single_binding(&mut coordinator, target_binding);

        assert_eq!(materialized.viewport_create_results().len(), 1);
        assert_eq!(
            materialized.viewport_create_results()[0].outcome(),
            NativeViewportCreateDispatchOutcome::Materialized
        );
        assert_eq!(materialized.binding_ingress_quiescences().len(), 1);
        assert_eq!(
            materialized.binding_ingress_quiescences()[0].binding(),
            parent
        );
        assert!(matches!(
            materialized.ordered().records()[0].event(),
            NativeIngressEvent::ViewportCreateResult(result)
                if result.parent() == parent
        ));
        assert!(matches!(
            materialized.ordered().records()[1].event(),
            NativeIngressEvent::BindingIngressQuiesced(quiesced)
                if quiesced.binding() == parent
        ));
    }

    #[test]
    fn native_ingress_preserves_presentation_before_later_pointer() {
        let mut coordinator = NativePlatformCoordinator::default();
        let binding = coordinator
            .register_viewport(viewport("ordered-presentation"))
            .unwrap();
        let ticket = coordinator.begin_presentation(binding).unwrap();
        coordinator
            .record_presentation_result(
                ticket,
                egui::PresentationResult::new(
                    binding.viewport_id(),
                    egui::UserData::new("first-frame"),
                    egui::PaintOutcome::Swapped,
                    None,
                ),
            )
            .unwrap();
        let move_sequence = coordinator
            .record_pointer_edge_for_backend(
                egui::BackendEventSequence::new(42),
                NativePointerEdgeFacts::new(
                    NativePointerSource::Viewport(binding),
                    native_delivery(binding),
                    pointer(2, 1),
                    NativePointerEdgeKind::Moved,
                    unknown_point(),
                    unknown_hover(),
                    unknown_capture(),
                ),
            )
            .unwrap();

        let ingress = freeze_single_binding(&mut coordinator, binding);
        let records = ingress.ordered().records();

        assert!(matches!(
            records[0].event(),
            NativeIngressEvent::PresentationResult(result)
                if result.binding() == binding
        ));
        assert!(matches!(
            records[1].event(),
            NativeIngressEvent::PointerEdge(edge) if edge.sequence() == move_sequence
        ));
        assert!(matches!(
            records[2].event(),
            NativeIngressEvent::PlatformSnapshot(_)
        ));
    }

    #[test]
    fn effect_dispatch_needs_a_later_observation_for_acknowledgement() {
        let mut coordinator = NativePlatformCoordinator::default();
        let binding = coordinator.register_viewport(viewport("effect")).unwrap();
        let baseline = coordinator
            .observe_property(
                binding,
                NativeEffectProperty::Presentation,
                NativeAuthority::known(NativePresentationState::Hidden),
                ObservationAcknowledgement::Baseline,
            )
            .unwrap();
        let request = coordinator
            .issue_effect(
                binding,
                NativeWindowEffect::SetVisible(true),
                NativeEffectCorrelation::new(egui::UserData::new(41_u64)),
            )
            .unwrap();

        assert_eq!(request.observation_fence(), baseline.generation());
        assert_eq!(
            coordinator.observe_property(
                binding,
                NativeEffectProperty::Presentation,
                NativeAuthority::known(NativePresentationState::Visible),
                ObservationAcknowledgement::Applied(&request),
            ),
            Err(NativePlatformError::EffectNotDispatched)
        );

        coordinator
            .report_effect_dispatch(&request, NativeEffectDispatchOutcome::Dispatched)
            .unwrap();

        let observed = coordinator
            .observe_property(
                binding,
                NativeEffectProperty::Presentation,
                NativeAuthority::known(NativePresentationState::Visible),
                ObservationAcknowledgement::Applied(&request),
            )
            .unwrap();
        assert!(observed.generation() > request.observation_fence());
        assert_eq!(
            observed
                .acknowledgement()
                .correlation()
                .and_then(|value| value.user_data().downcast_ref::<u64>()),
            Some(&41)
        );
    }

    #[test]
    fn cancelled_effect_reservation_is_silent_and_releases_its_lane() {
        let mut coordinator = NativePlatformCoordinator::default();
        let binding = coordinator
            .register_viewport(viewport("cancelled-effect"))
            .unwrap();
        let ingress_before = coordinator.ingress_ordinal;
        let request = coordinator
            .issue_effect(
                binding,
                NativeWindowEffect::SetVisible(true),
                NativeEffectCorrelation::new(egui::UserData::new("aborted")),
            )
            .unwrap();

        coordinator.cancel_effect_issue(&request).unwrap();

        assert_eq!(coordinator.ingress_ordinal, ingress_before);
        assert!(coordinator.pending_effects.is_empty());
        coordinator
            .issue_effect(
                binding,
                NativeWindowEffect::SetVisible(false),
                NativeEffectCorrelation::new(egui::UserData::new("retry")),
            )
            .expect("an aborted transaction must release the exact property lane");
    }

    #[test]
    fn one_property_lane_accepts_ordered_outstanding_effects() {
        let mut coordinator = NativePlatformCoordinator::default();
        let binding = coordinator
            .register_viewport(viewport("ordered-pointer-input-effects"))
            .unwrap();
        let enable = coordinator
            .issue_effect(
                binding,
                NativeWindowEffect::SetPointerPassThrough(true),
                NativeEffectCorrelation::new(egui::UserData::new("enable")),
            )
            .unwrap();
        coordinator
            .report_effect_dispatch(&enable, NativeEffectDispatchOutcome::Dispatched)
            .unwrap();

        let restore = coordinator
            .issue_effect(
                binding,
                NativeWindowEffect::SetPointerPassThrough(false),
                NativeEffectCorrelation::new(egui::UserData::new("restore")),
            )
            .expect("a causal successor must not wait for predecessor observation");
        coordinator
            .report_effect_dispatch(&restore, NativeEffectDispatchOutcome::Dispatched)
            .unwrap();

        let enable_observation = coordinator
            .observe_property(
                binding,
                NativeEffectProperty::PointerInput,
                NativeAuthority::known(NativePointerInputState::PassThrough),
                ObservationAcknowledgement::Applied(&enable),
            )
            .unwrap();
        let restore_observation = coordinator
            .observe_property(
                binding,
                NativeEffectProperty::PointerInput,
                NativeAuthority::known(NativePointerInputState::ReceivesInput),
                ObservationAcknowledgement::Applied(&restore),
            )
            .unwrap();

        assert_eq!(
            enable_observation
                .acknowledgement()
                .correlation()
                .and_then(|value| value.user_data().downcast_ref::<&str>()),
            Some(&"enable")
        );
        assert_eq!(
            restore_observation
                .acknowledgement()
                .correlation()
                .and_then(|value| value.user_data().downcast_ref::<&str>()),
            Some(&"restore")
        );
    }

    #[test]
    fn presentation_result_retains_the_ticket_binding_after_recreation() {
        let mut coordinator = NativePlatformCoordinator::default();
        let viewport_id = viewport("presentation");
        let first = coordinator.register_viewport(viewport_id).unwrap();
        let ticket = coordinator.begin_presentation(first).unwrap();
        coordinator.retire_viewport(first).unwrap();
        let second = coordinator.register_viewport(viewport_id).unwrap();

        let result = egui::PresentationResult::new(
            viewport_id,
            egui::UserData::new("frame"),
            egui::PaintOutcome::SubmittedToSwapchain,
            None,
        );
        coordinator
            .record_presentation_result(ticket, result)
            .unwrap();
        record_complete_window_snapshot(&mut coordinator, second);
        record_unknown_platform_facts(&mut coordinator);
        let ingress = coordinator.freeze_host_ingress().unwrap();
        let bound = &ingress.presentation_results()[0];

        assert_eq!(bound.binding(), first);
        assert_ne!(bound.binding(), second);
        assert_eq!(bound.result().viewport_id(), viewport_id);
    }

    #[test]
    fn presentation_follow_up_matches_the_requested_terminal_class() {
        let wake = NativeCoordinatorWake::default();
        let mut coordinator = NativePlatformCoordinator::with_wake(wake.clone());
        let binding = coordinator
            .register_viewport(viewport("presentation-follow-up"))
            .unwrap();

        let ordinary = coordinator.begin_presentation(binding).unwrap();
        coordinator
            .record_presentation_result(
                ordinary,
                egui::PresentationResult::new(
                    binding.viewport_id(),
                    egui::UserData::new("ordinary"),
                    egui::PaintOutcome::SubmittedToSwapchain,
                    None,
                ),
            )
            .unwrap();
        assert!(!wake.is_pending());

        let skipped_success = coordinator.begin_presentation(binding).unwrap();
        coordinator
            .record_presentation_result_with_follow_up(
                skipped_success,
                egui::PresentationResult::new(
                    binding.viewport_id(),
                    egui::UserData::new("skipped-success"),
                    egui::PaintOutcome::Skipped(egui::PaintSkipReason::SurfaceOccluded),
                    None,
                ),
                Some(crate::HostedPresentationFollowUp::SuccessfulSubmission),
            )
            .unwrap();
        assert!(!wake.is_pending());

        let successful = coordinator.begin_presentation(binding).unwrap();
        coordinator
            .record_presentation_result_with_follow_up(
                successful,
                egui::PresentationResult::new(
                    binding.viewport_id(),
                    egui::UserData::new("successful"),
                    egui::PaintOutcome::SubmittedToSwapchain,
                    None,
                ),
                Some(crate::HostedPresentationFollowUp::SuccessfulSubmission),
            )
            .unwrap();
        assert!(wake.is_pending());

        wake.begin_consume();
        let failed_terminal = coordinator.begin_presentation(binding).unwrap();
        coordinator
            .record_presentation_result_with_follow_up(
                failed_terminal,
                egui::PresentationResult::new(
                    binding.viewport_id(),
                    egui::UserData::new("failed-terminal"),
                    egui::PaintOutcome::Failed(egui::PaintFailure::CoordinatorAborted),
                    None,
                ),
                Some(crate::HostedPresentationFollowUp::AnyResult),
            )
            .unwrap();
        assert!(wake.is_pending());
    }

    #[test]
    fn retirement_waits_for_the_last_presentation_result_before_quiescing() {
        let mut coordinator = NativePlatformCoordinator::default();
        let viewport_id = viewport("retire-before-result");
        let binding = coordinator.register_viewport(viewport_id).unwrap();
        let ticket = coordinator.begin_presentation(binding).unwrap();
        let serial = ticket.serial;
        let tombstone = coordinator.retire_viewport(binding).unwrap();
        confirm_binding_ingress_quiescence(&mut coordinator, binding);

        let retired = freeze_empty_roster(&mut coordinator);
        assert_eq!(retired.retirement_tombstones(), &[tombstone]);
        assert!(retired.binding_ingress_quiescences().is_empty());

        coordinator
            .record_presentation_result(
                ticket,
                egui::PresentationResult::new(
                    viewport_id,
                    egui::UserData::new("late-frame"),
                    egui::PaintOutcome::SubmittedToSwapchain,
                    None,
                ),
            )
            .unwrap();
        let settled = freeze_empty_roster(&mut coordinator);
        assert_eq!(settled.presentation_results()[0].serial(), serial);
        assert_eq!(settled.binding_ingress_quiescences().len(), 1);
        let quiesced = settled.binding_ingress_quiescences()[0];
        assert_eq!(quiesced.binding(), binding);
        assert_eq!(quiesced.last_started_presentation(), Some(serial));
        assert!(matches!(
            settled.ordered().records()[0].event(),
            NativeIngressEvent::PresentationResult(result) if result.serial() == serial
        ));
        assert!(matches!(
            settled.ordered().records()[1].event(),
            NativeIngressEvent::BindingIngressQuiesced(recorded) if *recorded == quiesced
        ));
    }

    #[test]
    fn retirement_alone_cannot_claim_owner_ingress_quiescence() {
        let mut coordinator = NativePlatformCoordinator::default();
        let binding = coordinator
            .register_viewport(viewport("retirement-needs-owner-proof"))
            .unwrap();
        let tombstone = coordinator.retire_viewport(binding).unwrap();

        let ingress = freeze_empty_roster(&mut coordinator);

        assert_eq!(ingress.retirement_tombstones(), &[tombstone]);
        assert!(ingress.binding_ingress_quiescences().is_empty());
        assert!(coordinator.minted.contains(&binding));
        assert!(coordinator.tombstones.contains_key(&binding));
    }

    #[test]
    fn retirement_quiesces_only_after_every_outstanding_presentation() {
        let mut coordinator = NativePlatformCoordinator::default();
        let viewport_id = viewport("multiple-outstanding-presentations");
        let binding = coordinator.register_viewport(viewport_id).unwrap();
        let first = coordinator.begin_presentation(binding).unwrap();
        let second = coordinator.begin_presentation(binding).unwrap();
        let final_serial = second.serial;
        coordinator.retire_viewport(binding).unwrap();
        confirm_binding_ingress_quiescence(&mut coordinator, binding);

        coordinator
            .record_presentation_result(
                first,
                egui::PresentationResult::new(
                    viewport_id,
                    egui::UserData::new("first-frame"),
                    egui::PaintOutcome::SubmittedToSwapchain,
                    None,
                ),
            )
            .unwrap();
        let first_ingress = freeze_empty_roster(&mut coordinator);
        assert!(first_ingress.binding_ingress_quiescences().is_empty());

        coordinator
            .record_presentation_result(
                second,
                egui::PresentationResult::new(
                    viewport_id,
                    egui::UserData::new("second-frame"),
                    egui::PaintOutcome::Swapped,
                    None,
                ),
            )
            .unwrap();
        let final_ingress = freeze_empty_roster(&mut coordinator);
        assert_eq!(final_ingress.binding_ingress_quiescences().len(), 1);
        assert_eq!(
            final_ingress.binding_ingress_quiescences()[0].last_started_presentation(),
            Some(final_serial)
        );
    }

    #[test]
    fn completed_presentation_quiesces_immediately_after_later_retirement() {
        let mut coordinator = NativePlatformCoordinator::default();
        let viewport_id = viewport("result-before-retire");
        let binding = coordinator.register_viewport(viewport_id).unwrap();
        let ticket = coordinator.begin_presentation(binding).unwrap();
        let serial = ticket.serial;
        coordinator
            .record_presentation_result(
                ticket,
                egui::PresentationResult::new(
                    viewport_id,
                    egui::UserData::new("early-frame"),
                    egui::PaintOutcome::Swapped,
                    None,
                ),
            )
            .unwrap();
        let tombstone = coordinator.retire_viewport(binding).unwrap();
        confirm_binding_ingress_quiescence(&mut coordinator, binding);

        let ingress = freeze_empty_roster(&mut coordinator);
        let quiesced = ingress.binding_ingress_quiescences()[0];
        assert_eq!(quiesced.binding(), binding);
        assert_eq!(quiesced.retirement_generation(), tombstone.generation());
        assert_eq!(quiesced.last_started_presentation(), Some(serial));
        assert!(matches!(
            ingress.ordered().records()[0].event(),
            NativeIngressEvent::PresentationResult(result) if result.serial() == serial
        ));
        assert!(matches!(
            ingress.ordered().records()[1].event(),
            NativeIngressEvent::Retirement(recorded) if *recorded == tombstone
        ));
        assert!(matches!(
            ingress.ordered().records()[2].event(),
            NativeIngressEvent::BindingIngressQuiesced(recorded) if *recorded == quiesced
        ));
    }

    #[test]
    fn presentation_quiescence_isolated_across_viewport_incarnation_aba() {
        let mut coordinator = NativePlatformCoordinator::default();
        let viewport_id = viewport("presentation-quiescence-aba");
        let first = coordinator.register_viewport(viewport_id).unwrap();
        let first_ticket = coordinator.begin_presentation(first).unwrap();
        coordinator.retire_viewport(first).unwrap();
        confirm_binding_ingress_quiescence(&mut coordinator, first);
        let second = coordinator.register_viewport(viewport_id).unwrap();
        let second_ticket = coordinator.begin_presentation(second).unwrap();

        coordinator
            .record_presentation_result(
                first_ticket,
                egui::PresentationResult::new(
                    viewport_id,
                    egui::UserData::new("a1-frame"),
                    egui::PaintOutcome::SubmittedToSwapchain,
                    None,
                ),
            )
            .unwrap();
        let first_ingress = freeze_single_binding(&mut coordinator, second);
        assert_eq!(first_ingress.binding_ingress_quiescences().len(), 1);
        assert_eq!(
            first_ingress.binding_ingress_quiescences()[0].binding(),
            first
        );

        coordinator.retire_viewport(second).unwrap();
        confirm_binding_ingress_quiescence(&mut coordinator, second);
        coordinator
            .record_presentation_result(
                second_ticket,
                egui::PresentationResult::new(
                    viewport_id,
                    egui::UserData::new("a2-frame"),
                    egui::PaintOutcome::Swapped,
                    None,
                ),
            )
            .unwrap();
        let second_ingress = freeze_empty_roster(&mut coordinator);
        assert_eq!(second_ingress.binding_ingress_quiescences().len(), 1);
        assert_eq!(
            second_ingress.binding_ingress_quiescences()[0].binding(),
            second
        );
    }

    #[test]
    fn duplicate_late_presentation_result_cannot_reopen_quiesced_binding() {
        let mut coordinator = NativePlatformCoordinator::default();
        let viewport_id = viewport("duplicate-late-presentation");
        let binding = coordinator.register_viewport(viewport_id).unwrap();
        let ticket = coordinator.begin_presentation(binding).unwrap();
        let duplicate = NativePresentationTicket {
            binding,
            serial: ticket.serial,
        };
        coordinator.retire_viewport(binding).unwrap();
        confirm_binding_ingress_quiescence(&mut coordinator, binding);
        coordinator
            .record_presentation_result(
                ticket,
                egui::PresentationResult::new(
                    viewport_id,
                    egui::UserData::new("first-terminal"),
                    egui::PaintOutcome::Swapped,
                    None,
                ),
            )
            .unwrap();
        assert_eq!(
            coordinator.record_presentation_result(
                duplicate,
                egui::PresentationResult::new(
                    viewport_id,
                    egui::UserData::new("duplicate-terminal"),
                    egui::PaintOutcome::SubmittedToSwapchain,
                    None,
                ),
            ),
            Err(NativePlatformError::UnknownPresentationTicket)
        );

        let ingress = freeze_empty_roster(&mut coordinator);
        assert_eq!(ingress.presentation_results().len(), 1);
        assert_eq!(ingress.binding_ingress_quiescences().len(), 1);
    }

    #[test]
    fn retirement_without_presentations_quiesces_in_the_same_ordered_batch() {
        let mut coordinator = NativePlatformCoordinator::default();
        let binding = coordinator
            .register_viewport(viewport("immediate-presentation-quiescence"))
            .unwrap();
        let tombstone = coordinator.retire_viewport(binding).unwrap();
        confirm_binding_ingress_quiescence(&mut coordinator, binding);

        let ingress = freeze_empty_roster(&mut coordinator);
        let quiesced = ingress.binding_ingress_quiescences()[0];
        assert_eq!(quiesced.binding(), binding);
        assert_eq!(quiesced.retirement_generation(), tombstone.generation());
        assert_eq!(quiesced.last_started_presentation(), None);
        assert!(matches!(
            ingress.ordered().records()[0].event(),
            NativeIngressEvent::Retirement(recorded) if *recorded == tombstone
        ));
        assert!(matches!(
            ingress.ordered().records()[1].event(),
            NativeIngressEvent::BindingIngressQuiesced(recorded) if *recorded == quiesced
        ));
    }

    #[test]
    fn aborted_binding_quiescence_batch_retains_every_aba_guard() {
        let mut coordinator = NativePlatformCoordinator::default();
        let binding = coordinator
            .register_viewport(viewport("aborted-binding-quiescence"))
            .unwrap();
        coordinator
            .observe_property(
                binding,
                NativeEffectProperty::Focus,
                NativeAuthority::known(false),
                ObservationAcknowledgement::Baseline,
            )
            .unwrap();
        coordinator.retire_viewport(binding).unwrap();
        confirm_binding_ingress_quiescence(&mut coordinator, binding);
        record_unknown_platform_facts(&mut coordinator);
        let prepared = coordinator.prepare_host_ingress().unwrap();
        let (ingress, settlement) = prepared.into_parts();

        assert_eq!(ingress.binding_ingress_quiescences().len(), 1);
        assert!(coordinator.minted.contains(&binding));
        assert!(coordinator.tombstones.contains_key(&binding));
        assert!(
            coordinator
                .observation_generations
                .keys()
                .any(|(observed_binding, _)| *observed_binding == binding)
        );

        let wrong_settlement = NativeHostIngressSettlementKey(NativeIngressOrdinal::new(
            settlement.0.get().checked_add(1).unwrap(),
        ));
        assert_eq!(
            coordinator.commit_host_ingress(wrong_settlement),
            Err(NativePlatformError::HostIngressSettlementMismatch)
        );
        assert!(coordinator.minted.contains(&binding));
        assert!(coordinator.tombstones.contains_key(&binding));

        coordinator.abort_host_ingress(settlement).unwrap();

        assert!(coordinator.minted.contains(&binding));
        assert!(coordinator.tombstones.contains_key(&binding));
        assert!(
            coordinator
                .observation_generations
                .keys()
                .any(|(observed_binding, _)| *observed_binding == binding)
        );
    }

    #[test]
    fn committed_binding_quiescence_bounds_same_provider_retention() {
        let mut coordinator = NativePlatformCoordinator::default();
        let viewport_id = viewport("bounded-binding-retention");
        let mut last_binding = None;

        for _ in 0..10_000 {
            let binding = coordinator.register_viewport(viewport_id).unwrap();
            coordinator
                .observe_property(
                    binding,
                    NativeEffectProperty::Focus,
                    NativeAuthority::known(false),
                    ObservationAcknowledgement::Baseline,
                )
                .unwrap();
            coordinator.retire_viewport(binding).unwrap();
            confirm_binding_ingress_quiescence(&mut coordinator, binding);
            record_unknown_platform_facts(&mut coordinator);
            let prepared = coordinator.prepare_host_ingress().unwrap();
            let (ingress, settlement) = prepared.into_parts();

            assert_eq!(ingress.binding_ingress_quiescences().len(), 1);
            assert_eq!(ingress.binding_ingress_quiescences()[0].binding(), binding);
            assert_eq!(coordinator.minted.len(), 1);
            assert_eq!(coordinator.tombstones.len(), 1);
            assert_eq!(coordinator.observation_generations.len(), 1);

            coordinator.commit_host_ingress(settlement).unwrap();

            assert!(coordinator.minted.is_empty());
            assert!(coordinator.tombstones.is_empty());
            assert!(coordinator.observation_generations.is_empty());
            last_binding = Some(binding);
        }

        let last_binding = last_binding.expect("the churn loop produces a binding");
        assert!(matches!(
            coordinator.begin_presentation(last_binding),
            Err(NativePlatformError::UnknownBinding)
        ));
        assert_eq!(coordinator.retirement_tombstone(last_binding), None);
    }

    #[test]
    fn presentation_ticket_rejects_a_different_logical_viewport() {
        let mut coordinator = NativePlatformCoordinator::default();
        let binding = coordinator.register_viewport(viewport("first")).unwrap();
        let ticket = coordinator.begin_presentation(binding).unwrap();
        let result = egui::PresentationResult::new(
            viewport("second"),
            egui::UserData::new("frame"),
            egui::PaintOutcome::SubmittedToSwapchain,
            None,
        );

        assert_eq!(
            coordinator.record_presentation_result(ticket, result),
            Err(NativePlatformError::PresentationViewportMismatch)
        );
    }

    #[test]
    fn pointer_journal_preserves_distinct_pointer_identities() {
        let mut coordinator = NativePlatformCoordinator::default();
        let binding = coordinator
            .register_viewport(viewport("multi-pointer"))
            .unwrap();
        let first_pointer = pointer(7, 1);
        let second_pointer = pointer(7, 2);

        coordinator
            .record_pointer_edge(
                NativePointerSource::Viewport(binding),
                first_pointer,
                NativePointerEdgeKind::ButtonPressed(NativePointerButton::Primary),
                unknown_point(),
                unknown_hover(),
                unknown_capture(),
            )
            .unwrap();
        coordinator
            .record_pointer_edge(
                NativePointerSource::Viewport(binding),
                second_pointer,
                NativePointerEdgeKind::ButtonPressed(NativePointerButton::Primary),
                unknown_point(),
                unknown_hover(),
                unknown_capture(),
            )
            .unwrap();

        let ingress = freeze_single_binding(&mut coordinator, binding);
        let journal = ingress.pointer_journal();
        assert_eq!(journal.edges()[0].identity(), first_pointer);
        assert_eq!(journal.edges()[1].identity(), second_pointer);
        assert_eq!(first_pointer.device_id(), second_pointer.device_id());
        assert_ne!(first_pointer.pointer_id(), second_pointer.pointer_id());
    }

    #[test]
    fn host_ingress_requires_the_exact_active_roster_without_partial_drain() {
        let mut coordinator = NativePlatformCoordinator::default();
        let first = coordinator.register_viewport(viewport("roster-a")).unwrap();
        let second = coordinator.register_viewport(viewport("roster-b")).unwrap();
        let edge = coordinator
            .record_pointer_edge(
                NativePointerSource::Viewport(first),
                pointer(1, 1),
                NativePointerEdgeKind::Moved,
                unknown_point(),
                unknown_hover(),
                unknown_capture(),
            )
            .unwrap();
        record_complete_window_snapshot(&mut coordinator, first);
        record_unknown_platform_facts(&mut coordinator);

        assert_eq!(
            coordinator.freeze_host_ingress(),
            Err(NativePlatformError::IncompletePlatformRoster)
        );

        record_complete_window_snapshot(&mut coordinator, second);
        let ingress = coordinator.freeze_host_ingress().unwrap();
        assert_eq!(ingress.platform().inventory().len(), 2);
        assert_eq!(ingress.platform().windows().len(), 2);
        assert!(
            ingress
                .platform()
                .windows()
                .iter()
                .map(NativeWindowSnapshot::binding)
                .eq(ingress.platform().inventory().iter().copied())
        );
        assert_eq!(ingress.pointer_journal().edges()[0].sequence(), edge);
        assert_eq!(ingress.platform().snapshot_generation().get(), 1);
        let focus = ingress.platform().focused_observation();
        assert_eq!(focus.generation(), ingress.platform().snapshot_generation());
        assert_eq!(
            focus.authority().unavailable_reason(),
            Some(NativeUnavailableReason::Unsupported)
        );
    }

    #[test]
    fn window_snapshot_cannot_cross_an_inventory_generation() {
        let mut coordinator = NativePlatformCoordinator::default();
        let first = coordinator
            .register_viewport(viewport("snapshot-generation-a"))
            .unwrap();
        let stale = complete_window_snapshot(&mut coordinator, first);
        coordinator
            .register_viewport(viewport("snapshot-generation-b"))
            .unwrap();

        assert_eq!(
            coordinator.record_window_snapshot(stale),
            Err(NativePlatformError::WindowSnapshotMismatch)
        );
    }

    #[test]
    fn host_ingress_keeps_a1_terminals_separate_from_the_a2_roster() {
        let mut coordinator = NativePlatformCoordinator::default();
        let viewport_id = viewport("terminal-aba");
        let first = coordinator.register_viewport(viewport_id).unwrap();
        let ticket = coordinator.begin_presentation(first).unwrap();
        let request = coordinator
            .issue_effect(
                first,
                NativeWindowEffect::SetVisible(false),
                NativeEffectCorrelation::new(egui::UserData::new("hide-a1")),
            )
            .unwrap();
        coordinator
            .report_effect_dispatch(&request, NativeEffectDispatchOutcome::Dispatched)
            .unwrap();
        let retirement_sequence = egui::BackendEventSequence::new(91);
        let tombstone = coordinator
            .retire_viewport_for_backend(first, retirement_sequence)
            .unwrap();
        let second = coordinator.register_viewport(viewport_id).unwrap();
        coordinator
            .record_presentation_result(
                ticket,
                egui::PresentationResult::new(
                    viewport_id,
                    egui::UserData::new("a1-frame"),
                    egui::PaintOutcome::SubmittedToSwapchain,
                    None,
                ),
            )
            .unwrap();
        record_complete_window_snapshot(&mut coordinator, second);
        record_unknown_platform_facts(&mut coordinator);

        let ingress = coordinator.freeze_host_ingress().unwrap();
        assert_eq!(ingress.platform().inventory(), &[second]);
        assert_eq!(ingress.effect_results()[0].binding(), first);
        assert_eq!(ingress.presentation_results()[0].binding(), first);
        assert_eq!(ingress.retirement_tombstones(), &[tombstone.clone()]);
        assert!(matches!(
            ingress.ordered().records()[0].event(),
            NativeIngressEvent::EffectResult(result) if result.binding() == first
        ));
        assert!(matches!(
            ingress.ordered().records()[1].event(),
            NativeIngressEvent::Retirement(recorded) if *recorded == tombstone
        ));
        assert_eq!(
            ingress.ordered().records()[1].backend_event_sequence(),
            Some(retirement_sequence)
        );
        assert!(matches!(
            ingress.ordered().records()[2].event(),
            NativeIngressEvent::PresentationResult(result) if result.binding() == first
        ));
    }

    #[test]
    fn same_roster_focus_changes_advance_only_snapshot_generation() {
        let mut coordinator = NativePlatformCoordinator::default();
        let binding = coordinator
            .register_viewport(viewport("focus-generation"))
            .unwrap();
        record_complete_window_snapshot(&mut coordinator, binding);
        coordinator
            .record_platform_facts(
                NativeAuthority::known(NativeFocusedWindow::None),
                unknown_hover(),
                unknown_capture(),
            )
            .unwrap();
        let first = coordinator.freeze_host_ingress().unwrap();

        record_complete_window_snapshot(&mut coordinator, binding);
        coordinator
            .record_platform_facts(
                NativeAuthority::known(NativeFocusedWindow::Viewport(binding)),
                unknown_hover(),
                unknown_capture(),
            )
            .unwrap();
        let second = coordinator.freeze_host_ingress().unwrap();

        assert_eq!(
            first.platform().inventory_generation(),
            second.platform().inventory_generation()
        );
        assert_eq!(first.platform().snapshot_generation().get(), 1);
        assert_eq!(second.platform().snapshot_generation().get(), 2);
        assert_eq!(
            first.platform().focused().value(),
            Some(&NativeFocusedWindow::None)
        );
        assert_eq!(
            second.platform().focused().value(),
            Some(&NativeFocusedWindow::Viewport(binding))
        );
        assert_eq!(
            first.platform().focused_observation().generation(),
            first.platform().snapshot_generation()
        );
        assert_eq!(
            second.platform().focused_observation().generation(),
            second.platform().snapshot_generation()
        );
    }

    #[test]
    fn late_global_observation_cannot_authorize_a_newer_snapshot() {
        let mut coordinator = NativePlatformCoordinator::default();
        let binding = coordinator
            .register_viewport(viewport("late-focus"))
            .unwrap();
        record_complete_window_snapshot(&mut coordinator, binding);
        coordinator
            .record_platform_facts(
                NativeAuthority::known(NativeFocusedWindow::None),
                unknown_hover(),
                unknown_capture(),
            )
            .unwrap();
        let old = coordinator.freeze_host_ingress().unwrap();

        record_complete_window_snapshot(&mut coordinator, binding);
        coordinator
            .record_platform_facts(
                NativeAuthority::known(NativeFocusedWindow::Viewport(binding)),
                unknown_hover(),
                unknown_capture(),
            )
            .unwrap();
        let current = coordinator.freeze_host_ingress().unwrap();
        let old_focus = old.platform().focused_observation();
        let current_focus = current.platform().focused_observation();

        assert!(!current.platform().authorizes_global_observation(old_focus));
        assert!(
            current
                .platform()
                .authorizes_global_observation(current_focus)
        );
    }

    #[test]
    fn freeze_drains_terminals_once_and_preserves_pointer_watermark() {
        let mut coordinator = NativePlatformCoordinator::default();
        let binding = coordinator.register_viewport(viewport("drain")).unwrap();
        let retired = coordinator
            .register_viewport(viewport("drain-retired"))
            .unwrap();
        let tombstone = coordinator.retire_viewport(retired).unwrap();
        let ticket = coordinator.begin_presentation(binding).unwrap();
        coordinator
            .record_presentation_result(
                ticket,
                egui::PresentationResult::new(
                    binding.viewport_id(),
                    egui::UserData::new("drain-frame"),
                    egui::PaintOutcome::SubmittedToSwapchain,
                    None,
                ),
            )
            .unwrap();
        let request = coordinator
            .issue_effect(
                binding,
                NativeWindowEffect::RequestFocus,
                NativeEffectCorrelation::new(egui::UserData::new("drain-focus")),
            )
            .unwrap();
        coordinator
            .report_effect_dispatch(&request, NativeEffectDispatchOutcome::Unsupported)
            .unwrap();
        let sequence = coordinator
            .record_pointer_edge(
                NativePointerSource::Viewport(binding),
                pointer(3, 9),
                NativePointerEdgeKind::Moved,
                unknown_point(),
                unknown_hover(),
                unknown_capture(),
            )
            .unwrap();
        record_complete_window_snapshot(&mut coordinator, binding);
        record_unknown_platform_facts(&mut coordinator);
        let first = coordinator.freeze_host_ingress().unwrap();

        record_complete_window_snapshot(&mut coordinator, binding);
        record_unknown_platform_facts(&mut coordinator);
        let second = coordinator.freeze_host_ingress().unwrap();

        assert_eq!(first.pointer_journal().previous().get(), 0);
        assert_eq!(first.pointer_journal().through(), sequence);
        assert_eq!(first.effect_results().len(), 1);
        assert_eq!(first.presentation_results().len(), 1);
        assert_eq!(first.retirement_tombstones(), &[tombstone]);
        assert_eq!(second.pointer_journal().previous(), sequence);
        assert_eq!(second.pointer_journal().through(), sequence);
        assert!(second.pointer_journal().edges().is_empty());
        assert!(second.effect_results().is_empty());
        assert!(second.presentation_results().is_empty());
        assert!(second.retirement_tombstones().is_empty());
        assert_eq!(first.ordered().previous().get(), 0);
        assert_eq!(second.ordered().previous(), first.ordered().through());
        assert_eq!(second.ordered().records().len(), 1);
        assert!(matches!(
            second.ordered().records()[0].event(),
            NativeIngressEvent::PlatformSnapshot(generation)
                if generation == second.platform().snapshot_generation()
        ));
    }
}

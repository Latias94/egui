//! Atomic native-host ingress batches and their cross-lane causal order.

use super::{
    accessibility::NativeAccessibilityEdge,
    authority::{NativeCloseState, NativePointerSequence},
    create::NativeViewportCreateResult,
    effect::{NativeEffectResult, NativePropertyObservation},
    keyboard::NativeKeyEdge,
    pointer::{NativePointerEdge, NativePointerJournal},
    presentation::{NativePresentationResult, NativeRetirementQuiesced, NativeRetirementTombstone},
    snapshot::{NativePlatformSnapshot, NativePlatformSnapshotGeneration},
};

/// A coordinator-minted position in the native ingress total order.
///
/// The numeric value is observable for continuity checks, but only the native
/// platform coordinator can construct an ordinal. It is process-local and is
/// not a persistent identity.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NativeIngressOrdinal(u64);

impl NativeIngressOrdinal {
    pub(super) const fn new(value: u64) -> Self {
        Self(value)
    }

    /// Return the monotonically increasing numeric value.
    pub const fn get(self) -> u64 {
        self.0
    }
}

#[derive(Debug, PartialEq)]
pub(super) enum NativeIngressRecordKind {
    PointerEdge(NativePointerEdge),
    KeyEdge(NativeKeyEdge),
    AccessibilityEdge(NativeAccessibilityEdge),
    CloseObservation(NativePropertyObservation<NativeCloseState>),
    EffectResult(NativeEffectResult),
    PresentationResult(NativePresentationResult),
    Retirement(NativeRetirementTombstone),
    RetirementQuiesced(NativeRetirementQuiesced),
    ViewportCreateResult(NativeViewportCreateResult),
    PlatformSnapshot(NativePlatformSnapshotGeneration),
}

/// The typed fact carried by one ordered native ingress record.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum NativeIngressEvent<'a> {
    /// One physical pointer transition.
    PointerEdge(&'a NativePointerEdge),
    /// One binding-scoped physical keyboard transition.
    KeyEdge(&'a NativeKeyEdge),
    /// One binding-scoped native accessibility action.
    AccessibilityEdge(&'a NativeAccessibilityEdge),
    /// One binding-scoped native close transition.
    CloseObservation(&'a NativePropertyObservation<NativeCloseState>),
    /// The immediate outcome of dispatching a native effect.
    EffectResult(&'a NativeEffectResult),
    /// The renderer outcome for one incarnation-bound presentation.
    PresentationResult(&'a NativePresentationResult),
    /// An exact native viewport retirement.
    Retirement(&'a NativeRetirementTombstone),
    /// Proof that a retired viewport's presentation queue is empty.
    RetirementQuiesced(&'a NativeRetirementQuiesced),
    /// The scheduling result for a logical viewport without a binding.
    ViewportCreateResult(&'a NativeViewportCreateResult),
    /// The complete platform snapshot frozen after all preceding records.
    PlatformSnapshot(NativePlatformSnapshotGeneration),
}

/// One immutable fact in the native ingress total order.
///
/// Records cannot be constructed outside the provider. A backend event
/// sequence, when present, is correlation with the raw event routed into egui;
/// the coordinator-minted [`Self::ordinal`] remains the ordering authority.
#[derive(Debug, PartialEq)]
pub struct NativeIngressRecord {
    ordinal: NativeIngressOrdinal,
    backend_event_sequence: Option<egui::BackendEventSequence>,
    pub(super) kind: NativeIngressRecordKind,
}

/// Provider-validated position of one immutable egui event envelope.
///
/// [`crate::HostedViewportCycle::validate_native_event_envelope_claim`] binds
/// the envelope's process-local identity and viewport to exactly one record in
/// an immutable [`NativeHostIngress`]. Consumers must still inspect
/// [`Self::record`] and require the expected typed event lane before using it as
/// receiver or input authority.
#[derive(Debug)]
pub struct NativeEventEnvelopeReceipt<'a> {
    raw_event_index: usize,
    derivative_ordinal: u32,
    record: &'a NativeIngressRecord,
}

impl<'a> NativeEventEnvelopeReceipt<'a> {
    /// Return the envelope's exact position in the frozen viewport input.
    pub const fn raw_event_index(&self) -> usize {
        self.raw_event_index
    }

    /// Return the derivative order within the correlated backend event.
    pub const fn derivative_ordinal(&self) -> u32 {
        self.derivative_ordinal
    }

    /// Return the provider-owned ingress record that validated the correlation.
    pub const fn record(&self) -> &'a NativeIngressRecord {
        self.record
    }
}

impl NativeIngressRecord {
    pub(super) const fn new(
        ordinal: NativeIngressOrdinal,
        backend_event_sequence: Option<egui::BackendEventSequence>,
        kind: NativeIngressRecordKind,
    ) -> Self {
        Self {
            ordinal,
            backend_event_sequence,
            kind,
        }
    }

    /// Return the coordinator-minted total-order position.
    pub const fn ordinal(&self) -> NativeIngressOrdinal {
        self.ordinal
    }

    /// Return correlation with the originating raw backend event, if any.
    ///
    /// This value is not itself authority. The enclosing record proves that
    /// the provider accepted the correlation at this ordinal.
    pub const fn backend_event_sequence(&self) -> Option<egui::BackendEventSequence> {
        self.backend_event_sequence
    }

    /// Borrow the typed fact carried by this record.
    pub const fn event(&self) -> NativeIngressEvent<'_> {
        match &self.kind {
            NativeIngressRecordKind::PointerEdge(edge) => NativeIngressEvent::PointerEdge(edge),
            NativeIngressRecordKind::KeyEdge(edge) => NativeIngressEvent::KeyEdge(edge),
            NativeIngressRecordKind::AccessibilityEdge(edge) => {
                NativeIngressEvent::AccessibilityEdge(edge)
            }
            NativeIngressRecordKind::CloseObservation(observation) => {
                NativeIngressEvent::CloseObservation(observation)
            }
            NativeIngressRecordKind::EffectResult(result) => {
                NativeIngressEvent::EffectResult(result)
            }
            NativeIngressRecordKind::PresentationResult(result) => {
                NativeIngressEvent::PresentationResult(result)
            }
            NativeIngressRecordKind::Retirement(tombstone) => {
                NativeIngressEvent::Retirement(tombstone)
            }
            NativeIngressRecordKind::RetirementQuiesced(quiesced) => {
                NativeIngressEvent::RetirementQuiesced(quiesced)
            }
            NativeIngressRecordKind::ViewportCreateResult(result) => {
                NativeIngressEvent::ViewportCreateResult(result)
            }
            NativeIngressRecordKind::PlatformSnapshot(generation) => {
                NativeIngressEvent::PlatformSnapshot(*generation)
            }
        }
    }
}

/// One contiguous segment of the coordinator-owned native ingress order.
///
/// Records are in strictly increasing ordinal order and exactly cover
/// `previous + 1 ..= through`. The type has no public constructor, so consumers
/// may treat that relationship as an invariant rather than re-sorting lanes.
#[derive(Debug, PartialEq)]
pub struct NativeIngressJournal {
    previous: NativeIngressOrdinal,
    through: NativeIngressOrdinal,
    records: Vec<NativeIngressRecord>,
}

impl NativeIngressJournal {
    pub(super) const fn new(
        previous: NativeIngressOrdinal,
        through: NativeIngressOrdinal,
        records: Vec<NativeIngressRecord>,
    ) -> Self {
        Self {
            previous,
            through,
            records,
        }
    }

    /// Return the ordinal immediately before this frozen segment.
    pub const fn previous(&self) -> NativeIngressOrdinal {
        self.previous
    }

    /// Return the final ordinal covered by this frozen segment.
    pub const fn through(&self) -> NativeIngressOrdinal {
        self.through
    }

    /// Return every cross-lane fact in coordinator-recorded order.
    pub fn records(&self) -> &[NativeIngressRecord] {
        &self.records
    }
}

/// One atomic native-host ingress batch frozen at a coordinator boundary.
#[derive(Debug, PartialEq)]
pub struct NativeHostIngress {
    pub(super) platform: NativePlatformSnapshot,
    pub(super) ordered: NativeIngressJournal,
    pub(super) pointer_journal: NativePointerJournal,
    pub(super) effect_results: Vec<NativeEffectResult>,
    pub(super) presentation_results: Vec<NativePresentationResult>,
    pub(super) retirement_tombstones: Vec<NativeRetirementTombstone>,
    pub(super) retirement_quiescences: Vec<NativeRetirementQuiesced>,
    pub(super) viewport_create_results: Vec<NativeViewportCreateResult>,
}

impl NativeHostIngress {
    /// Return the complete native platform snapshot.
    pub const fn platform(&self) -> &NativePlatformSnapshot {
        &self.platform
    }

    /// Return the authoritative cross-lane order for this host batch.
    pub const fn ordered(&self) -> &NativeIngressJournal {
        &self.ordered
    }

    /// Consume one affine egui envelope claim and bind it to this provider batch.
    ///
    /// A public [`egui::BackendEventSequence`] is only a correlation claim. This
    /// method returns a receipt only when exactly one coordinator-minted record
    /// in this batch carries that sequence. `Unknown`, absent, and ambiguous
    /// correlations fail closed.
    pub(crate) fn validate_event_envelope_claim(
        &self,
        claim: egui::EventEnvelopeClaim,
    ) -> Option<NativeEventEnvelopeReceipt<'_>> {
        let egui::EventCorrelation::Known {
            sequence,
            derivative_ordinal,
        } = claim.correlation()
        else {
            return None;
        };
        let mut matching = self
            .ordered
            .records()
            .iter()
            .filter(|record| record.backend_event_sequence() == Some(sequence));
        let record = matching.next()?;
        if matching.next().is_some() {
            return None;
        }
        Some(NativeEventEnvelopeReceipt {
            raw_event_index: claim.raw_event_index(),
            derivative_ordinal,
            record,
        })
    }

    /// Return the contiguous native pointer journal segment.
    ///
    /// This compatibility view is derived from [`Self::ordered`].
    pub const fn pointer_journal(&self) -> &NativePointerJournal {
        &self.pointer_journal
    }

    /// Return native effect dispatch results recorded during this cycle.
    ///
    /// This compatibility view is derived from [`Self::ordered`].
    pub fn effect_results(&self) -> &[NativeEffectResult] {
        &self.effect_results
    }

    /// Return incarnation-bound renderer results recorded during this cycle.
    ///
    /// This compatibility view is derived from [`Self::ordered`].
    pub fn presentation_results(&self) -> &[NativePresentationResult] {
        &self.presentation_results
    }

    /// Return viewport retirements recorded during this cycle.
    ///
    /// This compatibility view is derived from [`Self::ordered`].
    pub fn retirement_tombstones(&self) -> &[NativeRetirementTombstone] {
        &self.retirement_tombstones
    }

    /// Return presentation-queue quiescence proofs recorded during this cycle.
    ///
    /// This compatibility view is derived from [`Self::ordered`].
    pub fn retirement_quiescences(&self) -> &[NativeRetirementQuiesced] {
        &self.retirement_quiescences
    }

    /// Return native viewport scheduling results recorded during this cycle.
    ///
    /// This compatibility view is derived from [`Self::ordered`].
    pub fn viewport_create_results(&self) -> &[NativeViewportCreateResult] {
        &self.viewport_create_results
    }
}

pub(super) fn derive_pointer_edges(records: &[NativeIngressRecord]) -> Vec<NativePointerEdge> {
    records
        .iter()
        .filter_map(|record| match &record.kind {
            NativeIngressRecordKind::PointerEdge(edge) => Some(edge.clone()),
            _ => None,
        })
        .collect()
}

pub(super) fn derive_effect_results(records: &[NativeIngressRecord]) -> Vec<NativeEffectResult> {
    records
        .iter()
        .filter_map(|record| match &record.kind {
            NativeIngressRecordKind::EffectResult(result) => Some(result.clone()),
            _ => None,
        })
        .collect()
}

pub(super) fn derive_presentation_results(
    records: &[NativeIngressRecord],
) -> Vec<NativePresentationResult> {
    records
        .iter()
        .filter_map(|record| match &record.kind {
            NativeIngressRecordKind::PresentationResult(result) => Some(result.clone()),
            _ => None,
        })
        .collect()
}

pub(super) fn derive_retirement_tombstones(
    records: &[NativeIngressRecord],
) -> Vec<NativeRetirementTombstone> {
    records
        .iter()
        .filter_map(|record| match &record.kind {
            NativeIngressRecordKind::Retirement(tombstone) => Some(tombstone.clone()),
            _ => None,
        })
        .collect()
}

pub(super) fn derive_retirement_quiescences(
    records: &[NativeIngressRecord],
) -> Vec<NativeRetirementQuiesced> {
    records
        .iter()
        .filter_map(|record| match &record.kind {
            NativeIngressRecordKind::RetirementQuiesced(quiesced) => Some(*quiesced),
            _ => None,
        })
        .collect()
}

pub(super) fn derive_viewport_create_results(
    records: &[NativeIngressRecord],
) -> Vec<NativeViewportCreateResult> {
    records
        .iter()
        .filter_map(|record| match &record.kind {
            NativeIngressRecordKind::ViewportCreateResult(result) => Some(result.clone()),
            _ => None,
        })
        .collect()
}

pub(super) fn pointer_watermark(
    records: &[NativeIngressRecord],
    previous: NativePointerSequence,
) -> NativePointerSequence {
    records
        .iter()
        .filter_map(|record| match &record.kind {
            NativeIngressRecordKind::PointerEdge(edge) => Some(edge.sequence()),
            _ => None,
        })
        .next_back()
        .unwrap_or(previous)
}

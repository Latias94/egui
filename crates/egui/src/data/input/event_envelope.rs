use std::sync::Arc;

use super::Event;

/// A process-local sequence assigned to one backend event before it is routed to a viewport.
///
/// Integrations that own several native windows must mint this from one shared counter. The value
/// is intentionally opaque to egui; it only correlates derivatives and establishes causal order
/// between backend events. It is not a window, viewport, capture, or input-authority token.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BackendEventSequence(u128);

impl BackendEventSequence {
    /// Construct a sequence from an integration-owned, monotonically increasing counter.
    ///
    /// This constructor is public so custom integrations can provide correlation. The resulting
    /// value is a claim, not a capability. A higher-level native runtime must bind it to its
    /// privately minted provider lease, exact window/viewport incarnation, and coordinator-owned
    /// raw event journal before using it as platform authority.
    #[inline]
    pub const fn new(value: u128) -> Self {
        Self(value)
    }

    /// Return the integration-defined sequence value.
    #[inline]
    pub const fn get(self) -> u128 {
        self.0
    }
}

/// Backend correlation attached to an [`EventEnvelope`].
///
/// `Known` means the integration can identify both the backend event and this egui derivative of
/// that event. It proves only that derivative correlation. It does not prove pointer capture,
/// receiver identity, coordinates, window/viewport incarnation, or a global button release.
/// `Unknown` is used for application-injected events and integrations that cannot make the narrow
/// derivative claim.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum EventCorrelation {
    /// No authoritative backend correlation is available.
    #[default]
    Unknown,

    /// The event was derived from an identified backend event.
    Known {
        /// The shared sequence of the source backend event.
        sequence: BackendEventSequence,

        /// Stable zero-based order among egui events derived from the same backend event.
        derivative_ordinal: u32,
    },
}

/// Affine claim over one immutable input envelope in the current egui pass.
///
/// [`crate::InputState::claim_event_envelope`] mints this value at most once for each raw event
/// index. The claim preserves the backend derivative correlation without requiring a consumer to
/// recover it from the mutable, compatibility-only [`crate::InputState::events`] projection.
/// It remains a narrow ordering claim: native window incarnation, receiver, capture, and platform
/// authority must still be proven by the integration that consumes it.
#[derive(Debug)]
pub struct EventEnvelopeClaim {
    raw_event_index: usize,
    correlation: EventCorrelation,
    envelope_identity: Option<Arc<()>>,
}

impl EventEnvelopeClaim {
    pub(crate) const fn new(
        raw_event_index: usize,
        correlation: EventCorrelation,
        envelope_identity: Option<Arc<()>>,
    ) -> Self {
        Self {
            raw_event_index,
            correlation,
            envelope_identity,
        }
    }

    /// Index of the claimed envelope in [`crate::RawInput::events`].
    #[inline]
    pub const fn raw_event_index(&self) -> usize {
        self.raw_event_index
    }

    /// Backend derivative correlation copied from the immutable envelope.
    #[inline]
    pub const fn correlation(&self) -> EventCorrelation {
        self.correlation
    }

    pub(crate) fn matches_envelope(&self, envelope: &EventEnvelope) -> bool {
        self.correlation == envelope.correlation()
            && match (&self.envelope_identity, &envelope.hook_identity) {
                (Some(claimed), Some(candidate)) => Arc::ptr_eq(claimed, candidate),
                _ => false,
            }
    }
}

impl EventCorrelation {
    /// Whether a backend supplied a derivative-correlation claim.
    #[inline]
    pub const fn is_known(self) -> bool {
        matches!(self, Self::Known { .. })
    }

    /// The source backend event sequence, when known.
    #[inline]
    pub const fn sequence(self) -> Option<BackendEventSequence> {
        match self {
            Self::Unknown => None,
            Self::Known { sequence, .. } => Some(sequence),
        }
    }

    /// The derivative ordinal within the source backend event, when known.
    #[inline]
    pub const fn derivative_ordinal(self) -> Option<u32> {
        match self {
            Self::Unknown => None,
            Self::Known {
                derivative_ordinal, ..
            } => Some(derivative_ordinal),
        }
    }
}

/// One egui event together with its backend correlation.
///
/// The event is immutable so application hooks cannot change an event while retaining its backend
/// provenance. Use [`Self::unknown`] for application-generated input. Even a known correlation is
/// not platform authority: higher-level consumers must validate it against a private provider
/// lease/incarnation and a coordinator-owned raw event journal. A private, non-serialized hook
/// identity lets integrations distinguish original envelopes (including `Unknown` envelopes) from
/// input inserted, deleted, duplicated, replaced, or reordered by an application hook.
#[derive(Clone, Debug)]
#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
pub struct EventEnvelope {
    event: Event,

    // Backend correlation is a live observation and must never become authoritative after a
    // serialize/deserialize round trip.
    #[cfg_attr(feature = "serde", serde(skip))]
    correlation: EventCorrelation,

    // A process-local capability used to distinguish every envelope that existed before an input
    // hook ran. Cloning an envelope shares this identity so post-snapshot duplicates are visible.
    #[cfg_attr(feature = "serde", serde(skip))]
    hook_identity: Option<Arc<()>>,
}

impl PartialEq for EventEnvelope {
    fn eq(&self, other: &Self) -> bool {
        self.event == other.event && self.correlation == other.correlation
    }
}

impl EventEnvelope {
    /// Wrap an event without claiming backend provenance.
    #[inline]
    pub fn unknown(event: Event) -> Self {
        Self {
            event,
            correlation: EventCorrelation::Unknown,
            hook_identity: Some(Arc::new(())),
        }
    }

    /// The egui event payload.
    #[inline]
    pub fn event(&self) -> &Event {
        &self.event
    }

    /// Consume the envelope and return its egui event payload.
    #[inline]
    pub fn into_event(self) -> Event {
        self.event
    }

    /// Backend correlation for this event.
    #[inline]
    pub const fn correlation(&self) -> EventCorrelation {
        self.correlation
    }

    pub(crate) fn event_mut(&mut self) -> &mut Event {
        &mut self.event
    }

    fn known(event: Event, sequence: BackendEventSequence, derivative_ordinal: u32) -> Self {
        Self {
            event,
            correlation: EventCorrelation::Known {
                sequence,
                derivative_ordinal,
            },
            hook_identity: Some(Arc::new(())),
        }
    }

    pub(crate) fn has_same_hook_identity(&self, other: &Self) -> bool {
        match (&self.hook_identity, &other.hook_identity) {
            (Some(left), Some(right)) => Arc::ptr_eq(left, right),
            _ => false,
        }
    }

    pub(crate) fn hook_identity_ptr(&self) -> Option<*const ()> {
        self.hook_identity.as_ref().map(Arc::as_ptr)
    }

    pub(crate) fn hook_identity(&self) -> Option<Arc<()>> {
        self.hook_identity.clone()
    }

    pub(crate) fn reset_hook_identity(&mut self) -> *const () {
        let identity = Arc::new(());
        let identity_ptr = Arc::as_ptr(&identity);
        self.hook_identity = Some(identity);
        identity_ptr
    }

    pub(crate) fn forget_correlation(&mut self) {
        self.correlation = EventCorrelation::Unknown;
    }
}

impl From<Event> for EventEnvelope {
    #[inline]
    fn from(event: Event) -> Self {
        Self::unknown(event)
    }
}

/// Assigns stable derivative ordinals to egui events produced from one backend event.
///
/// Create one derivation per backend callback and use [`Self::envelope`] for every egui event it
/// produces. A derivation created with [`Self::unknown`] explicitly makes no correlation claim.
#[derive(Debug)]
pub struct BackendEventDerivation {
    sequence: Option<BackendEventSequence>,
    next_derivative_ordinal: Option<u32>,
}

impl BackendEventDerivation {
    /// Start a derivation for a sequenced backend event.
    ///
    /// This public constructor lets integrations state correlation. It does not mint platform
    /// authority; consumers must validate the resulting envelopes at their private runtime
    /// boundary.
    #[inline]
    pub const fn known(sequence: BackendEventSequence) -> Self {
        Self {
            sequence: Some(sequence),
            next_derivative_ordinal: Some(0),
        }
    }

    /// Start a derivation for events whose backend origin is unknown.
    #[inline]
    pub const fn unknown() -> Self {
        Self {
            sequence: None,
            next_derivative_ordinal: None,
        }
    }

    /// Wrap the next egui event derived from this backend event.
    pub fn envelope(&mut self, event: Event) -> EventEnvelope {
        let Some(sequence) = self.sequence else {
            return EventEnvelope::unknown(event);
        };
        let Some(derivative_ordinal) = self.next_derivative_ordinal else {
            return EventEnvelope::unknown(event);
        };

        self.next_derivative_ordinal = derivative_ordinal.checked_add(1);
        EventEnvelope::known(event, sequence, derivative_ordinal)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Context, Key, Modifiers, RawInput};

    fn pressed_key(key: Key) -> Event {
        Event::Key {
            key,
            physical_key: Some(key),
            pressed: true,
            repeat: false,
            modifiers: Modifiers::NONE,
        }
    }

    #[test]
    fn derivatives_share_sequence_and_have_stable_ordinals() {
        let sequence = BackendEventSequence::new(41);
        let mut derivation = BackendEventDerivation::known(sequence);

        let first = derivation.envelope(Event::Copy);
        let second = derivation.envelope(Event::Cut);

        assert_eq!(first.correlation().sequence(), Some(sequence));
        assert_eq!(first.correlation().derivative_ordinal(), Some(0));
        assert_eq!(second.correlation().sequence(), Some(sequence));
        assert_eq!(second.correlation().derivative_ordinal(), Some(1));
    }

    #[test]
    fn unknown_derivation_never_claims_backend_provenance() {
        let mut derivation = BackendEventDerivation::unknown();
        let event = derivation.envelope(Event::Copy);

        assert_eq!(event.correlation(), EventCorrelation::Unknown);
    }

    #[test]
    fn duplicate_envelopes_are_claimed_once_in_backend_order() {
        let sequence = BackendEventSequence::new(91);
        let mut derivation = BackendEventDerivation::known(sequence);
        let context = Context::default();
        let mut claims = Vec::new();

        let _ = context.run_ui(
            RawInput {
                events: vec![
                    derivation.envelope(pressed_key(Key::Enter)),
                    derivation.envelope(pressed_key(Key::Enter)),
                ],
                ..RawInput::default()
            },
            |ui| {
                for _ in 0..3 {
                    claims.push(ui.input_mut(|input| {
                        input.claim_event_envelope(|event| {
                            matches!(
                                event,
                                Event::Key {
                                    key: Key::Enter,
                                    pressed: true,
                                    ..
                                }
                            )
                        })
                    }));
                }
            },
        );

        let first = claims[0]
            .as_ref()
            .expect("the first derivative is claimable");
        let second = claims[1]
            .as_ref()
            .expect("the duplicate derivative remains independently claimable");
        assert_eq!(first.raw_event_index(), 0);
        assert_eq!(second.raw_event_index(), 1);
        assert_eq!(first.correlation().sequence(), Some(sequence));
        assert_eq!(first.correlation().derivative_ordinal(), Some(0));
        assert_eq!(second.correlation().sequence(), Some(sequence));
        assert_eq!(second.correlation().derivative_ordinal(), Some(1));
        assert!(claims[2].is_none());
    }

    #[test]
    fn unknown_envelope_is_still_consumed_affinely() {
        let context = Context::default();
        let mut claims = Vec::new();
        let _ = context.run_ui(
            RawInput {
                events: vec![EventEnvelope::unknown(pressed_key(Key::Space))],
                ..RawInput::default()
            },
            |ui| {
                for _ in 0..2 {
                    claims.push(ui.input_mut(|input| {
                        input.claim_event_envelope(|event| {
                            matches!(
                                event,
                                Event::Key {
                                    key: Key::Space,
                                    pressed: true,
                                    ..
                                }
                            )
                        })
                    }));
                }
            },
        );

        assert_eq!(
            claims[0].as_ref().map(EventEnvelopeClaim::correlation),
            Some(EventCorrelation::Unknown)
        );
        assert!(claims[1].is_none());
    }
}

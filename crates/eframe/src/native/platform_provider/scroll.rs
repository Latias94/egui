//! Lossless native scroll samples before framework aggregation.

use super::authority::{NativeAuthority, NativePointerDeviceId};

/// Provider-owned identity of one phaseful native scroll sequence.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NativeScrollSequenceToken(u64);

impl NativeScrollSequenceToken {
    pub(crate) const fn new(value: u64) -> Self {
        Self(value)
    }

    /// Return the provider-owned monotonic representation.
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// A finite two-axis native scroll vector.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct NativeFiniteScrollVector {
    x_bits: u64,
    y_bits: u64,
}

impl NativeFiniteScrollVector {
    pub(crate) fn new(x: f64, y: f64) -> Option<Self> {
        if !x.is_finite() || !y.is_finite() {
            return None;
        }
        Some(Self {
            x_bits: canonical_component(x).to_bits(),
            y_bits: canonical_component(y).to_bits(),
        })
    }

    /// Return horizontal content movement.
    pub const fn x(self) -> f64 {
        f64::from_bits(self.x_bits)
    }

    /// Return vertical content movement.
    pub const fn y(self) -> f64 {
        f64::from_bits(self.y_bits)
    }
}

const fn canonical_component(value: f64) -> f64 {
    if value == 0.0 { 0.0 } else { value }
}

/// Raw unit reported by the native backend.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum NativeScrollDelta {
    /// Native physical pixels. The runtime binds these to a core coordinate generation.
    PhysicalPixels(NativeFiniteScrollVector),
    /// Platform-defined line units.
    Lines(NativeFiniteScrollVector),
}

impl NativeScrollDelta {
    /// Return the raw content-movement vector.
    pub const fn vector(self) -> NativeFiniteScrollVector {
        match self {
            Self::PhysicalPixels(vector) | Self::Lines(vector) => vector,
        }
    }
}

/// Why a native smooth-scroll sequence was cancelled.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum NativeScrollCancelReason {
    /// The platform explicitly cancelled the gesture.
    PlatformCancelled,
    /// The physical device retired.
    DeviceRemoved,
}

/// Native phase retained without inventing timer-based terminality.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum NativeScrollPhase {
    /// One independent wheel sample with no provider sequence.
    Discrete,
    /// Start a provider-defined smooth sequence.
    Begin,
    /// Continue the active provider-defined sequence.
    Update,
    /// Apply an optional final sample and terminate the sequence.
    End,
    /// Terminate without applying a sample.
    Cancel(NativeScrollCancelReason),
}

/// Whether the platform identified a sample as direct input or momentum.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum NativeScrollMomentum {
    /// Direct user-controlled movement.
    Direct,
    /// Platform-generated momentum.
    Momentum,
}

/// Event-time keyboard modifiers retained with one scroll edge.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct NativeScrollModifiers {
    shift: bool,
    control: bool,
    alt: bool,
    command: bool,
}

impl NativeScrollModifiers {
    pub(crate) const fn new(shift: bool, control: bool, alt: bool, command: bool) -> Self {
        Self {
            shift,
            control,
            alt,
            command,
        }
    }

    /// Return whether Shift was pressed.
    pub const fn shift(self) -> bool {
        self.shift
    }

    /// Return whether Control was pressed.
    pub const fn control(self) -> bool {
        self.control
    }

    /// Return whether Alt was pressed.
    pub const fn alt(self) -> bool {
        self.alt
    }

    /// Return whether the platform command modifier was pressed.
    pub const fn command(self) -> bool {
        self.command
    }
}

/// One lossless native scroll sample carried by the global pointer journal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NativeScrollEdge {
    device: NativePointerDeviceId,
    sequence: Option<NativeScrollSequenceToken>,
    phase: NativeScrollPhase,
    delta: Option<NativeScrollDelta>,
    momentum: NativeAuthority<NativeScrollMomentum>,
    modifiers: NativeAuthority<NativeScrollModifiers>,
}

impl NativeScrollEdge {
    pub(crate) fn new(
        device: NativePointerDeviceId,
        sequence: Option<NativeScrollSequenceToken>,
        phase: NativeScrollPhase,
        delta: Option<NativeScrollDelta>,
        momentum: NativeAuthority<NativeScrollMomentum>,
        modifiers: NativeAuthority<NativeScrollModifiers>,
    ) -> Option<Self> {
        let legal_shape = match phase {
            NativeScrollPhase::Discrete => sequence.is_none() && delta.is_some(),
            NativeScrollPhase::Begin => sequence.is_some(),
            NativeScrollPhase::Update => sequence.is_some() && delta.is_some(),
            NativeScrollPhase::End => sequence.is_some(),
            NativeScrollPhase::Cancel(_) => sequence.is_some() && delta.is_none(),
        };
        legal_shape.then_some(Self {
            device,
            sequence,
            phase,
            delta,
            momentum,
            modifiers,
        })
    }

    /// Return the exact physical device that produced the sample.
    pub const fn device(&self) -> NativePointerDeviceId {
        self.device
    }

    /// Return the provider sequence token for a phaseful gesture.
    pub const fn sequence(&self) -> Option<NativeScrollSequenceToken> {
        self.sequence
    }

    /// Return the native phase.
    pub const fn phase(&self) -> NativeScrollPhase {
        self.phase
    }

    /// Return the raw delta and unit, when this phase carries a sample.
    pub const fn delta(&self) -> Option<NativeScrollDelta> {
        self.delta
    }

    /// Return momentum authority retained from the backend.
    pub const fn momentum(&self) -> &NativeAuthority<NativeScrollMomentum> {
        &self.momentum
    }

    /// Return event-time modifier authority.
    pub const fn modifiers(&self) -> &NativeAuthority<NativeScrollModifiers> {
        &self.modifiers
    }
}

//! Opaque identities and native fact authority.

/// Why the native integration cannot authoritatively report a fact.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NativeUnavailableReason {
    /// The backend has not observed the fact yet.
    NotObserved,
    /// The platform API cannot prove the fact.
    Unsupported,
    /// The fact was intentionally withheld because its source is stale.
    StaleSource,
    /// The relevant native object no longer exists.
    Retired,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum NativeAuthorityState<T> {
    Known(T),
    Unknown(NativeUnavailableReason),
}

/// A fact that is either known from the native backend or explicitly unavailable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NativeAuthority<T>(NativeAuthorityState<T>);

impl<T> NativeAuthority<T> {
    pub(crate) fn known(value: T) -> Self {
        Self(NativeAuthorityState::Known(value))
    }

    pub(crate) fn unknown(reason: NativeUnavailableReason) -> Self {
        Self(NativeAuthorityState::Unknown(reason))
    }

    /// Return the authoritative value, if one is available.
    pub fn value(&self) -> Option<&T> {
        match &self.0 {
            NativeAuthorityState::Known(value) => Some(value),
            NativeAuthorityState::Unknown(_) => None,
        }
    }

    /// Return why this fact is unavailable.
    pub fn unavailable_reason(&self) -> Option<NativeUnavailableReason> {
        match self.0 {
            NativeAuthorityState::Known(_) => None,
            NativeAuthorityState::Unknown(reason) => Some(reason),
        }
    }

    /// Return whether the backend authoritatively knows this fact.
    pub fn is_known(&self) -> bool {
        matches!(self.0, NativeAuthorityState::Known(_))
    }
}

macro_rules! private_counter {
    ($name:ident, $doc:literal) => {
        #[doc = $doc]
        #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(u64);

        impl $name {
            pub(crate) const fn new(value: u64) -> Self {
                Self(value)
            }

            /// Return the monotonically increasing numeric value.
            pub const fn get(self) -> u64 {
                self.0
            }
        }
    };
}

private_counter!(
    NativeViewportIncarnation,
    "A coordinator-minted lifetime of one native viewport binding."
);
private_counter!(
    NativePlatformGeneration,
    "A monotonic generation value for a native platform authority lane."
);
private_counter!(
    NativeObservationGeneration,
    "The generation of one binding and property observation lane."
);
private_counter!(
    NativePointerSequence,
    "The total-order sequence of one native pointer edge."
);
private_counter!(
    NativePointerDeviceId,
    "The backend-assigned identity of one physical pointer device."
);
private_counter!(
    NativePointerId,
    "The backend-assigned identity of one pointer stream on a device."
);
private_counter!(
    NativeEffectRequestId,
    "The coordinator-minted identity of one native effect request."
);
private_counter!(
    NativeViewportCreateRequestId,
    "The coordinator-minted identity of one native viewport creation request."
);

/// The exact lifetime-bound identity of an egui viewport's native object.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NativeViewportBinding {
    pub(super) viewport_id: egui::ViewportId,
    pub(super) incarnation: NativeViewportIncarnation,
}

impl NativeViewportBinding {
    pub(super) const fn new(
        viewport_id: egui::ViewportId,
        incarnation: NativeViewportIncarnation,
    ) -> Self {
        Self {
            viewport_id,
            incarnation,
        }
    }

    /// Return the logical egui viewport identifier.
    pub const fn viewport_id(self) -> egui::ViewportId {
        self.viewport_id
    }

    /// Return the exact native lifetime of the viewport.
    pub const fn incarnation(self) -> NativeViewportIncarnation {
        self.incarnation
    }
}

/// A point in desktop-global physical pixels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NativePhysicalPoint {
    x: i32,
    y: i32,
}

impl NativePhysicalPoint {
    /// Construct a point in desktop-global physical pixels.
    pub const fn new(x: i32, y: i32) -> Self {
        Self { x, y }
    }

    /// Return the horizontal physical-pixel coordinate.
    pub const fn x(self) -> i32 {
        self.x
    }

    /// Return the vertical physical-pixel coordinate.
    pub const fn y(self) -> i32 {
        self.y
    }
}

/// A rectangle in desktop-global physical pixels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NativePhysicalRect {
    min: NativePhysicalPoint,
    max: NativePhysicalPoint,
}

impl NativePhysicalRect {
    /// Construct a rectangle from its physical-pixel corners.
    pub const fn new(min: NativePhysicalPoint, max: NativePhysicalPoint) -> Self {
        Self { min, max }
    }

    /// Return the minimum corner.
    pub const fn min(self) -> NativePhysicalPoint {
        self.min
    }

    /// Return the maximum corner.
    pub const fn max(self) -> NativePhysicalPoint {
        self.max
    }
}

/// The native owner under the pointer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NativeHoveredWindow {
    /// The pointer is over an owned viewport.
    Viewport(NativeViewportBinding),
    /// The pointer is over a foreign native window.
    Foreign,
    /// The pointer is over no native window.
    None,
}

/// The current native pointer-capture owner.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NativeCaptureOwner {
    /// An owned viewport has capture.
    Viewport(NativeViewportBinding),
    /// A foreign native window has capture.
    Foreign,
    /// No native window has capture.
    None,
}

/// The exact endpoint which delivered one native pointer edge.
///
/// Delivery is edge-local. It does not imply persistent capture or identify
/// the window currently under the pointer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NativePointerDeliveryOwner {
    /// An exact owned viewport incarnation delivered the edge.
    Viewport(NativeViewportBinding),
    /// A native endpoint outside this integration delivered the edge.
    Foreign,
    /// The edge had no native-window delivery endpoint.
    None,
}

/// The current native keyboard-focus owner.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NativeFocusedWindow {
    /// An owned viewport has focus.
    Viewport(NativeViewportBinding),
    /// A foreign native window has focus.
    Foreign,
    /// No native window has focus.
    None,
}

/// The presentation state observed for one native viewport.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NativePresentationState {
    /// The viewport is visible and not minimized.
    Visible,
    /// The viewport is hidden.
    Hidden,
    /// The viewport is minimized.
    Minimized,
}

/// Whether one native viewport accepts pointer input.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NativePointerInputState {
    /// Pointer input reaches the viewport.
    ReceivesInput,
    /// Pointer input passes through the viewport.
    PassThrough,
}

/// The native close and destruction state of one viewport.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NativeCloseState {
    /// The viewport is live and has no pending native close request.
    LiveClear,
    /// The platform reported a close request that has not been resolved.
    LiveRequested,
    /// The native viewport was destroyed.
    Destroyed,
}

/// A property lane on which an effect and later observation are correlated.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum NativeEffectProperty {
    /// Visibility and minimization state.
    Presentation,
    /// Pointer hit-testing state.
    PointerInput,
    /// Keyboard focus ownership.
    Focus,
    /// Native outer geometry.
    Geometry,
    /// Native close request state.
    Close,
    /// Native object creation or destruction.
    Lifecycle,
}

/// A platform-provider invariant violation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NativePlatformError {
    /// The viewport already has an active native binding.
    ViewportAlreadyRegistered,
    /// The binding was never minted by this coordinator.
    UnknownBinding,
    /// The exact binding has already retired.
    RetiredBinding,
    /// The effect request is not pending.
    UnknownEffectRequest,
    /// The reported request does not match the pending lane entry.
    EffectRequestMismatch,
    /// An observation attempted to acknowledge an effect before dispatch.
    EffectNotDispatched,
    /// A dispatch result was already recorded for this effect request.
    EffectDispatchAlreadyRecorded,
    /// A logical viewport already has an unfinished creation request.
    ViewportCreateLaneBusy,
    /// The viewport creation request is not pending.
    UnknownViewportCreateRequest,
    /// The reported viewport creation request does not match its pending lane.
    ViewportCreateRequestMismatch,
    /// The renderer result belongs to another logical viewport.
    PresentationViewportMismatch,
    /// The presentation ticket is not outstanding on its exact binding.
    UnknownPresentationTicket,
    /// A window snapshot contains an observation for another binding or property lane.
    WindowSnapshotMismatch,
    /// The current cycle does not contain exactly one snapshot per active binding.
    IncompletePlatformRoster,
    /// A previously prepared host-ingress batch has not reached its settlement boundary.
    HostIngressInFlight,
    /// A fatal hosted-cycle abort made the native ingress provider unusable.
    HostIngressPoisoned,
    /// A host-ingress settlement ticket does not name the prepared batch.
    HostIngressSettlementMismatch,
    /// A monotonically increasing identity was exhausted.
    CounterExhausted,
}

impl std::fmt::Display for NativePlatformError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::ViewportAlreadyRegistered => "viewport already has an active native binding",
            Self::UnknownBinding => "native viewport binding was not minted by this coordinator",
            Self::RetiredBinding => "native viewport binding has retired",
            Self::UnknownEffectRequest => "native effect request is not pending",
            Self::EffectRequestMismatch => "native effect request does not match its pending lane",
            Self::EffectNotDispatched => "native effect has not been dispatched",
            Self::EffectDispatchAlreadyRecorded => {
                "native effect dispatch result was already recorded"
            }
            Self::ViewportCreateLaneBusy => "native viewport creation lane is busy",
            Self::UnknownViewportCreateRequest => "native viewport creation request is not pending",
            Self::ViewportCreateRequestMismatch => {
                "native viewport creation request does not match its pending lane"
            }
            Self::PresentationViewportMismatch => {
                "renderer result does not match the presentation ticket viewport"
            }
            Self::UnknownPresentationTicket => {
                "native presentation ticket is not outstanding on its exact binding"
            }
            Self::WindowSnapshotMismatch => {
                "native window snapshot contains mismatched binding or property authority"
            }
            Self::IncompletePlatformRoster => {
                "native platform snapshot does not exactly cover the active roster"
            }
            Self::HostIngressInFlight => {
                "a native host-ingress batch is already awaiting settlement"
            }
            Self::HostIngressPoisoned => "a fatal hosted-cycle abort poisoned native host ingress",
            Self::HostIngressSettlementMismatch => {
                "native host-ingress settlement does not match the prepared batch"
            }
            Self::CounterExhausted => "native platform monotonic counter was exhausted",
        })
    }
}

impl std::error::Error for NativePlatformError {}

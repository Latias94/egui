//! Device-aware, globally ordered native pointer facts.

use super::authority::{
    NativeAuthority, NativeCaptureOwner, NativeHoveredWindow, NativePhysicalPoint,
    NativePointerDeliveryOwner, NativePointerDeviceId, NativePointerId, NativePointerSequence,
    NativeViewportBinding,
};
use super::scroll::NativeScrollEdge;
use super::work_area::NativeWorkAreaRoute;

/// Which native receiver delivered a pointer edge.
///
/// `Viewport` is an edge-local receiver proof minted from the actual native
/// window callback. It does not imply persistent pointer capture or hover.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NativePointerSource {
    /// The event came from an owned viewport.
    Viewport(NativeViewportBinding),
    /// The event came from a foreign native source.
    Foreign,
    /// The event has no native window source.
    None,
}

/// The exact device and pointer stream that produced an input edge.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NativePointerIdentity {
    device_id: NativePointerDeviceId,
    pointer_id: NativePointerId,
}

/// Event-time coordinates of one exact hovered native viewport.
///
/// The capture is frozen synchronously with its pointer edge. A consumer must
/// match [`Self::binding`] and the edge's desktop position before using it; a
/// later platform snapshot is not a substitute for this fact.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct NativePointerCoordinateCapture {
    pub(super) binding: NativeViewportBinding,
    pub(super) content_origin: NativePhysicalPoint,
    pub(super) native_scale_factor: f64,
    pub(super) presentation_scale_factor: f64,
}

impl NativePointerCoordinateCapture {
    pub(crate) const fn new(
        binding: NativeViewportBinding,
        content_origin: NativePhysicalPoint,
        native_scale_factor: f64,
        presentation_scale_factor: f64,
    ) -> Self {
        Self {
            binding,
            content_origin,
            native_scale_factor,
            presentation_scale_factor,
        }
    }

    /// Return the exact hovered native viewport incarnation.
    pub const fn binding(self) -> NativeViewportBinding {
        self.binding
    }

    /// Return the event-time desktop origin of the viewport content area.
    pub const fn content_origin(self) -> NativePhysicalPoint {
        self.content_origin
    }

    /// Return the event-time operating-system pixel scale.
    pub const fn native_scale_factor(self) -> f64 {
        self.native_scale_factor
    }

    /// Return the event-time physical-pixel scale of the presented egui coordinate system.
    pub const fn presentation_scale_factor(self) -> f64 {
        self.presentation_scale_factor
    }
}

impl NativePointerIdentity {
    pub(crate) const fn new(device_id: NativePointerDeviceId, pointer_id: NativePointerId) -> Self {
        Self {
            device_id,
            pointer_id,
        }
    }

    /// Return the backend-assigned physical device identity.
    pub const fn device_id(self) -> NativePointerDeviceId {
        self.device_id
    }

    /// Return the pointer stream identity within the device.
    pub const fn pointer_id(self) -> NativePointerId {
        self.pointer_id
    }
}

/// A native pointer button identity.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NativePointerButton {
    /// Primary pointer button.
    Primary,
    /// Secondary pointer button.
    Secondary,
    /// Middle pointer button.
    Middle,
    /// Browser-style back button.
    Back,
    /// Browser-style forward button.
    Forward,
    /// A backend-defined additional button.
    Other(u16),
}

/// The physical transition represented by a pointer journal entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NativePointerEdgeKind {
    /// The pointer moved.
    Moved,
    /// A button was pressed.
    ButtonPressed(NativePointerButton),
    /// A button was released.
    ButtonReleased(NativePointerButton),
    /// Native capture ownership changed.
    CaptureChanged,
    /// The platform authoritatively cancelled the pointer stream.
    Cancelled,
    /// One lossless scroll sample before framework aggregation.
    Scrolled(NativeScrollEdge),
}

/// One globally ordered native pointer transition.
#[derive(Clone, Debug, PartialEq)]
pub struct NativePointerEdge {
    pub(super) sequence: NativePointerSequence,
    pub(super) source: NativePointerSource,
    pub(super) delivery_owner: NativeAuthority<NativePointerDeliveryOwner>,
    pub(super) identity: NativePointerIdentity,
    pub(super) kind: NativePointerEdgeKind,
    pub(super) stream_terminal: bool,
    pub(super) position: NativeAuthority<NativePhysicalPoint>,
    pub(super) hovered: NativeAuthority<NativeHoveredWindow>,
    pub(super) hovered_coordinates: NativeAuthority<NativePointerCoordinateCapture>,
    pub(super) delivery_coordinates: NativeAuthority<NativePointerCoordinateCapture>,
    pub(super) capture: NativeAuthority<NativeCaptureOwner>,
    pub(super) work_area: NativeAuthority<NativeWorkAreaRoute>,
}

impl NativePointerEdge {
    /// Return the total-order sequence.
    pub const fn sequence(&self) -> NativePointerSequence {
        self.sequence
    }

    /// Return the native receiver that delivered this event.
    pub const fn source(&self) -> NativePointerSource {
        self.source
    }

    /// Return the edge-local native receiver proof.
    ///
    /// This aliases [`Self::source`] while making the distinction from the
    /// separate persistent [`Self::capture`] authority explicit to consumers.
    pub const fn receiver(&self) -> NativePointerSource {
        self.source
    }

    /// Return the exact edge-local native delivery endpoint.
    pub const fn delivery_owner(&self) -> &NativeAuthority<NativePointerDeliveryOwner> {
        &self.delivery_owner
    }

    /// Return the exact device and pointer stream identity.
    pub const fn identity(&self) -> NativePointerIdentity {
        self.identity
    }

    /// Return the physical transition.
    pub const fn kind(&self) -> NativePointerEdgeKind {
        self.kind
    }

    /// Return whether this edge normally terminates an ephemeral pointer stream.
    pub const fn ends_stream(&self) -> bool {
        self.stream_terminal
    }

    /// Return desktop-global physical position authority.
    pub const fn position(&self) -> &NativeAuthority<NativePhysicalPoint> {
        &self.position
    }

    /// Return exact hovered-window authority at this edge.
    pub const fn hovered(&self) -> &NativeAuthority<NativeHoveredWindow> {
        &self.hovered
    }

    /// Return event-time coordinates for the exact hovered viewport.
    pub const fn hovered_coordinates(&self) -> &NativeAuthority<NativePointerCoordinateCapture> {
        &self.hovered_coordinates
    }

    /// Return event-time coordinates for the exact delivery viewport.
    pub const fn delivery_coordinates(&self) -> &NativeAuthority<NativePointerCoordinateCapture> {
        &self.delivery_coordinates
    }

    /// Return exact capture authority at this edge.
    pub const fn capture(&self) -> &NativeAuthority<NativeCaptureOwner> {
        &self.capture
    }

    /// Return the event-time work-area selection for an explicit no-window route.
    pub const fn work_area(&self) -> &NativeAuthority<NativeWorkAreaRoute> {
        &self.work_area
    }
}

/// A contiguous batch of the native pointer journal.
#[derive(Clone, Debug, PartialEq)]
pub struct NativePointerJournal {
    pub(super) previous: NativePointerSequence,
    pub(super) through: NativePointerSequence,
    pub(super) edges: Vec<NativePointerEdge>,
}

impl NativePointerJournal {
    /// Return the sequence immediately before this batch.
    pub const fn previous(&self) -> NativePointerSequence {
        self.previous
    }

    /// Return the last sequence covered by this batch.
    pub const fn through(&self) -> NativePointerSequence {
        self.through
    }

    /// Return the ordered pointer transitions.
    pub fn edges(&self) -> &[NativePointerEdge] {
        &self.edges
    }
}

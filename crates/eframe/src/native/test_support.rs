//! Deterministic native ingress support used only by fork integration tests.

use winit::event_loop::{EventLoopClosed, EventLoopProxy};

use super::winit_integration::UserEvent;

/// A high-level scroll delta accepted by the native test driver.
///
/// This test-only value carries no viewport binding, provider identity, or
/// ingress position. The native event loop resolves those authoritative facts
/// when it consumes the enclosing pointer event.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum NativeTestScrollDelta {
    /// Platform-defined line units.
    Lines {
        /// Horizontal content movement.
        x: f32,
        /// Vertical content movement.
        y: f32,
    },
    /// Native physical pixels.
    PhysicalPixels {
        /// Horizontal content movement.
        x: f64,
        /// Vertical content movement.
        y: f64,
    },
}

impl NativeTestScrollDelta {
    /// Creates a line-unit scroll delta.
    #[must_use]
    pub const fn lines(x: f32, y: f32) -> Self {
        Self::Lines { x, y }
    }

    /// Creates a physical-pixel scroll delta.
    #[must_use]
    pub const fn physical_pixels(x: f64, y: f64) -> Self {
        Self::PhysicalPixels { x, y }
    }
}

/// A high-level pointer action accepted by the native test driver.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum NativeTestPointerAction {
    /// Move the synthetic pointer without changing its primary-button state.
    Move,
    /// Press the synthetic primary pointer button.
    PrimaryPressed,
    /// Release the synthetic primary pointer button.
    PrimaryReleased,
    /// Deliver one independent wheel sample without inventing a smooth-scroll sequence.
    Scroll(NativeTestScrollDelta),
}

/// The destination of one deterministic native pointer edge.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum NativeTestPointerLocation {
    /// A logical point within one named owned viewport.
    Viewport {
        /// The viewport that received the edge.
        viewport: egui::ViewportId,
        /// The point in the viewport's presented egui coordinates.
        ///
        /// The event loop snaps this request to the nearest representable physical pixel and
        /// probes the presented graph at the corresponding snapped logical point.
        position: egui::Pos2,
    },
    /// A logical point within the sole live non-root viewport.
    ///
    /// The event loop rejects this selector unless exactly one child viewport
    /// is currently live. It avoids leaking a fork binding or a dynamically
    /// assigned viewport identifier into the test application.
    UniqueChild {
        /// The point in the child's presented egui coordinates, snapped as documented above.
        position: egui::Pos2,
    },
    /// A desktop-global physical point outside every owned viewport.
    ///
    /// The event loop validates the exact outside-all proof against its current
    /// native roster. Non-integral or covered points are rejected.
    OutsideAll {
        /// The desktop-global physical point, represented exactly as integral `f32` values.
        desktop_position: egui::Pos2,
    },
}

/// One viewport-local pointer event for the deterministic native test driver.
///
/// The event-loop implementation resolves the current viewport binding,
/// presented hit graph, scale, desktop position, and work-area authority at
/// delivery time. It rejects missing or stale facts rather than accepting a
/// caller-supplied native binding or receipt.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct NativeTestPointerEvent {
    location: NativeTestPointerLocation,
    action: NativeTestPointerAction,
}

impl NativeTestPointerEvent {
    /// Creates one synthetic pointer event at an exact logical point of a live viewport.
    #[must_use]
    pub const fn new(
        viewport: egui::ViewportId,
        position: egui::Pos2,
        action: NativeTestPointerAction,
    ) -> Self {
        Self {
            location: NativeTestPointerLocation::Viewport { viewport, position },
            action,
        }
    }

    /// Creates one pointer event at a desktop-global point outside every owned viewport.
    #[must_use]
    pub const fn outside_all(
        desktop_position: egui::Pos2,
        action: NativeTestPointerAction,
    ) -> Self {
        Self {
            location: NativeTestPointerLocation::OutsideAll { desktop_position },
            action,
        }
    }

    /// Creates one pointer event in the sole currently live child viewport.
    #[must_use]
    pub const fn unique_child(position: egui::Pos2, action: NativeTestPointerAction) -> Self {
        Self {
            location: NativeTestPointerLocation::UniqueChild { position },
            action,
        }
    }

    /// Returns the event location that the native event loop will validate.
    #[must_use]
    pub const fn location(self) -> NativeTestPointerLocation {
        self.location
    }

    /// Returns the requested pointer action.
    #[must_use]
    pub const fn action(self) -> NativeTestPointerAction {
        self.action
    }
}

/// Event-loop proxy for deterministic native ingress tests.
///
/// This type exists only with the default-disabled `native-test-support`
/// feature. It does not expose native bindings, ingress ordinals, platform
/// receipts, or docking state. The event loop remains the sole authority that
/// resolves those facts before forwarding the action through normal ingress.
#[derive(Clone)]
pub struct NativeTestDriver {
    event_loop: EventLoopProxy<UserEvent>,
}

impl NativeTestDriver {
    pub(crate) const fn new(event_loop: EventLoopProxy<UserEvent>) -> Self {
        Self { event_loop }
    }

    /// Enqueues one deterministic pointer event on the native event loop.
    ///
    /// An event-loop closure means the test runtime has already terminated;
    /// no input is applied in that case.
    pub fn send_pointer(
        &self,
        event: NativeTestPointerEvent,
    ) -> Result<(), EventLoopClosed<UserEvent>> {
        self.event_loop
            .send_event(UserEvent::NativeTestPointer(event))
    }
}

impl std::fmt::Debug for NativeTestDriver {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("NativeTestDriver")
            .finish_non_exhaustive()
    }
}

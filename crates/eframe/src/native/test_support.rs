//! Deterministic native ingress support used only by fork integration tests.

use winit::dpi::PhysicalPosition;
use winit::event::{DeviceId, MouseScrollDelta, TouchPhase, WindowEvent};
use winit::event_loop::{EventLoopClosed, EventLoopProxy};

use super::winit_integration::UserEvent;

/// A high-level scroll delta accepted by the native test driver.
///
/// This test-only value carries no viewport binding, provider identity, or
/// ingress position. The native event loop resolves those authoritative facts
/// when it consumes the enclosing window event.
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

/// One deterministic wheel event delivered through the production winit window-event path.
///
/// The caller names only an egui viewport, an event-time viewport-local physical position, and
/// the platform delta. The event loop resolves the current native window and mints the backend
/// sequence. Binding, presentation, receiver, derivative, and docking authority remain owned by
/// the production provider pipeline.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct NativeTestWindowScroll {
    viewport: egui::ViewportId,
    position: [f64; 2],
    delta: NativeTestScrollDelta,
}

impl NativeTestWindowScroll {
    /// Creates a wheel event at one exact viewport-local physical position.
    #[must_use]
    pub const fn new(
        viewport: egui::ViewportId,
        position: [f64; 2],
        delta: NativeTestScrollDelta,
    ) -> Self {
        Self {
            viewport,
            position,
            delta,
        }
    }

    /// Returns the viewport whose current native window must receive the event.
    #[must_use]
    pub const fn viewport(self) -> egui::ViewportId {
        self.viewport
    }

    pub(crate) fn into_window_event(self) -> WindowEvent {
        let delta = match self.delta {
            NativeTestScrollDelta::Lines { x, y } => MouseScrollDelta::LineDelta(x, y),
            NativeTestScrollDelta::PhysicalPixels { x, y } => {
                MouseScrollDelta::PixelDelta(PhysicalPosition::new(x, y))
            }
        };
        // Winit reserves this identity for tests. It is only a stable device key for the normal
        // eframe input pipeline and is never passed to an operating-system API.
        let device_id = DeviceId::dummy();
        WindowEvent::MouseWheel {
            device_id,
            delta,
            phase: TouchPhase::Moved,
            position: Some(PhysicalPosition::new(self.position[0], self.position[1])),
        }
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

    /// Enqueues one wheel event through the production winit window-event path.
    pub fn send_window_scroll(
        &self,
        event: NativeTestWindowScroll,
    ) -> Result<(), EventLoopClosed<UserEvent>> {
        self.event_loop
            .send_event(UserEvent::NativeTestWindowScroll(event))
    }
}

impl std::fmt::Debug for NativeTestDriver {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("NativeTestDriver")
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_scroll_preserves_event_position_and_physical_delta() {
        let event = NativeTestWindowScroll::new(
            egui::ViewportId::ROOT,
            [31.0, 47.0],
            NativeTestScrollDelta::physical_pixels(-3.5, 4.75),
        )
        .into_window_event();

        let WindowEvent::MouseWheel {
            delta: MouseScrollDelta::PixelDelta(delta),
            phase,
            position: Some(position),
            ..
        } = event
        else {
            panic!("test scroll must become one positioned pixel wheel event");
        };
        assert_eq!(delta, PhysicalPosition::new(-3.5, 4.75));
        assert_eq!(phase, TouchPhase::Moved);
        assert_eq!(position, PhysicalPosition::new(31.0, 47.0));
    }

    #[test]
    fn window_scroll_preserves_line_delta() {
        let event = NativeTestWindowScroll::new(
            egui::ViewportId::ROOT,
            [1.0, 2.0],
            NativeTestScrollDelta::lines(1.25, -2.5),
        )
        .into_window_event();

        assert!(matches!(
            event,
            WindowEvent::MouseWheel {
                delta: MouseScrollDelta::LineDelta(1.25, -2.5),
                position: Some(position),
                ..
            } if position == PhysicalPosition::new(1.0, 2.0)
        ));
    }
}

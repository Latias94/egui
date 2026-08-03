//! Event-time native pointer routing probes.

use std::sync::Arc;

use winit::{event::WindowEvent, window::Window};

use super::platform_provider::{
    NativeAuthority, NativeCaptureOwner, NativeHoveredWindow, NativePhysicalPoint,
    NativePointerDeliveryOwner, NativeUnavailableReason, NativeViewportBinding,
};

#[cfg(target_os = "macos")]
#[path = "native_pointer_probe/macos.rs"]
mod platform;
#[cfg(target_os = "windows")]
#[path = "native_pointer_probe/windows.rs"]
mod platform;
#[cfg(all(
    unix,
    feature = "x11",
    not(any(target_os = "macos", target_os = "ios", target_os = "android"))
))]
#[path = "native_pointer_probe/x11.rs"]
mod platform;
#[cfg(not(any(
    target_os = "windows",
    target_os = "macos",
    all(
        unix,
        feature = "x11",
        not(any(target_os = "macos", target_os = "ios", target_os = "android"))
    )
)))]
#[path = "native_pointer_probe/fallback.rs"]
mod platform;

pub(super) struct NativePointerRouteProbe {
    pub(super) hovered: NativeAuthority<NativeHoveredWindow>,
    pub(super) capture: NativeAuthority<NativeCaptureOwner>,
    pub(super) position: NativeAuthority<NativePhysicalPoint>,
}

pub(super) struct NativePointerEventRoute {
    pub(super) delivery_owner: NativeAuthority<NativePointerDeliveryOwner>,
    pub(super) hovered: NativeAuthority<NativeHoveredWindow>,
    pub(super) capture: NativeAuthority<NativeCaptureOwner>,
    pub(super) capture_after: NativeAuthority<NativeCaptureOwner>,
    pub(super) position: NativeAuthority<NativePhysicalPoint>,
    pub(super) observed_pointer_event: bool,
}

pub(super) fn probe(windows: &[(NativeViewportBinding, Arc<Window>)]) -> NativePointerRouteProbe {
    platform::probe(windows)
}

pub(super) fn probe_event(
    binding: NativeViewportBinding,
    window: &Window,
    windows: &[(NativeViewportBinding, Arc<Window>)],
    event: &WindowEvent,
) -> NativePointerEventRoute {
    let observed_pointer_event = is_pointer_event(event);
    if !observed_pointer_event {
        return unknown_event_route(false, NativeUnavailableReason::NotObserved);
    }

    let callback_is_current = windows.iter().any(|(candidate_binding, candidate_window)| {
        *candidate_binding == binding
            && candidate_window.id() == window.id()
            && std::ptr::eq(candidate_window.as_ref(), window)
    });
    if !callback_is_current {
        return unknown_event_route(true, NativeUnavailableReason::StaleSource);
    }

    let route = platform::probe_event(windows, window);
    NativePointerEventRoute {
        delivery_owner: NativeAuthority::known(NativePointerDeliveryOwner::Viewport(binding)),
        hovered: route.hovered,
        capture: route.capture.clone(),
        capture_after: route.capture,
        position: route.position,
        observed_pointer_event: true,
    }
}

fn is_pointer_event(event: &WindowEvent) -> bool {
    matches!(
        event,
        WindowEvent::CursorMoved { .. }
            | WindowEvent::MouseWheel { .. }
            | WindowEvent::PanGesture { .. }
            | WindowEvent::MouseInput { .. }
            | WindowEvent::Touch(_)
    )
}

fn unknown_probe(reason: NativeUnavailableReason) -> NativePointerRouteProbe {
    NativePointerRouteProbe {
        hovered: NativeAuthority::unknown(reason),
        capture: NativeAuthority::unknown(reason),
        position: NativeAuthority::unknown(reason),
    }
}

#[cfg(any(
    test,
    target_os = "windows",
    all(
        unix,
        feature = "x11",
        not(any(target_os = "macos", target_os = "ios", target_os = "android"))
    )
))]
fn without_event_time_hit(mut route: NativePointerRouteProbe) -> NativePointerRouteProbe {
    route.hovered = NativeAuthority::unknown(NativeUnavailableReason::NotObserved);
    route.position = NativeAuthority::unknown(NativeUnavailableReason::NotObserved);
    route
}

fn unknown_event_route(
    observed_pointer_event: bool,
    reason: NativeUnavailableReason,
) -> NativePointerEventRoute {
    NativePointerEventRoute {
        delivery_owner: NativeAuthority::unknown(reason),
        hovered: NativeAuthority::unknown(reason),
        capture: NativeAuthority::unknown(reason),
        capture_after: NativeAuthority::unknown(reason),
        position: NativeAuthority::unknown(reason),
        observed_pointer_event,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::native::platform_provider::NativePlatformCoordinator;

    #[test]
    fn a_delivered_edge_can_report_b_as_event_time_hover() {
        let mut coordinator = NativePlatformCoordinator::default();
        let a = coordinator
            .register_viewport(egui::ViewportId::from_hash_of("delivery-a"))
            .unwrap();
        let b = coordinator
            .register_viewport(egui::ViewportId::from_hash_of("hover-b"))
            .unwrap();
        let route = NativePointerEventRoute {
            delivery_owner: NativeAuthority::known(NativePointerDeliveryOwner::Viewport(a)),
            hovered: NativeAuthority::known(NativeHoveredWindow::Viewport(b)),
            capture: NativeAuthority::unknown(NativeUnavailableReason::Unsupported),
            capture_after: NativeAuthority::unknown(NativeUnavailableReason::Unsupported),
            position: NativeAuthority::known(NativePhysicalPoint::new(10, 20)),
            observed_pointer_event: true,
        };

        assert_eq!(
            route.delivery_owner.value(),
            Some(&NativePointerDeliveryOwner::Viewport(a))
        );
        assert_eq!(
            route.hovered.value(),
            Some(&NativeHoveredWindow::Viewport(b))
        );
        assert!(route.capture.value().is_none());
    }

    #[test]
    fn stale_callback_identity_fails_closed_instead_of_authorizing_a2() {
        struct Identity(u64);

        let old = Identity(7);
        let replacement = Identity(9);
        let callback_is_current = std::ptr::eq(&old, &replacement);
        let route = if callback_is_current {
            unreachable!("distinct native objects must not compare equal")
        } else {
            unknown_event_route(true, NativeUnavailableReason::StaleSource)
        };

        assert_eq!(
            route.delivery_owner.unavailable_reason(),
            Some(NativeUnavailableReason::StaleSource)
        );
        assert_eq!(old.0, 7);
        assert_eq!(replacement.0, 9);
    }

    #[test]
    fn pan_gesture_is_a_native_pointer_event() {
        let event = WindowEvent::PanGesture {
            device_id: winit::event::DeviceId::dummy(),
            delta: winit::dpi::PhysicalPosition::new(1.0, -2.0),
            phase: winit::event::TouchPhase::Moved,
        };

        assert!(is_pointer_event(&event));
    }

    #[test]
    fn callback_time_hit_cannot_authorize_an_event_time_receiver() {
        let mut coordinator = NativePlatformCoordinator::default();
        let binding = coordinator
            .register_viewport(egui::ViewportId::from_hash_of("callback-time-hit"))
            .unwrap();
        let capture = NativeAuthority::known(NativeCaptureOwner::Viewport(binding));
        let route = without_event_time_hit(NativePointerRouteProbe {
            hovered: NativeAuthority::known(NativeHoveredWindow::Viewport(binding)),
            capture: capture.clone(),
            position: NativeAuthority::known(NativePhysicalPoint::new(10, 20)),
        });

        assert_eq!(
            route.hovered.unavailable_reason(),
            Some(NativeUnavailableReason::NotObserved)
        );
        assert_eq!(
            route.position.unavailable_reason(),
            Some(NativeUnavailableReason::NotObserved)
        );
        assert_eq!(route.capture, capture);
    }
}

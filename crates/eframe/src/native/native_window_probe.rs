//! Exact native window facts not exposed by portable winit getters.

use raw_window_handle::{HasWindowHandle as _, RawWindowHandle};
use winit::window::Window;

use super::platform_provider::{
    NativeAuthority, NativeBackendCapabilities, NativeBackendCapability, NativePhysicalPoint,
    NativePhysicalRect, NativePointerInputState, NativeUnavailableReason,
};

#[cfg(target_os = "macos")]
#[path = "native_window_probe/macos.rs"]
mod platform;
#[cfg(target_os = "windows")]
#[path = "native_window_probe/windows.rs"]
mod platform;
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
#[path = "native_window_probe/fallback.rs"]
mod platform;

pub(super) struct NativeWindowProbe {
    pub(super) work_area: NativeAuthority<NativePhysicalRect>,
    pub(super) pointer_input: NativeAuthority<NativePointerInputState>,
}

pub(super) fn probe(window: &Window) -> NativeWindowProbe {
    platform::probe(window)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum NativeWindowBackend {
    MacOs,
    Windows,
    X11,
    Wayland,
    Unknown,
}

/// Returns capabilities from the actual runtime window backend, not enabled Cargo features.
pub(super) fn capabilities(window: &Window) -> NativeBackendCapabilities {
    let backend = window
        .window_handle()
        .map(|handle| backend_from_raw_handle(handle.as_raw()))
        .unwrap_or(NativeWindowBackend::Unknown);
    capabilities_for_backend(backend)
}

const fn backend_from_raw_handle(handle: RawWindowHandle) -> NativeWindowBackend {
    match handle {
        RawWindowHandle::AppKit(_) => NativeWindowBackend::MacOs,
        RawWindowHandle::Win32(_) => NativeWindowBackend::Windows,
        // Winit's X11 backend exposes Xlib handles. The fork's route probe is
        // deliberately Xlib-backed, so an arbitrary Xcb handle stays unknown.
        RawWindowHandle::Xlib(_) => NativeWindowBackend::X11,
        RawWindowHandle::Wayland(_) => NativeWindowBackend::Wayland,
        _ => NativeWindowBackend::Unknown,
    }
}

const fn capabilities_for_backend(backend: NativeWindowBackend) -> NativeBackendCapabilities {
    use NativeBackendCapability::{Supported, Unknown, Unsupported};

    match backend {
        NativeWindowBackend::MacOs => {
            NativeBackendCapabilities::new(Supported, Supported, Supported)
                .with_pointer_routing(Supported, Supported)
                .with_pointer_hit_test(Supported, Supported)
                // The current provider proves focus on an owned window, but cannot distinguish a
                // foreign focused window from an environment with no focused window.
                .with_focus(Unsupported, Supported)
        }
        NativeWindowBackend::Windows | NativeWindowBackend::X11 => {
            NativeBackendCapabilities::new(Supported, Supported, Supported)
                .with_pointer_routing(Supported, Supported)
                // Dispatch exists, but no exact post-dispatch state probe is wired yet.
                .with_pointer_hit_test(Unsupported, Supported)
                .with_focus(Unsupported, Supported)
        }
        NativeWindowBackend::Wayland => {
            NativeBackendCapabilities::new(Supported, Unsupported, Unsupported)
                .with_pointer_routing(Unsupported, Unsupported)
                // Wayland supports setting an input region but exposes neither desktop-global
                // routing nor a read-back of the current region through this provider.
                .with_pointer_hit_test(Unsupported, Supported)
                .with_focus(Unsupported, Unsupported)
        }
        NativeWindowBackend::Unknown => NativeBackendCapabilities::new(Unknown, Unknown, Unknown)
            .with_pointer_routing(Unknown, Unknown)
            .with_pointer_hit_test(Unknown, Unknown)
            .with_focus(Unknown, Unknown),
    }
}

fn unknown_probe() -> NativeWindowProbe {
    NativeWindowProbe {
        work_area: NativeAuthority::unknown(NativeUnavailableReason::Unsupported),
        pointer_input: NativeAuthority::unknown(NativeUnavailableReason::Unsupported),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wayland_keeps_object_lifecycle_but_rejects_visibility_and_global_placement() {
        let capabilities = capabilities_for_backend(NativeWindowBackend::Wayland);

        assert_eq!(
            capabilities.window_lifecycle(),
            NativeBackendCapability::Supported
        );
        assert_eq!(
            capabilities.window_visibility(),
            NativeBackendCapability::Unsupported
        );
        assert_eq!(
            capabilities.global_window_placement(),
            NativeBackendCapability::Unsupported
        );
        assert_eq!(
            capabilities.hovered_window(),
            NativeBackendCapability::Unsupported
        );
        assert_eq!(
            capabilities.desktop_pointer_position(),
            NativeBackendCapability::Unsupported
        );
        assert_eq!(
            capabilities.authoritative_button_state(),
            NativeBackendCapability::Supported
        );
        assert_eq!(
            capabilities.pointer_hit_test_observation(),
            NativeBackendCapability::Unsupported
        );
        assert_eq!(
            capabilities.pointer_hit_test_control(),
            NativeBackendCapability::Supported
        );
        assert_eq!(
            capabilities.global_focus_observation(),
            NativeBackendCapability::Unsupported
        );
        assert_eq!(
            capabilities.window_activation_control(),
            NativeBackendCapability::Unsupported
        );
        assert_eq!(
            capabilities.close_cancellation(),
            NativeBackendCapability::Supported
        );
    }

    #[test]
    fn macos_exposes_the_only_complete_pointer_hit_test_lane() {
        let capabilities = capabilities_for_backend(NativeWindowBackend::MacOs);

        assert_eq!(
            capabilities.hovered_window(),
            NativeBackendCapability::Supported
        );
        assert_eq!(
            capabilities.desktop_pointer_position(),
            NativeBackendCapability::Supported
        );
        assert_eq!(
            capabilities.pointer_hit_test_observation(),
            NativeBackendCapability::Supported
        );
        assert_eq!(
            capabilities.pointer_hit_test_control(),
            NativeBackendCapability::Supported
        );
        assert_eq!(
            capabilities.global_focus_observation(),
            NativeBackendCapability::Unsupported
        );
        assert_eq!(
            capabilities.window_activation_control(),
            NativeBackendCapability::Supported
        );
    }

    #[test]
    fn windows_and_x11_expose_routing_but_not_hit_test_readback() {
        for backend in [NativeWindowBackend::Windows, NativeWindowBackend::X11] {
            let capabilities = capabilities_for_backend(backend);

            assert_eq!(
                capabilities.hovered_window(),
                NativeBackendCapability::Supported
            );
            assert_eq!(
                capabilities.desktop_pointer_position(),
                NativeBackendCapability::Supported
            );
            assert_eq!(
                capabilities.pointer_hit_test_observation(),
                NativeBackendCapability::Unsupported
            );
            assert_eq!(
                capabilities.pointer_hit_test_control(),
                NativeBackendCapability::Supported
            );
            assert_eq!(
                capabilities.window_activation_control(),
                NativeBackendCapability::Supported
            );
        }
    }

    #[test]
    fn an_unidentified_backend_remains_unknown() {
        let capabilities = capabilities_for_backend(NativeWindowBackend::Unknown);

        assert_eq!(
            capabilities.window_lifecycle(),
            NativeBackendCapability::Unknown
        );
        assert_eq!(
            capabilities.window_visibility(),
            NativeBackendCapability::Unknown
        );
        assert_eq!(
            capabilities.global_window_placement(),
            NativeBackendCapability::Unknown
        );
        assert_eq!(
            capabilities.hovered_window(),
            NativeBackendCapability::Unknown
        );
        assert_eq!(
            capabilities.pointer_hit_test_control(),
            NativeBackendCapability::Unknown
        );
        assert_eq!(
            capabilities.authoritative_inventory(),
            NativeBackendCapability::Supported
        );
        assert_eq!(
            capabilities.authoritative_button_state(),
            NativeBackendCapability::Supported
        );
        assert_eq!(
            capabilities.close_cancellation(),
            NativeBackendCapability::Supported
        );
    }
}

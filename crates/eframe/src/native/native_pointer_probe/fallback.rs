//! Fail-closed route facts for platforms without global pointer authority.

use std::sync::Arc;

use winit::window::Window;

use super::{
    NativePointerRouteProbe, NativeUnavailableReason, NativeViewportBinding, unknown_probe,
};

pub(super) fn probe(_windows: &[(NativeViewportBinding, Arc<Window>)]) -> NativePointerRouteProbe {
    // Wayland intentionally lands here. The compositor does not expose a
    // global hovered-window inventory or persistent capture owner.
    unknown_probe(NativeUnavailableReason::Unsupported)
}

pub(super) fn probe_event(
    windows: &[(NativeViewportBinding, Arc<Window>)],
) -> NativePointerRouteProbe {
    probe(windows)
}

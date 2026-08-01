//! Fail-closed work-area facts until an exact platform bridge is available.

use winit::window::Window;

use super::{NativeAuthority, NativeMonitorWorkArea, NativeUnavailableReason, unknown};

pub(super) fn probe(_window: &Window) -> NativeAuthority<Vec<NativeMonitorWorkArea>> {
    // X11 requires proved EWMH work-area facts; Wayland intentionally exposes
    // no desktop-global placement authority. Full monitor bounds are not a
    // substitute for either platform's usable work area.
    unknown(NativeUnavailableReason::Unsupported)
}

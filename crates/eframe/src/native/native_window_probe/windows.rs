//! Fail-closed native window facts until the Win32 fact bridge is available.

use winit::window::Window;

use super::{NativeWindowProbe, unknown_probe};

/// Windows does not yet expose an exact mixed-DPI work-area and pointer-input
/// observation through this fork. Returning `Unknown` keeps placement and
/// pass-through fail-closed instead of deriving authority from window geometry.
pub(super) fn probe(_window: &Window) -> NativeWindowProbe {
    unknown_probe()
}

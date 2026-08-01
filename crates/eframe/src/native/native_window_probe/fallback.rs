//! Fail-closed native window facts for unsupported platforms.

use winit::window::Window;

use super::{NativeWindowProbe, unknown_probe};

pub(super) fn probe(_window: &Window) -> NativeWindowProbe {
    unknown_probe()
}

//! Exact native monitor work-area rosters.

use winit::window::Window;

use super::platform_provider::{
    NativeAuthority, NativePhysicalPoint, NativePhysicalRect, NativeUnavailableReason,
};

#[cfg(target_os = "macos")]
#[path = "native_work_area_probe/macos.rs"]
mod platform;
#[cfg(target_os = "windows")]
#[path = "native_work_area_probe/windows.rs"]
mod platform;
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
#[path = "native_work_area_probe/fallback.rs"]
mod platform;

#[derive(Clone, Debug, PartialEq)]
pub(super) struct NativeMonitorWorkArea {
    pub(super) identity: String,
    pub(super) monitor_bounds: NativePhysicalRect,
    pub(super) work_area_bounds: NativePhysicalRect,
    pub(super) scale_factor: f64,
}

pub(super) fn probe(window: &Window) -> NativeAuthority<Vec<NativeMonitorWorkArea>> {
    platform::probe(window)
}

fn physical_rect(min_x: i32, min_y: i32, width: u32, height: u32) -> Option<NativePhysicalRect> {
    if width == 0 || height == 0 {
        return None;
    }
    let width = i32::try_from(width).ok()?;
    let height = i32::try_from(height).ok()?;
    Some(NativePhysicalRect::new(
        NativePhysicalPoint::new(min_x, min_y),
        NativePhysicalPoint::new(min_x.checked_add(width)?, min_y.checked_add(height)?),
    ))
}

fn unknown(reason: NativeUnavailableReason) -> NativeAuthority<Vec<NativeMonitorWorkArea>> {
    NativeAuthority::unknown(reason)
}

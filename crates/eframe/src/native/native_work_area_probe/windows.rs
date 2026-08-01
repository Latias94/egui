//! Win32-backed complete monitor work-area roster.

use std::mem::size_of;

use windows_sys::Win32::Graphics::Gdi::{GetMonitorInfoW, MONITORINFO};
use winit::{platform::windows::MonitorHandleExtWindows as _, window::Window};

use super::{
    NativeAuthority, NativeMonitorWorkArea, NativePhysicalPoint, NativePhysicalRect,
    NativeUnavailableReason, physical_rect, unknown,
};

#[expect(
    unsafe_code,
    reason = "GetMonitorInfoW reads into a correctly sized value for a live winit monitor handle"
)]
pub(super) fn probe(window: &Window) -> NativeAuthority<Vec<NativeMonitorWorkArea>> {
    let mut roster = Vec::new();
    for monitor in window.available_monitors() {
        let position = monitor.position();
        let size = monitor.size();
        let Some(monitor_bounds) = physical_rect(position.x, position.y, size.width, size.height)
        else {
            return unknown(NativeUnavailableReason::StaleSource);
        };
        let scale = monitor.scale_factor();
        if !scale.is_finite() || scale <= 0.0 {
            return unknown(NativeUnavailableReason::StaleSource);
        }
        let mut info = MONITORINFO {
            cbSize: size_of::<MONITORINFO>() as u32,
            rcMonitor: Default::default(),
            rcWork: Default::default(),
            dwFlags: 0,
        };
        // SAFETY: the handle comes from this live winit MonitorHandle and info
        // names a correctly sized writable MONITORINFO value.
        if unsafe { GetMonitorInfoW(monitor.hmonitor(), &mut info) } == 0 {
            return unknown(NativeUnavailableReason::StaleSource);
        }
        let work = info.rcWork;
        if work.right <= work.left || work.bottom <= work.top {
            return unknown(NativeUnavailableReason::StaleSource);
        }
        roster.push(NativeMonitorWorkArea {
            identity: format!("windows-monitor:{}", monitor.native_id()),
            monitor_bounds,
            work_area_bounds: NativePhysicalRect::new(
                NativePhysicalPoint::new(work.left, work.top),
                NativePhysicalPoint::new(work.right, work.bottom),
            ),
            scale_factor: scale,
        });
    }
    if roster.is_empty() {
        unknown(NativeUnavailableReason::NotObserved)
    } else {
        NativeAuthority::known(roster)
    }
}

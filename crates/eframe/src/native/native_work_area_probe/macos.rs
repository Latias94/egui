//! AppKit-backed complete monitor work-area roster.

use objc2::MainThreadMarker;
use objc2_app_kit::NSScreen;
use objc2_foundation::NSRect;
use winit::{platform::macos::MonitorHandleExtMacOS as _, window::Window};

use super::{
    NativeAuthority, NativeMonitorWorkArea, NativePhysicalPoint, NativePhysicalRect,
    NativeUnavailableReason, physical_rect, unknown,
};

fn rounded_i32(value: f64) -> Option<i32> {
    if !value.is_finite() || value < f64::from(i32::MIN) || value > f64::from(i32::MAX) {
        return None;
    }
    Some(value.round() as i32)
}

#[expect(
    unsafe_code,
    reason = "winit supplies the live NSScreen pointer for each monitor handle"
)]
pub(super) fn probe(window: &Window) -> NativeAuthority<Vec<NativeMonitorWorkArea>> {
    let Some(mtm) = MainThreadMarker::new() else {
        return unknown(NativeUnavailableReason::NotObserved);
    };
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
        let Some(screen_ptr) = monitor.ns_screen() else {
            return unknown(NativeUnavailableReason::StaleSource);
        };
        // SAFETY: winit returns the live NSScreen for this MonitorHandle and the
        // probe runs synchronously on AppKit's main thread.
        let screen = unsafe { &*screen_ptr.cast::<NSScreen>() };
        let frame = screen.frame();
        let visible = screen.visibleFrame();
        let Some(offset_x) = rounded_i32((visible.origin.x - frame.origin.x) * scale) else {
            return unknown(NativeUnavailableReason::StaleSource);
        };
        let frame_top = frame.origin.y + frame.size.height;
        let visible_top = visible.origin.y + visible.size.height;
        let Some(offset_y) = rounded_i32((frame_top - visible_top) * scale) else {
            return unknown(NativeUnavailableReason::StaleSource);
        };
        let (Some(width), Some(height)) = (
            rounded_i32(visible.size.width * scale),
            rounded_i32(visible.size.height * scale),
        ) else {
            return unknown(NativeUnavailableReason::StaleSource);
        };
        if width <= 0 || height <= 0 {
            return unknown(NativeUnavailableReason::StaleSource);
        }
        let (Some(min_x), Some(min_y)) = (
            position.x.checked_add(offset_x),
            position.y.checked_add(offset_y),
        ) else {
            return unknown(NativeUnavailableReason::StaleSource);
        };
        let (Some(max_x), Some(max_y)) = (min_x.checked_add(width), min_y.checked_add(height))
        else {
            return unknown(NativeUnavailableReason::StaleSource);
        };
        roster.push(NativeMonitorWorkArea {
            identity: format!("macos-display:{}", monitor.native_id()),
            monitor_bounds,
            work_area_bounds: NativePhysicalRect::new(
                NativePhysicalPoint::new(min_x, min_y),
                NativePhysicalPoint::new(max_x, max_y),
            ),
            scale_factor: scale,
        });
    }
    if roster.is_empty() {
        probe_app_kit_screens(mtm)
    } else {
        NativeAuthority::known(roster)
    }
}

fn probe_app_kit_screens(mtm: MainThreadMarker) -> NativeAuthority<Vec<NativeMonitorWorkArea>> {
    let screens = NSScreen::screens(mtm);
    let Some(primary) = screens.iter().next() else {
        return unknown(NativeUnavailableReason::NotObserved);
    };
    let primary_frame = primary.frame();
    let primary_top = primary_frame.origin.y + primary_frame.size.height;
    if !primary_top.is_finite() {
        return unknown(NativeUnavailableReason::StaleSource);
    }

    let mut roster = Vec::with_capacity(screens.len());
    for screen in screens.iter() {
        let scale = screen.backingScaleFactor();
        let display_id = screen.CGDirectDisplayID();
        if display_id == 0 || !scale.is_finite() || scale <= 0.0 {
            return unknown(NativeUnavailableReason::StaleSource);
        }
        let Some((monitor_bounds, work_area_bounds)) =
            app_kit_physical_bounds(screen.frame(), screen.visibleFrame(), primary_top, scale)
        else {
            return unknown(NativeUnavailableReason::StaleSource);
        };
        roster.push(NativeMonitorWorkArea {
            identity: format!("macos-display:{display_id}"),
            monitor_bounds,
            work_area_bounds,
            scale_factor: scale,
        });
    }
    NativeAuthority::known(roster)
}

fn app_kit_physical_bounds(
    frame: NSRect,
    visible: NSRect,
    primary_top: f64,
    scale: f64,
) -> Option<(NativePhysicalRect, NativePhysicalRect)> {
    if !scale.is_finite() || scale <= 0.0 || !primary_top.is_finite() {
        return None;
    }
    let frame_top = frame.origin.y + frame.size.height;
    let visible_top = visible.origin.y + visible.size.height;
    let monitor_x = rounded_i32(frame.origin.x * scale)?;
    let monitor_y = rounded_i32((primary_top - frame_top) * scale)?;
    let monitor_width = rounded_i32(frame.size.width * scale)?;
    let monitor_height = rounded_i32(frame.size.height * scale)?;
    let work_x =
        monitor_x.checked_add(rounded_i32((visible.origin.x - frame.origin.x) * scale)?)?;
    let work_y = monitor_y.checked_add(rounded_i32((frame_top - visible_top) * scale)?)?;
    let work_width = rounded_i32(visible.size.width * scale)?;
    let work_height = rounded_i32(visible.size.height * scale)?;
    if monitor_width <= 0 || monitor_height <= 0 || work_width <= 0 || work_height <= 0 {
        return None;
    }
    let monitor_max_x = monitor_x.checked_add(monitor_width)?;
    let monitor_max_y = monitor_y.checked_add(monitor_height)?;
    let work_max_x = work_x.checked_add(work_width)?;
    let work_max_y = work_y.checked_add(work_height)?;
    if work_x < monitor_x
        || work_y < monitor_y
        || work_max_x > monitor_max_x
        || work_max_y > monitor_max_y
    {
        return None;
    }
    Some((
        NativePhysicalRect::new(
            NativePhysicalPoint::new(monitor_x, monitor_y),
            NativePhysicalPoint::new(monitor_max_x, monitor_max_y),
        ),
        NativePhysicalRect::new(
            NativePhysicalPoint::new(work_x, work_y),
            NativePhysicalPoint::new(work_max_x, work_max_y),
        ),
    ))
}

#[cfg(test)]
mod tests {
    use objc2_foundation::{NSPoint, NSSize};

    use super::*;

    #[test]
    fn app_kit_bounds_match_winit_top_left_physical_coordinates() {
        let frame = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(1920.0, 1080.0));
        let visible = NSRect::new(NSPoint::new(0.0, 75.0), NSSize::new(1920.0, 975.0));

        let (monitor, work_area) = app_kit_physical_bounds(frame, visible, 1080.0, 1.0)
            .expect("the screen geometry is valid");

        assert_eq!(
            monitor,
            NativePhysicalRect::new(
                NativePhysicalPoint::new(0, 0),
                NativePhysicalPoint::new(1920, 1080),
            )
        );
        assert_eq!(
            work_area,
            NativePhysicalRect::new(
                NativePhysicalPoint::new(0, 30),
                NativePhysicalPoint::new(1920, 1005),
            )
        );
    }

    #[test]
    fn app_kit_bounds_scale_each_secondary_screen_in_its_own_physical_space() {
        let frame = NSRect::new(NSPoint::new(-1280.0, 0.0), NSSize::new(1280.0, 800.0));

        let (monitor, work_area) = app_kit_physical_bounds(frame, frame, 1080.0, 2.0)
            .expect("the mixed-DPI screen geometry is valid");

        let expected = NativePhysicalRect::new(
            NativePhysicalPoint::new(-2560, 560),
            NativePhysicalPoint::new(0, 2160),
        );
        assert_eq!(monitor, expected);
        assert_eq!(work_area, expected);
    }
}

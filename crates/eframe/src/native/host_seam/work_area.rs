use winit::event_loop::ActiveEventLoop;

use super::NativePhysicalRect;

/// Process-local identity for one native display.
///
/// This value is intended only for correlating adjacent native-host snapshots.
/// It is not a persistent monitor identifier and does not expose a platform
/// handle.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NativeDisplayId(u64);

impl std::fmt::Debug for NativeDisplayId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("NativeDisplayId(..)")
    }
}

/// Exact display and usable work-area facts captured at one root output boundary.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct NativeWorkAreaRecord {
    display_id: NativeDisplayId,
    display_bounds: NativePhysicalRect,
    work_area_bounds: NativePhysicalRect,
    scale_factor: f64,
}

impl NativeWorkAreaRecord {
    #[cfg(any(
        test,
        target_os = "windows",
        target_os = "macos",
        all(target_os = "linux", feature = "x11")
    ))]
    pub(super) const fn new(
        display_id: u64,
        display_bounds: NativePhysicalRect,
        work_area_bounds: NativePhysicalRect,
        scale_factor: f64,
    ) -> Self {
        Self {
            display_id: NativeDisplayId(display_id),
            display_bounds,
            work_area_bounds,
            scale_factor,
        }
    }

    /// Returns the process-local display identity.
    pub const fn display_id(self) -> NativeDisplayId {
        self.display_id
    }

    /// Returns the full physical display rectangle.
    pub const fn display_bounds(self) -> NativePhysicalRect {
        self.display_bounds
    }

    /// Returns the usable physical work area after platform-reserved regions.
    pub const fn work_area_bounds(self) -> NativePhysicalRect {
        self.work_area_bounds
    }

    /// Returns the exact native scale factor for this display.
    pub const fn scale_factor(self) -> f64 {
        self.scale_factor
    }
}

/// Complete work-area authority attached to one root viewport roster.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum NativeWorkAreaRoster<'a> {
    /// Every current native display and its exact usable work area.
    Exact(&'a [NativeWorkAreaRecord]),
    /// The active backend cannot prove a complete exact roster.
    Unknown,
}

#[derive(Debug)]
pub(super) enum OwnedNativeWorkAreaRoster {
    Exact(Vec<NativeWorkAreaRecord>),
    Unknown,
}

impl OwnedNativeWorkAreaRoster {
    pub(super) fn capture(event_loop: &ActiveEventLoop) -> Self {
        platform::capture(event_loop).map_or(Self::Unknown, Self::Exact)
    }

    pub(super) fn as_borrowed(&self) -> NativeWorkAreaRoster<'_> {
        match self {
            Self::Exact(records) => NativeWorkAreaRoster::Exact(records),
            Self::Unknown => NativeWorkAreaRoster::Unknown,
        }
    }
}

#[cfg(target_os = "windows")]
mod platform {
    use std::mem;

    use windows_sys::Win32::Foundation::S_OK;
    use windows_sys::Win32::Graphics::Gdi::{GetMonitorInfoW, MONITORINFO};
    use windows_sys::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
    use winit::event_loop::ActiveEventLoop;
    use winit::platform::windows::MonitorHandleExtWindows as _;

    use super::{NativePhysicalRect, NativeWorkAreaRecord};

    #[expect(unsafe_code)]
    pub(super) fn capture(event_loop: &ActiveEventLoop) -> Option<Vec<NativeWorkAreaRecord>> {
        let mut records = Vec::new();
        for monitor in event_loop.available_monitors() {
            let raw = monitor.hmonitor();
            if raw == 0 {
                return None;
            }
            let hmonitor = raw as usize as *mut core::ffi::c_void;
            let mut info = MONITORINFO {
                cbSize: mem::size_of::<MONITORINFO>() as u32,
                ..MONITORINFO::default()
            };
            if unsafe { GetMonitorInfoW(hmonitor, &mut info) } == 0 {
                return None;
            }
            let mut dpi_x = 0;
            let mut dpi_y = 0;
            if unsafe { GetDpiForMonitor(hmonitor, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y) }
                != S_OK
                || dpi_x == 0
                || dpi_x != dpi_y
            {
                return None;
            }
            records.push(NativeWorkAreaRecord::new(
                raw as u64,
                rect(info.rcMonitor)?,
                rect(info.rcWork)?,
                f64::from(dpi_x) / 96.0,
            ));
        }
        complete(records)
    }

    fn rect(rect: windows_sys::Win32::Foundation::RECT) -> Option<NativePhysicalRect> {
        let width = u32::try_from(rect.right.checked_sub(rect.left)?).ok()?;
        let height = u32::try_from(rect.bottom.checked_sub(rect.top)?).ok()?;
        (width > 0 && height > 0)
            .then_some(NativePhysicalRect::new(rect.left, rect.top, width, height))
    }

    fn complete(mut records: Vec<NativeWorkAreaRecord>) -> Option<Vec<NativeWorkAreaRecord>> {
        records.sort_by_key(|record| record.display_id());
        (!records.is_empty()
            && records
                .windows(2)
                .all(|pair| pair[0].display_id() != pair[1].display_id()))
        .then_some(records)
    }
}

#[cfg(target_os = "macos")]
mod platform {
    use objc2::rc::Retained;
    use objc2_app_kit::NSScreen;
    use objc2_foundation::NSRect;
    use winit::event_loop::ActiveEventLoop;
    use winit::platform::macos::MonitorHandleExtMacOS as _;

    use super::{NativePhysicalRect, NativeWorkAreaRecord};

    #[expect(unsafe_code)]
    pub(super) fn capture(event_loop: &ActiveEventLoop) -> Option<Vec<NativeWorkAreaRecord>> {
        let mut records = Vec::new();
        let mut common_scale_factor = None;
        for monitor in event_loop.available_monitors() {
            let scale_factor = monitor.scale_factor();
            if !record_uniform_scale_factor(&mut common_scale_factor, scale_factor) {
                return None;
            }
            let display_position = monitor.position();
            let display_size = monitor.size();
            if display_size.width == 0 || display_size.height == 0 {
                return None;
            }

            let screen = monitor.ns_screen()?.cast::<NSScreen>();
            if screen.is_null() {
                return None;
            }
            // SAFETY: The pointer comes from a live `NSScreen`. Retaining it here
            // establishes ownership for the complete frame/visible-frame read instead
            // of relying on the temporary owner inside winit's extension method.
            let screen = unsafe { Retained::retain(screen) }?;
            let display_bounds = NativePhysicalRect::new(
                display_position.x,
                display_position.y,
                display_size.width,
                display_size.height,
            );
            let work_area_bounds = physical_work_area(
                display_bounds,
                scale_factor,
                screen.frame(),
                screen.visibleFrame(),
            )?;
            records.push(NativeWorkAreaRecord::new(
                u64::from(monitor.native_id()),
                display_bounds,
                work_area_bounds,
                scale_factor,
            ));
        }
        complete(records)
    }

    fn physical_work_area(
        display: NativePhysicalRect,
        scale_factor: f64,
        frame: NSRect,
        visible: NSRect,
    ) -> Option<NativePhysicalRect> {
        if !scale_factor.is_finite()
            || scale_factor <= 0.0
            || !rect_is_finite(frame)
            || !rect_is_finite(visible)
            || frame.size.width <= 0.0
            || frame.size.height <= 0.0
            || visible.size.width <= 0.0
            || visible.size.height <= 0.0
        {
            return None;
        }
        if !physical_extent_matches(frame.size.width, scale_factor, display.width())
            || !physical_extent_matches(frame.size.height, scale_factor, display.height())
        {
            return None;
        }

        let left = visible.origin.x - frame.origin.x;
        let bottom = visible.origin.y - frame.origin.y;
        let right = frame.origin.x + frame.size.width - visible.origin.x - visible.size.width;
        let top = frame.origin.y + frame.size.height - visible.origin.y - visible.size.height;
        let left = physical_inset(left, scale_factor)?;
        let bottom = physical_inset(bottom, scale_factor)?;
        let right = physical_inset(right, scale_factor)?;
        let top = physical_inset(top, scale_factor)?;
        let horizontal = left.checked_add(right)?;
        let vertical = top.checked_add(bottom)?;
        let width = display.width().checked_sub(horizontal)?;
        let height = display.height().checked_sub(vertical)?;
        if width == 0 || height == 0 {
            return None;
        }

        Some(NativePhysicalRect::new(
            display.x().checked_add(i32::try_from(left).ok()?)?,
            display.y().checked_add(i32::try_from(top).ok()?)?,
            width,
            height,
        ))
    }

    fn rect_is_finite(rect: NSRect) -> bool {
        [
            rect.origin.x,
            rect.origin.y,
            rect.size.width,
            rect.size.height,
        ]
        .into_iter()
        .all(f64::is_finite)
    }

    fn physical_inset(inset: f64, scale_factor: f64) -> Option<u32> {
        const COORDINATE_EPSILON: f64 = 1.0e-6;

        if inset < -COORDINATE_EPSILON {
            return None;
        }
        let physical = inset.max(0.0) * scale_factor;
        (physical.is_finite() && physical <= f64::from(u32::MAX)).then(|| physical.round() as u32)
    }

    fn physical_extent_matches(logical: f64, scale_factor: f64, physical: u32) -> bool {
        let expected = logical * scale_factor;
        expected.is_finite() && (expected - f64::from(physical)).abs() <= 1.0
    }

    fn record_uniform_scale_factor(expected: &mut Option<f64>, scale_factor: f64) -> bool {
        const SCALE_EPSILON: f64 = 1.0e-6;

        if !scale_factor.is_finite() || scale_factor <= 0.0 {
            return false;
        }
        if let Some(current) = *expected {
            (current - scale_factor).abs() <= SCALE_EPSILON
        } else {
            *expected = Some(scale_factor);
            true
        }
    }

    fn complete(mut records: Vec<NativeWorkAreaRecord>) -> Option<Vec<NativeWorkAreaRecord>> {
        records.sort_by_key(|record| record.display_id());
        (!records.is_empty()
            && records
                .windows(2)
                .all(|pair| pair[0].display_id() != pair[1].display_id())
            && records.iter().enumerate().all(|(index, record)| {
                records[index + 1..]
                    .iter()
                    .all(|other| !rects_overlap(record.display_bounds(), other.display_bounds()))
            }))
        .then_some(records)
    }

    fn rects_overlap(first: NativePhysicalRect, second: NativePhysicalRect) -> bool {
        let first_right = i64::from(first.x()) + i64::from(first.width());
        let first_bottom = i64::from(first.y()) + i64::from(first.height());
        let second_right = i64::from(second.x()) + i64::from(second.width());
        let second_bottom = i64::from(second.y()) + i64::from(second.height());
        i64::from(first.x()) < second_right
            && i64::from(second.x()) < first_right
            && i64::from(first.y()) < second_bottom
            && i64::from(second.y()) < first_bottom
    }

    #[cfg(test)]
    mod tests {
        use objc2_foundation::{NSPoint, NSSize};

        use super::*;

        #[test]
        fn visible_frame_insets_convert_to_physical_top_left_coordinates() {
            let display = NativePhysicalRect::new(200, -100, 2_880, 1_800);
            let frame = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(1_440.0, 900.0));
            let visible = NSRect::new(NSPoint::new(80.0, 23.0), NSSize::new(1_360.0, 853.0));

            assert_eq!(
                physical_work_area(display, 2.0, frame, visible),
                Some(NativePhysicalRect::new(360, -52, 2_720, 1_706))
            );
        }

        #[test]
        fn visible_frame_outside_display_is_rejected() {
            let display = NativePhysicalRect::new(0, 0, 1_440, 900);
            let frame = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(1_440.0, 900.0));
            let visible = NSRect::new(NSPoint::new(-1.0, 0.0), NSSize::new(1_441.0, 900.0));

            assert_eq!(physical_work_area(display, 1.0, frame, visible), None);
        }

        #[test]
        fn mismatched_backing_extent_is_rejected() {
            let display = NativePhysicalRect::new(0, 0, 2_880, 1_800);
            let frame = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(1_440.0, 900.0));

            assert_eq!(physical_work_area(display, 1.0, frame, frame), None);
        }

        #[test]
        fn mixed_display_scales_are_not_published_as_one_exact_roster() {
            let mut common = None;

            assert!(record_uniform_scale_factor(&mut common, 2.0));
            assert!(!record_uniform_scale_factor(&mut common, 1.0));
        }

        #[test]
        fn overlapping_display_records_are_rejected() {
            let first = NativeWorkAreaRecord::new(
                1,
                NativePhysicalRect::new(0, 0, 1_920, 1_080),
                NativePhysicalRect::new(0, 24, 1_920, 1_056),
                1.0,
            );
            let mirrored = NativeWorkAreaRecord::new(
                2,
                NativePhysicalRect::new(0, 0, 1_920, 1_080),
                NativePhysicalRect::new(0, 24, 1_920, 1_056),
                1.0,
            );

            assert_eq!(complete(vec![first, mirrored]), None);
        }
    }
}

#[cfg(any(test, all(target_os = "linux", feature = "x11")))]
#[derive(Clone, Copy, Debug, PartialEq)]
struct X11WorkAreaFacts {
    display_id: u64,
    display_bounds: NativePhysicalRect,
    work_area_bounds: NativePhysicalRect,
    scale_factor: f64,
}

#[cfg(any(test, all(target_os = "linux", feature = "x11")))]
fn compile_x11_snapshot(
    generation: u64,
    facts: impl IntoIterator<Item = X11WorkAreaFacts>,
) -> Option<Vec<NativeWorkAreaRecord>> {
    if generation == 0 {
        return None;
    }

    let mut records = facts
        .into_iter()
        .map(|fact| {
            (fact.display_id != 0
                && fact.scale_factor.is_finite()
                && fact.scale_factor > 0.0
                && valid_x11_rect(fact.display_bounds)
                && valid_x11_rect(fact.work_area_bounds)
                && x11_rect_contains(fact.display_bounds, fact.work_area_bounds))
            .then_some(NativeWorkAreaRecord::new(
                fact.display_id,
                fact.display_bounds,
                fact.work_area_bounds,
                fact.scale_factor,
            ))
        })
        .collect::<Option<Vec<_>>>()?;
    records.sort_by_key(|record| record.display_id());
    (!records.is_empty()
        && records
            .windows(2)
            .all(|pair| pair[0].display_id() != pair[1].display_id())
        && records.iter().enumerate().all(|(index, record)| {
            records[index + 1..]
                .iter()
                .all(|other| !x11_rects_overlap(record.display_bounds(), other.display_bounds()))
        }))
    .then_some(records)
}

#[cfg(any(test, all(target_os = "linux", feature = "x11")))]
fn valid_x11_rect(rect: NativePhysicalRect) -> bool {
    let Ok(width) = i32::try_from(rect.width()) else {
        return false;
    };
    let Ok(height) = i32::try_from(rect.height()) else {
        return false;
    };
    width > 0
        && height > 0
        && rect.x().checked_add(width).is_some()
        && rect.y().checked_add(height).is_some()
}

#[cfg(any(test, all(target_os = "linux", feature = "x11")))]
fn x11_rect_contains(outer: NativePhysicalRect, inner: NativePhysicalRect) -> bool {
    let outer_right = i64::from(outer.x()) + i64::from(outer.width());
    let outer_bottom = i64::from(outer.y()) + i64::from(outer.height());
    let inner_right = i64::from(inner.x()) + i64::from(inner.width());
    let inner_bottom = i64::from(inner.y()) + i64::from(inner.height());
    outer.x() <= inner.x()
        && outer.y() <= inner.y()
        && outer_right >= inner_right
        && outer_bottom >= inner_bottom
}

#[cfg(any(test, all(target_os = "linux", feature = "x11")))]
fn x11_rects_overlap(first: NativePhysicalRect, second: NativePhysicalRect) -> bool {
    let first_right = i64::from(first.x()) + i64::from(first.width());
    let first_bottom = i64::from(first.y()) + i64::from(first.height());
    let second_right = i64::from(second.x()) + i64::from(second.width());
    let second_bottom = i64::from(second.y()) + i64::from(second.height());
    i64::from(first.x()) < second_right
        && i64::from(second.x()) < first_right
        && i64::from(first.y()) < second_bottom
        && i64::from(second.y()) < first_bottom
}

#[cfg(all(target_os = "linux", feature = "x11"))]
mod platform {
    use winit::event_loop::ActiveEventLoop;
    use winit::platform::x11::ActiveEventLoopExtX11 as _;

    use super::{NativePhysicalRect, NativeWorkAreaRecord, X11WorkAreaFacts, compile_x11_snapshot};

    pub(super) fn capture(event_loop: &ActiveEventLoop) -> Option<Vec<NativeWorkAreaRecord>> {
        let snapshot = event_loop.work_area_authority_snapshot()?;
        compile_x11_snapshot(
            snapshot.generation(),
            snapshot.records().iter().map(|record| {
                let monitor_position = record.monitor_position();
                let monitor_size = record.monitor_size();
                let work_area_position = record.work_area_position();
                let work_area_size = record.work_area_size();
                X11WorkAreaFacts {
                    display_id: u64::from(record.crtc_id()),
                    display_bounds: NativePhysicalRect::new(
                        monitor_position.x,
                        monitor_position.y,
                        monitor_size.width,
                        monitor_size.height,
                    ),
                    work_area_bounds: NativePhysicalRect::new(
                        work_area_position.x,
                        work_area_position.y,
                        work_area_size.width,
                        work_area_size.height,
                    ),
                    scale_factor: record.scale_factor(),
                }
            }),
        )
    }
}

#[cfg(any(
    all(target_os = "linux", not(feature = "x11")),
    not(any(target_os = "windows", target_os = "macos", target_os = "linux"))
))]
mod platform {
    use winit::event_loop::ActiveEventLoop;

    use super::NativeWorkAreaRecord;

    pub(super) fn capture(_event_loop: &ActiveEventLoop) -> Option<Vec<NativeWorkAreaRecord>> {
        None
    }
}

#[cfg(test)]
mod x11_tests {
    use super::*;

    fn facts(
        display_id: u64,
        display_bounds: NativePhysicalRect,
        work_area_bounds: NativePhysicalRect,
        scale_factor: f64,
    ) -> X11WorkAreaFacts {
        X11WorkAreaFacts {
            display_id,
            display_bounds,
            work_area_bounds,
            scale_factor,
        }
    }

    #[test]
    fn exact_x11_snapshot_preserves_sorted_mixed_scale_records() {
        let records = compile_x11_snapshot(
            7,
            [
                facts(
                    20,
                    NativePhysicalRect::new(1_920, 0, 1_920, 1_080),
                    NativePhysicalRect::new(1_920, 24, 1_920, 1_056),
                    1.5,
                ),
                facts(
                    10,
                    NativePhysicalRect::new(0, 0, 1_920, 1_080),
                    NativePhysicalRect::new(0, 24, 1_920, 1_056),
                    1.0,
                ),
            ],
        )
        .expect("the validated X11 authority should map exactly");

        assert_eq!(records.len(), 2);
        assert_eq!(records[0].display_id(), NativeDisplayId(10));
        assert_eq!(records[1].display_id(), NativeDisplayId(20));
        assert_eq!(records[1].scale_factor(), 1.5);
    }

    #[test]
    fn invalid_x11_authority_never_becomes_an_exact_roster() {
        let display = NativePhysicalRect::new(0, 0, 1_920, 1_080);
        let usable = NativePhysicalRect::new(0, 24, 1_920, 1_056);

        assert_eq!(
            compile_x11_snapshot(0, [facts(1, display, usable, 1.0)]),
            None
        );
        assert_eq!(compile_x11_snapshot(1, []), None);
        assert_eq!(
            compile_x11_snapshot(1, [facts(0, display, usable, 1.0)]),
            None
        );
        assert_eq!(
            compile_x11_snapshot(
                1,
                [
                    facts(1, display, usable, 1.0),
                    facts(1, display, usable, 1.0)
                ],
            ),
            None
        );
        assert_eq!(
            compile_x11_snapshot(
                1,
                [
                    facts(1, display, usable, 1.0),
                    facts(
                        2,
                        NativePhysicalRect::new(1_000, 0, 1_920, 1_080),
                        NativePhysicalRect::new(1_000, 24, 1_920, 1_056),
                        1.0,
                    ),
                ],
            ),
            None
        );
        assert_eq!(
            compile_x11_snapshot(
                1,
                [facts(
                    1,
                    display,
                    NativePhysicalRect::new(1_900, 24, 100, 100),
                    1.0,
                )],
            ),
            None
        );
        assert_eq!(
            compile_x11_snapshot(1, [facts(1, display, usable, f64::NAN)]),
            None
        );
    }
}

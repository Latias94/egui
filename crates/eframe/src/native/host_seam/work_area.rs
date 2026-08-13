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

#[cfg(not(target_os = "windows"))]
mod platform {
    use winit::event_loop::ActiveEventLoop;

    use super::NativeWorkAreaRecord;

    pub(super) fn capture(_event_loop: &ActiveEventLoop) -> Option<Vec<NativeWorkAreaRecord>> {
        None
    }
}

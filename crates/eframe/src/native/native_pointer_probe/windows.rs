//! Win32 event-thread route probes.

use std::{collections::BTreeMap, sync::Arc};

use raw_window_handle::{HasWindowHandle as _, RawWindowHandle};
use windows_sys::Win32::{
    Foundation::{HWND, POINT},
    UI::{
        Input::KeyboardAndMouse::GetCapture,
        WindowsAndMessaging::{GA_ROOT, GetAncestor, GetCursorPos, WindowFromPoint},
    },
};
use winit::window::Window;

use super::{
    NativeAuthority, NativeCaptureOwner, NativeHoveredWindow, NativePointerRouteProbe,
    NativeUnavailableReason, NativeViewportBinding, unknown_probe,
};

fn window_handle(window: &Window) -> Option<HWND> {
    let handle = window.window_handle().ok()?;
    let RawWindowHandle::Win32(handle) = handle.as_raw() else {
        return None;
    };
    Some(handle.hwnd.get() as HWND)
}

#[expect(
    unsafe_code,
    reason = "Win32 route authority requires canonicalizing validated winit HWND values"
)]
fn exact_roster(
    windows: &[(NativeViewportBinding, Arc<Window>)],
) -> Option<BTreeMap<usize, NativeViewportBinding>> {
    let mut roster = BTreeMap::new();
    for (binding, window) in windows {
        let root = unsafe { GetAncestor(window_handle(window)?, GA_ROOT) };
        if root.is_null() || roster.insert(root as usize, *binding).is_some() {
            return None;
        }
    }
    Some(roster)
}

#[expect(
    unsafe_code,
    reason = "Win32 pointer authority requires current event-thread User32 queries"
)]
pub(super) fn probe(windows: &[(NativeViewportBinding, Arc<Window>)]) -> NativePointerRouteProbe {
    let Some(roster) = exact_roster(windows) else {
        return unknown_probe(NativeUnavailableReason::StaleSource);
    };

    let mut point = POINT { x: 0, y: 0 };
    let hovered = if unsafe { GetCursorPos(&mut point) } == 0 {
        NativeAuthority::unknown(NativeUnavailableReason::NotObserved)
    } else {
        let hovered = unsafe { WindowFromPoint(point) };
        if hovered.is_null() {
            NativeAuthority::known(NativeHoveredWindow::None)
        } else {
            let hovered = unsafe { GetAncestor(hovered, GA_ROOT) };
            NativeAuthority::known(
                roster
                    .get(&(hovered as usize))
                    .copied()
                    .map_or(NativeHoveredWindow::Foreign, NativeHoveredWindow::Viewport),
            )
        }
    };

    let capture = match unsafe { GetCapture() } {
        capture if capture.is_null() => {
            // `GetCapture` is thread-local. A null result cannot prove that a
            // window owned by another thread lacks global capture.
            NativeAuthority::unknown(NativeUnavailableReason::Unsupported)
        }
        capture => {
            let capture = unsafe { GetAncestor(capture, GA_ROOT) };
            NativeAuthority::known(
                roster
                    .get(&(capture as usize))
                    .copied()
                    .map_or(NativeCaptureOwner::Foreign, NativeCaptureOwner::Viewport),
            )
        }
    };

    NativePointerRouteProbe { hovered, capture }
}

pub(super) fn probe_event(
    windows: &[(NativeViewportBinding, Arc<Window>)],
) -> NativePointerRouteProbe {
    probe(windows)
}

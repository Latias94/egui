//! X11 pointer routing with bounded WM reparenting descent.

use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::c_void,
    os::raw::{c_int, c_uint, c_ulong},
    sync::Arc,
};

use raw_window_handle::{
    HasDisplayHandle as _, HasWindowHandle as _, RawDisplayHandle, RawWindowHandle,
};
use winit::window::Window;

use super::{
    NativeAuthority, NativeCaptureOwner, NativeHoveredWindow, NativePointerRouteProbe,
    NativeUnavailableReason, NativeViewportBinding, unknown_probe,
};

const MAX_POINTER_DESCENT: usize = 32;
const MAX_TREE_DEPTH: usize = 8;
const MAX_TREE_NODES: usize = 256;

#[link(name = "X11")]
unsafe extern "C" {
    fn XRootWindow(display: *mut c_void, screen_number: c_int) -> c_ulong;
    fn XQueryPointer(
        display: *mut c_void,
        window: c_ulong,
        root_return: *mut c_ulong,
        child_return: *mut c_ulong,
        root_x_return: *mut c_int,
        root_y_return: *mut c_int,
        win_x_return: *mut c_int,
        win_y_return: *mut c_int,
        mask_return: *mut c_uint,
    ) -> c_int;
    fn XQueryTree(
        display: *mut c_void,
        window: c_ulong,
        root_return: *mut c_ulong,
        parent_return: *mut c_ulong,
        children_return: *mut *mut c_ulong,
        child_count_return: *mut c_uint,
    ) -> c_int;
    fn XFree(data: *mut c_void) -> c_int;
}

struct X11Roster {
    display: *mut c_void,
    root: c_ulong,
    bindings: BTreeMap<c_ulong, NativeViewportBinding>,
}

#[expect(
    unsafe_code,
    reason = "X11 route authority requires validated raw display and window handles"
)]
fn exact_roster(windows: &[(NativeViewportBinding, Arc<Window>)]) -> Option<X11Roster> {
    let mut expected_display = None;
    let mut expected_screen = None;
    let mut bindings = BTreeMap::new();
    for (binding, window) in windows {
        let display_handle = window.display_handle().ok()?;
        let RawDisplayHandle::Xlib(display_handle) = display_handle.as_raw() else {
            return None;
        };
        let display = display_handle.display?.as_ptr();
        if expected_display.is_some_and(|expected| expected != display)
            || expected_screen.is_some_and(|expected| expected != display_handle.screen)
        {
            return None;
        }
        expected_display = Some(display);
        expected_screen = Some(display_handle.screen);

        let window_handle = window.window_handle().ok()?;
        let RawWindowHandle::Xlib(window_handle) = window_handle.as_raw() else {
            return None;
        };
        if bindings.insert(window_handle.window, *binding).is_some() {
            return None;
        }
    }
    let display = expected_display?;
    let root = unsafe { XRootWindow(display, expected_screen?) };
    (root != 0).then_some(X11Roster {
        display,
        root,
        bindings,
    })
}

#[expect(
    unsafe_code,
    reason = "XQueryPointer is the X server's event-time pointer hierarchy authority"
)]
fn pointer_child(roster: &X11Roster, window: c_ulong) -> Option<c_ulong> {
    let mut root = 0;
    let mut child = 0;
    let mut root_x = 0;
    let mut root_y = 0;
    let mut window_x = 0;
    let mut window_y = 0;
    let mut mask = 0;
    (unsafe {
        XQueryPointer(
            roster.display,
            window,
            &mut root,
            &mut child,
            &mut root_x,
            &mut root_y,
            &mut window_x,
            &mut window_y,
            &mut mask,
        )
    } != 0)
        .then_some(child)
}

#[expect(
    unsafe_code,
    reason = "XQueryTree allocates a bounded child roster released with XFree"
)]
fn window_children(roster: &X11Roster, window: c_ulong) -> Option<Vec<c_ulong>> {
    let mut root = 0;
    let mut parent = 0;
    let mut children = std::ptr::null_mut();
    let mut child_count = 0;
    let queried = unsafe {
        XQueryTree(
            roster.display,
            window,
            &mut root,
            &mut parent,
            &mut children,
            &mut child_count,
        )
    };
    if queried == 0 {
        return None;
    }
    let result = if children.is_null() || child_count == 0 {
        Vec::new()
    } else {
        unsafe { std::slice::from_raw_parts(children, child_count as usize) }.to_vec()
    };
    if !children.is_null() {
        let _ = unsafe { XFree(children.cast()) };
    }
    Some(result)
}

fn descendant_binding(roster: &X11Roster, start: c_ulong) -> Option<Option<NativeViewportBinding>> {
    let mut stack = vec![(start, 0_usize)];
    let mut visited = BTreeSet::new();
    while let Some((window, depth)) = stack.pop() {
        if !visited.insert(window) {
            continue;
        }
        if visited.len() > MAX_TREE_NODES {
            return Some(None);
        }
        if let Some(binding) = roster.bindings.get(&window) {
            return Some(Some(*binding));
        }
        if depth >= MAX_TREE_DEPTH {
            continue;
        }
        stack.extend(
            window_children(roster, window)?
                .into_iter()
                .map(|child| (child, depth + 1)),
        );
    }
    Some(None)
}

fn hovered(roster: &X11Roster) -> NativeAuthority<NativeHoveredWindow> {
    let Some(mut window) = pointer_child(roster, roster.root) else {
        return NativeAuthority::unknown(NativeUnavailableReason::NotObserved);
    };
    if window == 0 {
        return NativeAuthority::known(NativeHoveredWindow::None);
    }
    let pointer_root = window;
    for _ in 0..MAX_POINTER_DESCENT {
        if let Some(binding) = roster.bindings.get(&window) {
            return NativeAuthority::known(NativeHoveredWindow::Viewport(*binding));
        }
        let Some(child) = pointer_child(roster, window) else {
            return NativeAuthority::unknown(NativeUnavailableReason::NotObserved);
        };
        if child == 0 {
            break;
        }
        window = child;
    }

    match descendant_binding(roster, window).or_else(|| descendant_binding(roster, pointer_root)) {
        Some(Some(binding)) => NativeAuthority::known(NativeHoveredWindow::Viewport(binding)),
        Some(None) => NativeAuthority::known(NativeHoveredWindow::Foreign),
        None => NativeAuthority::unknown(NativeUnavailableReason::NotObserved),
    }
}

pub(super) fn probe(windows: &[(NativeViewportBinding, Arc<Window>)]) -> NativePointerRouteProbe {
    let Some(roster) = exact_roster(windows) else {
        return unknown_probe(NativeUnavailableReason::StaleSource);
    };
    NativePointerRouteProbe {
        hovered: hovered(&roster),
        // X11 provides no query for another client's active pointer grab. The
        // edge-local native delivery binding remains exact and independent.
        capture: NativeAuthority::unknown(NativeUnavailableReason::Unsupported),
    }
}

pub(super) fn probe_event(
    windows: &[(NativeViewportBinding, Arc<Window>)],
) -> NativePointerRouteProbe {
    probe(windows)
}

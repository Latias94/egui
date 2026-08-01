//! AppKit route probes using the application window order and exact input state.

use std::{collections::BTreeMap, sync::Arc};

use objc2::MainThreadMarker;
use objc2_app_kit::{NSApplication, NSEvent, NSView, NSWindow};
use raw_window_handle::{HasWindowHandle as _, RawWindowHandle};
use winit::window::Window;

use super::{
    NativeAuthority, NativeHoveredWindow, NativePointerRouteProbe, NativeUnavailableReason,
    NativeViewportBinding, unknown_probe,
};

#[expect(
    unsafe_code,
    reason = "raw-window-handle supplies a live NSView owned by the winit Window"
)]
fn native_window(window: &Window) -> Option<objc2::rc::Retained<NSWindow>> {
    let handle = window.window_handle().ok()?;
    let RawWindowHandle::AppKit(handle) = handle.as_raw() else {
        return None;
    };
    let view = unsafe { handle.ns_view.cast::<NSView>().as_ref() };
    view.window()
}

fn exact_roster(
    windows: &[(NativeViewportBinding, Arc<Window>)],
) -> Option<BTreeMap<isize, NativeViewportBinding>> {
    let mut roster = BTreeMap::new();
    for (binding, window) in windows {
        let number = native_window(window)?.windowNumber();
        if roster.insert(number, *binding).is_some() {
            return None;
        }
    }
    Some(roster)
}

fn point_is_inside(point: objc2_foundation::NSPoint, window: &NSWindow) -> bool {
    let frame = window.frame();
    point.x >= frame.origin.x
        && point.y >= frame.origin.y
        && point.x < frame.origin.x + frame.size.width
        && point.y < frame.origin.y + frame.size.height
}

fn hovered(
    mtm: MainThreadMarker,
    windows: &[(NativeViewportBinding, Arc<Window>)],
) -> NativeAuthority<NativeHoveredWindow> {
    let Some(roster) = exact_roster(windows) else {
        return NativeAuthority::unknown(NativeUnavailableReason::StaleSource);
    };
    let point = NSEvent::mouseLocation();
    let platform_hit = NSWindow::windowNumberAtPoint_belowWindowWithWindowNumber(point, 0, mtm);
    let app = NSApplication::sharedApplication(mtm);
    let ordered_hit = app.orderedWindows().iter().find(|window| {
        window.isVisible()
            && !window.isMiniaturized()
            && !window.ignoresMouseEvents()
            && point_is_inside(point, window)
    });

    match (platform_hit, ordered_hit) {
        (0, None) => NativeAuthority::known(NativeHoveredWindow::None),
        (0, Some(_)) => NativeAuthority::unknown(NativeUnavailableReason::StaleSource),
        (platform_hit, Some(window)) if platform_hit == window.windowNumber() => {
            NativeAuthority::known(
                roster
                    .get(&platform_hit)
                    .copied()
                    .map_or(NativeHoveredWindow::Foreign, NativeHoveredWindow::Viewport),
            )
        }
        // A foreign application window can be above our first eligible AppKit
        // window. `windowNumberAtPoint` is used only to prove that difference,
        // never as the sole eligibility test for one of our own windows.
        (_, _) => NativeAuthority::known(NativeHoveredWindow::Foreign),
    }
}

pub(super) fn probe(windows: &[(NativeViewportBinding, Arc<Window>)]) -> NativePointerRouteProbe {
    let Some(mtm) = MainThreadMarker::new() else {
        return unknown_probe(NativeUnavailableReason::NotObserved);
    };
    NativePointerRouteProbe {
        hovered: hovered(mtm, windows),
        // AppKit does not expose a persistent mouse-capture owner. Native
        // callbacks still provide exact edge-local delivery authority.
        capture: NativeAuthority::unknown(NativeUnavailableReason::Unsupported),
    }
}

pub(super) fn probe_event(
    windows: &[(NativeViewportBinding, Arc<Window>)],
) -> NativePointerRouteProbe {
    // This executes synchronously inside the winit callback, so the hover fact
    // is sampled at the edge rather than reconstructed at host-frame freeze.
    probe(windows)
}

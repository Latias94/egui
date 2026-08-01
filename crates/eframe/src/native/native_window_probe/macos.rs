//! AppKit-backed work-area and pointer-input observations.

use objc2::MainThreadMarker;
use objc2_app_kit::{NSView, NSWindow};
use raw_window_handle::{HasWindowHandle as _, RawWindowHandle};
use winit::window::Window;

use super::{
    NativeAuthority, NativePhysicalPoint, NativePhysicalRect, NativePointerInputState,
    NativeUnavailableReason, NativeWindowProbe, unknown_probe,
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

fn rounded_i32(value: f64) -> Option<i32> {
    let rounded = value.round();
    (rounded.is_finite() && rounded >= f64::from(i32::MIN) && rounded <= f64::from(i32::MAX))
        .then_some(rounded as i32)
}

fn work_area(window: &Window, native: &NSWindow) -> NativeAuthority<NativePhysicalRect> {
    let Some(screen) = native.screen() else {
        return NativeAuthority::unknown(NativeUnavailableReason::NotObserved);
    };
    let Ok(outer_origin) = window.outer_position() else {
        return NativeAuthority::unknown(NativeUnavailableReason::NotObserved);
    };
    let scale = native.backingScaleFactor();
    if !scale.is_finite() || scale <= 0.0 {
        return NativeAuthority::unknown(NativeUnavailableReason::StaleSource);
    }

    // AppKit uses bottom-left logical screen coordinates while winit exposes
    // top-left desktop-global physical coordinates. Calibrating against this
    // exact window avoids assuming a primary-screen origin or a shared DPI.
    let frame = native.frame();
    let visible = screen.visibleFrame();
    let Some(offset_x) = rounded_i32((visible.origin.x - frame.origin.x) * scale) else {
        return NativeAuthority::unknown(NativeUnavailableReason::StaleSource);
    };
    let frame_top = frame.origin.y + frame.size.height;
    let visible_top = visible.origin.y + visible.size.height;
    let Some(offset_y) = rounded_i32((frame_top - visible_top) * scale) else {
        return NativeAuthority::unknown(NativeUnavailableReason::StaleSource);
    };
    let Some(width) = rounded_i32(visible.size.width * scale) else {
        return NativeAuthority::unknown(NativeUnavailableReason::StaleSource);
    };
    let Some(height) = rounded_i32(visible.size.height * scale) else {
        return NativeAuthority::unknown(NativeUnavailableReason::StaleSource);
    };
    let Some(min_x) = outer_origin.x.checked_add(offset_x) else {
        return NativeAuthority::unknown(NativeUnavailableReason::StaleSource);
    };
    let Some(min_y) = outer_origin.y.checked_add(offset_y) else {
        return NativeAuthority::unknown(NativeUnavailableReason::StaleSource);
    };
    let (Some(max_x), Some(max_y)) = (min_x.checked_add(width), min_y.checked_add(height)) else {
        return NativeAuthority::unknown(NativeUnavailableReason::StaleSource);
    };
    if width <= 0 || height <= 0 {
        return NativeAuthority::unknown(NativeUnavailableReason::StaleSource);
    }

    NativeAuthority::known(NativePhysicalRect::new(
        NativePhysicalPoint::new(min_x, min_y),
        NativePhysicalPoint::new(max_x, max_y),
    ))
}

pub(super) fn probe(window: &Window) -> NativeWindowProbe {
    if MainThreadMarker::new().is_none() {
        return unknown_probe();
    }
    let Some(native) = native_window(window) else {
        return unknown_probe();
    };
    NativeWindowProbe {
        work_area: work_area(window, &native),
        pointer_input: NativeAuthority::known(if native.ignoresMouseEvents() {
            NativePointerInputState::PassThrough
        } else {
            NativePointerInputState::ReceivesInput
        }),
    }
}

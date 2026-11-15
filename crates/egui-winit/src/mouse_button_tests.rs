//! Button edges retain their own native position without synthesizing motion.

use super::*;
use egui::{Event, Modifiers, PointerButton};
use winit::event::MouseButton;

struct UnavailableDisplay;

impl HasDisplayHandle for UnavailableDisplay {
    fn display_handle(
        &self,
    ) -> Result<raw_window_handle::DisplayHandle<'_>, raw_window_handle::HandleError> {
        Err(raw_window_handle::HandleError::NotSupported)
    }
}

fn state() -> State {
    State::new(
        egui::Context::default(),
        ViewportId::ROOT,
        &UnavailableDisplay,
        None,
        None,
        None,
    )
}

#[test]
fn replacement_button_position_overrides_the_previous_cursor_at_each_ui_scale_without_a_move() {
    for scale in [1.0, 1.25, 2.0, 2.5] {
        let mut state = state();
        let previous = egui::pos2(10.0, 15.0);
        state.pointer_pos_in_points = Some(previous);
        state.on_mouse_button_input(ElementState::Pressed, MouseButton::Left, None, scale);
        let output = state.egui_ctx.run_ui(state.egui_input.take(), |ui| {
            ui.input(|input| {
                assert!(input.pointer.primary_down());
                assert_eq!(input.pointer.press_origin(), Some(previous));
            });
        });
        output.drop_without_applying_deltas();

        state.modifiers = Modifiers::SHIFT;
        state.on_mouse_button_input(
            ElementState::Pressed,
            MouseButton::Left,
            Some(PhysicalPosition::new(120.0, 240.0)),
            scale,
        );
        let position = egui::pos2(120.0 / scale, 240.0 / scale);
        assert_eq!(
            state.egui_input.events,
            [Event::PointerButton {
                pos: position,
                button: PointerButton::Primary,
                pressed: true,
                modifiers: Modifiers::SHIFT,
            }],
        );
        let output = state.egui_ctx.run_ui(state.egui_input.take(), |ui| {
            ui.input(|input| {
                assert_eq!(input.pointer.press_origin(), Some(position));
                assert_eq!(input.pointer.latest_pos(), Some(position));
                assert!(input.pointer.primary_down());
            });
        });
        output.drop_without_applying_deltas();
    }
}

#[test]
fn an_outside_release_after_cursor_left_reaches_egui_without_a_cached_position() {
    let mut state = state();
    let press = egui::pos2(20.0, 30.0);
    state.pointer_pos_in_points = Some(press);
    state.on_mouse_button_input(ElementState::Pressed, MouseButton::Left, None, 2.5);
    let output = state.egui_ctx.run_ui(state.egui_input.take(), |ui| {
        assert!(ui.input(|input| input.pointer.primary_down()));
    });
    output.drop_without_applying_deltas();

    // CursorLeft removes hover position; the real MouseUp still owns its local sample.
    state.pointer_pos_in_points = None;
    state.egui_input.events.push(Event::PointerGone);
    state.on_mouse_button_input(
        ElementState::Released,
        MouseButton::Left,
        Some(PhysicalPosition::new(-50.0, 1_200.0)),
        2.5,
    );
    let position = egui::pos2(-20.0, 480.0);
    assert_eq!(
        state.egui_input.events,
        [
            Event::PointerGone,
            Event::PointerButton {
                pos: position,
                button: PointerButton::Primary,
                pressed: false,
                modifiers: Modifiers::NONE,
            },
        ],
    );
    let output = state.egui_ctx.run_ui(state.egui_input.take(), |ui| {
        ui.input(|input| {
            assert!(!input.pointer.any_down());
            assert!(input.pointer.any_released());
            assert_eq!(input.pointer.latest_pos(), Some(position));
        });
    });
    output.drop_without_applying_deltas();
    assert!(state.pointer_pos_in_points.is_none());
}

#[test]
fn callbacks_without_button_position_preserve_the_existing_cache_fallback() {
    let mut state = state();
    let position = egui::pos2(40.0, 60.0);
    state.pointer_pos_in_points = Some(position);
    for edge in [ElementState::Pressed, ElementState::Released] {
        state.on_mouse_button_input(edge, MouseButton::Right, None, 2.0);
    }
    assert_eq!(
        state.egui_input.events,
        [
            Event::PointerButton {
                pos: position,
                button: PointerButton::Secondary,
                pressed: true,
                modifiers: Modifiers::NONE,
            },
            Event::PointerButton {
                pos: position,
                button: PointerButton::Secondary,
                pressed: false,
                modifiers: Modifiers::NONE,
            },
        ],
    );
    state.egui_input.events.clear();
    state.pointer_pos_in_points = None;
    for edge in [ElementState::Pressed, ElementState::Released] {
        state.on_mouse_button_input(edge, MouseButton::Left, None, 2.0);
    }
    assert!(state.egui_input.events.is_empty());
}

#[test]
fn simulated_touch_uses_the_exact_button_position_without_an_extra_move() {
    let mut state = state();
    state.simulate_touch_screen = true;
    state.on_mouse_button_input(
        ElementState::Pressed,
        MouseButton::Left,
        Some(PhysicalPosition::new(80.0, 120.0)),
        2.0,
    );
    state.on_mouse_button_input(
        ElementState::Released,
        MouseButton::Left,
        Some(PhysicalPosition::new(140.0, 180.0)),
        2.0,
    );
    let start = egui::pos2(40.0, 60.0);
    let end = egui::pos2(70.0, 90.0);
    assert_eq!(
        state.egui_input.events,
        [
            Event::PointerButton {
                pos: start,
                button: PointerButton::Primary,
                pressed: true,
                modifiers: Modifiers::NONE,
            },
            Event::Touch {
                device_id: egui::TouchDeviceId(0),
                id: egui::TouchId(0),
                phase: egui::TouchPhase::Start,
                pos: start,
                force: None,
            },
            Event::PointerButton {
                pos: end,
                button: PointerButton::Primary,
                pressed: false,
                modifiers: Modifiers::NONE,
            },
            Event::PointerGone,
            Event::Touch {
                device_id: egui::TouchDeviceId(0),
                id: egui::TouchId(0),
                phase: egui::TouchPhase::End,
                pos: end,
                force: None,
            },
        ],
    );
    assert!(!state.is_any_pointer_button_down());
    assert!(state.simulated_touch_pos_in_points.is_none());
}

#[test]
fn capture_loss_cancels_ui_buttons_without_synthesizing_a_release() {
    let mut state = state();
    state.on_mouse_button_input(
        ElementState::Pressed,
        MouseButton::Left,
        Some(PhysicalPosition::new(80.0, 120.0)),
        2.0,
    );
    let output = state.egui_ctx.run_ui(state.egui_input.take(), |ui| {
        assert!(ui.input(|input| input.pointer.primary_down()));
    });
    output.drop_without_applying_deltas();
    state.cancel_pointer_input();
    assert!(!state.is_any_pointer_button_down());
    assert_eq!(state.egui_input.events, [Event::PointerCancelled]);
    let output = state.egui_ctx.run_ui(state.egui_input.take(), |ui| {
        ui.input(|input| {
            assert!(!input.pointer.any_down());
            assert!(!input.pointer.any_released());
            assert!(!input.pointer.any_click());
        });
    });
    output.drop_without_applying_deltas();
}

#[test]
fn cancellation_is_queued_for_its_viewport_without_mutating_the_current_viewport() {
    let mut root = state();
    root.on_mouse_button_input(
        ElementState::Pressed,
        MouseButton::Left,
        Some(PhysicalPosition::new(80.0, 120.0)),
        2.0,
    );
    let output = root.egui_ctx.run_ui(root.egui_input.take(), |ui| {
        assert!(ui.input(|input| input.pointer.primary_down()));
    });
    output.drop_without_applying_deltas();

    let child_id = ViewportId::from_hash_of("cancelled-child");
    let mut child = State::new(
        root.egui_ctx.clone(),
        child_id,
        &UnavailableDisplay,
        None,
        None,
        None,
    );
    child.cancel_pointer_input();
    assert_eq!(child.viewport_id, child_id);
    assert_eq!(child.egui_input.events, [Event::PointerCancelled]);
    root.egui_ctx
        .input(|input| assert!(input.pointer.primary_down()));
    // take_egui_input normally stamps this field using the native window.
    let mut input = child.egui_input.take();
    input.viewport_id = child.viewport_id;
    input.viewports.entry(child.viewport_id).or_default();
    let output = child.egui_ctx.run_ui(input, |ui| {
        assert_eq!(ui.ctx().viewport_id(), child_id);
        assert!(!ui.input(|input| input.pointer.any_down()));
    });
    output.drop_without_applying_deltas();
    let output = root.egui_ctx.run_ui(root.egui_input.take(), |ui| {
        assert!(ui.input(|input| input.pointer.primary_down()));
    });
    output.drop_without_applying_deltas();
}

#[test]
fn simulated_touch_cancellation_after_cursor_left_ends_the_active_contact_without_touch_end() {
    let mut state = state();
    state.simulate_touch_screen = true;
    let position = egui::pos2(40.0, 60.0);
    state.on_mouse_button_input(
        ElementState::Pressed,
        MouseButton::Left,
        Some(PhysicalPosition::new(80.0, 120.0)),
        2.0,
    );
    let output = state.egui_ctx.run_ui(state.egui_input.take(), |ui| {
        assert!(ui.input(|input| input.any_touches()));
    });
    output.drop_without_applying_deltas();
    state.pointer_pos_in_points = None;
    state.egui_input.events.push(Event::PointerGone);
    state.cancel_pointer_input();
    assert_eq!(
        state.egui_input.events,
        [
            Event::PointerGone,
            Event::PointerCancelled,
            Event::Touch {
                device_id: egui::TouchDeviceId(0),
                id: egui::TouchId(0),
                phase: egui::TouchPhase::Cancel,
                pos: position,
                force: None,
            },
        ],
    );
    assert!(!state.is_any_pointer_button_down());
    assert!(state.simulated_touch_pos_in_points.is_none());
    let output = state.egui_ctx.run_ui(state.egui_input.take(), |ui| {
        ui.input(|input| {
            assert!(!input.any_touches());
            assert!(!input.pointer.any_down());
            assert!(!input.pointer.any_released());
        });
    });
    output.drop_without_applying_deltas();
    state.cancel_pointer_input();
    assert_eq!(state.egui_input.events, [Event::PointerCancelled]);
}

#[test]
fn cancellation_without_a_simulated_contact_preserves_real_touches() {
    let mut state = state();
    state.simulate_touch_screen = true;
    state.pointer_touch_id = Some(7);
    let position = egui::pos2(40.0, 60.0);
    state.pointer_pos_in_points = Some(position);
    state.egui_input.events.push(Event::Touch {
        device_id: egui::TouchDeviceId(42),
        id: egui::TouchId(7),
        phase: egui::TouchPhase::Start,
        pos: position,
        force: None,
    });
    let output = state.egui_ctx.run_ui(state.egui_input.take(), |ui| {
        assert!(ui.input(|input| input.any_touches()));
    });
    output.drop_without_applying_deltas();

    state.cancel_pointer_input();
    assert_eq!(state.egui_input.events, [Event::PointerCancelled]);
    assert_eq!(state.pointer_touch_id, Some(7));
    let output = state.egui_ctx.run_ui(state.egui_input.take(), |ui| {
        assert!(ui.input(|input| input.any_touches()));
    });
    output.drop_without_applying_deltas();
    state.egui_input.events.push(Event::Touch {
        device_id: egui::TouchDeviceId(42),
        id: egui::TouchId(7),
        phase: egui::TouchPhase::Cancel,
        pos: position,
        force: None,
    });
    let output = state.egui_ctx.run_ui(state.egui_input.take(), |ui| {
        assert!(!ui.input(|input| input.any_touches()));
    });
    output.drop_without_applying_deltas();
}

#[test]
fn cancelled_state_accepts_an_exact_fresh_press_and_release_in_the_same_input_batch() {
    fn show(state: &mut State) -> egui::Response {
        let mut response = None;
        let mut input = state.egui_input.take();
        input.screen_rect = Some(Rect::from_min_size(Pos2::ZERO, egui::vec2(400.0, 300.0)));
        let output = state.egui_ctx.run_ui(input, |ui| {
            response =
                Some(ui.allocate_response(egui::vec2(180.0, 100.0), egui::Sense::click_and_drag()));
        });
        output.drop_without_applying_deltas();
        response.unwrap()
    }

    let mut state = state();
    let position = show(&mut state).rect.center();
    state.on_mouse_button_input(
        ElementState::Pressed,
        MouseButton::Left,
        Some(PhysicalPosition::new(
            f64::from(position.x * 2.0),
            f64::from(position.y * 2.0),
        )),
        2.0,
    );
    assert!(show(&mut state).is_pointer_button_down_on());

    state.cancel_pointer_input();
    state.pointer_pos_in_points = None;
    for edge in [ElementState::Pressed, ElementState::Released] {
        state.on_mouse_button_input(
            edge,
            MouseButton::Left,
            Some(PhysicalPosition::new(
                f64::from(position.x * 2.0),
                f64::from(position.y * 2.0),
            )),
            2.0,
        );
    }
    assert!(matches!(
        state.egui_input.events.as_slice(),
        [
            Event::PointerCancelled,
            Event::PointerButton { pressed: true, .. },
            Event::PointerButton { pressed: false, .. },
        ]
    ));
    let clicked = show(&mut state);
    assert!(
        clicked.clicked(),
        "the fresh press and release own their click"
    );
    assert!(!clicked.is_pointer_button_down_on());
    assert!(!clicked.dragged());
    state.egui_ctx.input(|input| {
        assert!(!input.pointer.any_down());
        assert_eq!(input.pointer.latest_pos(), Some(position));
    });
}

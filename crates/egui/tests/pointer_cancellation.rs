use egui::{
    Context, DragAndDrop, Event, Modifiers, PointerButton, RawInput, Rect, Sense, pos2, vec2,
};

fn button(point: egui::Pos2, button: PointerButton, pressed: bool) -> Event {
    Event::PointerButton {
        pos: point,
        button,
        pressed,
        modifiers: Modifiers::NONE,
    }
}

fn show(ctx: &Context, events: Vec<Event>) -> egui::Response {
    show_viewport(ctx, egui::ViewportId::ROOT, events)
}

fn show_viewport(
    ctx: &Context,
    viewport_id: egui::ViewportId,
    events: Vec<Event>,
) -> egui::Response {
    let mut response = None;
    let mut input = RawInput {
        viewport_id,
        screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(400.0, 300.0))),
        events,
        ..Default::default()
    };
    input.viewports.entry(viewport_id).or_default();
    let output = ctx.run_ui(input, |ui| {
        response = Some(ui.allocate_response(vec2(180.0, 100.0), Sense::click_and_drag()));
    });
    output.drop_without_applying_deltas();
    response.unwrap()
}

#[test]
fn cancelled_content_drag_rejects_late_motion_and_release_then_accepts_a_fresh_press() {
    let ctx = Context::default();
    let point = show(&ctx, vec![]).rect.center();
    show(
        &ctx,
        vec![
            Event::PointerMoved(point),
            button(point, PointerButton::Primary, true),
        ],
    );
    let dragged = point + vec2(30.0, 0.0);
    assert!(show(&ctx, vec![Event::PointerMoved(dragged)]).dragged());
    DragAndDrop::set_payload(&ctx, 42_u32);

    let cancelled = show(&ctx, vec![Event::PointerCancelled]);
    assert!(!cancelled.dragged());
    assert!(!cancelled.clicked());
    ctx.input(|input| {
        assert!(!input.pointer.any_down());
        assert!(
            !input.pointer.any_released(),
            "cancellation is not a physical release"
        );
        assert!(!input.pointer.any_click());
    });
    assert!(!DragAndDrop::has_any_payload(&ctx));
    let late = show(
        &ctx,
        vec![
            Event::PointerMoved(point),
            button(point, PointerButton::Primary, false),
        ],
    );
    assert!(!late.dragged());
    assert!(!late.clicked());
    show(&ctx, vec![button(point, PointerButton::Primary, true)]);
    assert!(show(&ctx, vec![button(point, PointerButton::Primary, false)]).clicked());
}

#[test]
fn cancellation_clears_combined_ui_buttons_and_cannot_turn_a_pending_press_into_a_click() {
    let ctx = Context::default();
    let point = show(&ctx, vec![]).rect.center();
    show(
        &ctx,
        vec![
            button(point, PointerButton::Primary, true),
            button(point, PointerButton::Secondary, true),
        ],
    );
    let cancelled = show(
        &ctx,
        vec![Event::PointerCancelled, Event::PointerMoved(point)],
    );
    assert!(!cancelled.clicked());
    ctx.input(|input| assert!(!input.pointer.any_down()));
    for which in [PointerButton::Secondary, PointerButton::Primary] {
        assert!(!show(&ctx, vec![button(point, which, false)]).clicked());
    }
}

#[test]
fn cancellation_followed_by_a_fresh_press_in_the_same_pass_keeps_the_new_interaction() {
    let ctx = Context::default();
    let point = show(&ctx, vec![]).rect.center();
    show(&ctx, vec![button(point, PointerButton::Primary, true)]);
    assert!(show(&ctx, vec![Event::PointerMoved(point + vec2(30.0, 0.0))]).dragged());

    let replacement = show(
        &ctx,
        vec![
            Event::PointerCancelled,
            button(point, PointerButton::Primary, true),
        ],
    );
    assert!(replacement.is_pointer_button_down_on());
    assert!(!replacement.clicked());
    ctx.input(|input| assert!(input.pointer.primary_down()));
    assert!(show(&ctx, vec![button(point, PointerButton::Primary, false)]).clicked());
}

#[test]
fn cancellation_in_another_viewport_preserves_the_source_drag_payload() {
    let ctx = Context::default();
    let point = show(&ctx, vec![]).rect.center();
    show(&ctx, vec![button(point, PointerButton::Primary, true)]);
    assert!(show(&ctx, vec![Event::PointerMoved(point + vec2(30.0, 0.0))]).dragged());
    DragAndDrop::set_payload(&ctx, 42_u32);

    let other = egui::ViewportId::from_hash_of("unrelated-cancelled-viewport");
    show_viewport(&ctx, other, vec![Event::PointerCancelled]);
    assert_eq!(DragAndDrop::payload::<u32>(&ctx).as_deref(), Some(&42));

    show(&ctx, vec![Event::PointerCancelled]);
    assert!(!DragAndDrop::has_any_payload(&ctx));
}

#[test]
fn an_old_secondary_release_does_not_end_a_new_primary_press_or_its_drag_payload() {
    let ctx = Context::default();
    let point = show(&ctx, vec![]).rect.center();
    show(
        &ctx,
        vec![
            button(point, PointerButton::Primary, true),
            button(point, PointerButton::Secondary, true),
        ],
    );
    show(&ctx, vec![Event::PointerCancelled]);
    assert!(
        show(&ctx, vec![button(point, PointerButton::Primary, true)]).is_pointer_button_down_on()
    );
    DragAndDrop::set_payload(&ctx, 42_u32);

    let late = show(&ctx, vec![button(point, PointerButton::Secondary, false)]);
    assert!(!late.secondary_clicked());
    assert!(late.is_pointer_button_down_on());
    ctx.input(|input| {
        assert!(input.pointer.primary_down());
        assert!(!input.pointer.any_released());
    });
    assert!(DragAndDrop::has_any_payload(&ctx));
    assert!(show(&ctx, vec![button(point, PointerButton::Primary, false)]).clicked());
}

#[test]
fn a_release_in_a_new_viewport_still_ends_cross_viewport_drag_and_drop() {
    let ctx = Context::default();
    let point = show(&ctx, vec![]).rect.center();
    show(&ctx, vec![button(point, PointerButton::Primary, true)]);
    DragAndDrop::set_payload(&ctx, 42_u32);
    let other = egui::ViewportId::from_hash_of("drop-target");
    show_viewport(
        &ctx,
        other,
        vec![button(point, PointerButton::Primary, false)],
    );
    assert!(!DragAndDrop::has_any_payload(&ctx));
}

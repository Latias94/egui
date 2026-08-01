use crate::{
    Area, Context, Event, FullOutput, Id, LayerId, Modal, Modifiers, Order, PaintFailure,
    PaintOutcome, PaintSkipReason, PointerButton, PointerReceiverAuthority,
    PointerReceiverUnavailableReason, PointerRoute, RawInput, Rect, Sense, UserData, pos2, vec2,
};

const BACKGROUND: &str = "presented_hit_graph_background";
const REPLACEMENT: &str = "presented_hit_graph_replacement";
const OVERLAY: &str = "presented_hit_graph_overlay";

fn screen_rect() -> Rect {
    Rect::from_min_max(pos2(0.0, 0.0), pos2(240.0, 160.0))
}

fn target_rect() -> Rect {
    Rect::from_min_max(pos2(10.0, 10.0), pos2(90.0, 90.0))
}

fn raw(events: Vec<Event>) -> RawInput {
    RawInput {
        screen_rect: Some(screen_rect()),
        events: events.into_iter().map(Into::into).collect(),
        ..Default::default()
    }
}

fn press(position: crate::Pos2) -> Event {
    Event::PointerButton {
        pos: position,
        button: PointerButton::Primary,
        pressed: true,
        modifiers: Modifiers::NONE,
    }
}

fn register_target(ui: &crate::Ui, id: &'static str) {
    let _ = ui.interact(target_rect(), Id::new(id), Sense::click_and_drag());
}

fn run_pass(
    context: &Context,
    events: Vec<Event>,
    mut ui: impl FnMut(&mut crate::Ui),
) -> FullOutput {
    context.run_ui(raw(events), |root| ui(root))
}

fn click_receiver_id(authority: PointerReceiverAuthority<crate::PointerHit>) -> Option<Id> {
    match authority {
        PointerReceiverAuthority::Known(hit) => hit.click_receiver.map(|receiver| receiver.id),
        PointerReceiverAuthority::Unknown(_) => None,
    }
}

#[test]
fn completed_but_unpresented_previous_pass_is_unknown() {
    let context = Context::default();
    let _unpresented = run_pass(&context, vec![], |ui| register_target(ui, BACKGROUND));
    let next = run_pass(&context, vec![press(target_rect().center())], |ui| {
        register_target(ui, BACKGROUND);
    });

    let Some(record) = next.pointer_receiver_journal.records.first() else {
        panic!("expected one pointer receiver record");
    };
    assert_eq!(
        record.hit,
        PointerReceiverAuthority::Unknown(
            PointerReceiverUnavailableReason::PreviousPassNotPresented
        )
    );
    assert_eq!(record.route_after, PointerRoute::default());
}

#[test]
fn successful_promotion_authorizes_immediately_following_pass() {
    let context = Context::default();
    let first = run_pass(&context, vec![], |ui| register_target(ui, BACKGROUND));
    let Some(candidate) = first.pointer_hit_graph_candidate else {
        panic!("completed pass must emit a hit graph candidate");
    };
    assert!(candidate.settle(&PaintOutcome::SubmittedToSwapchain));

    let next = run_pass(&context, vec![press(target_rect().center())], |ui| {
        register_target(ui, BACKGROUND);
    });
    let Some(record) = next.pointer_receiver_journal.records.first() else {
        panic!("expected one pointer receiver record");
    };
    let PointerReceiverAuthority::Known(hit) = &record.hit else {
        panic!("successful presentation must authorize the next pass");
    };
    assert_eq!(hit.widget_pass_nr, 0);
    assert_eq!(
        hit.click_receiver.map(|receiver| receiver.id),
        Some(Id::new(BACKGROUND))
    );
    assert_eq!(
        hit.drag_receiver.map(|receiver| receiver.id),
        Some(Id::new(BACKGROUND))
    );
    assert_ne!(record.route_after, PointerRoute::default());
}

#[test]
fn browser_canvas_submission_promotes_without_claiming_compositor_visibility() {
    let context = Context::default();
    let output = run_pass(&context, vec![], |ui| register_target(ui, BACKGROUND));
    let Some(candidate) = output.pointer_hit_graph_candidate else {
        panic!("completed pass must emit a hit graph candidate");
    };

    assert!(candidate.settle(&PaintOutcome::SubmittedToBrowserCanvas));
    assert!(
        context
            .presented_pointer_hit_graph_for(crate::ViewportId::ROOT)
            .is_some()
    );
}

#[test]
fn explicit_headless_acceptance_promotes_without_a_renderer_outcome() {
    let context = Context::default();
    let output = run_pass(&context, vec![], |ui| register_target(ui, BACKGROUND));
    let Some(candidate) = output.pointer_hit_graph_candidate else {
        panic!("completed pass must emit a hit graph candidate");
    };

    assert!(candidate.accept_for_headless_host());
    assert!(
        context
            .presented_pointer_hit_graph_for(crate::ViewportId::ROOT)
            .is_some()
    );
}

#[test]
fn context_exposes_only_the_successfully_presented_graph_for_one_viewport() {
    let context = Context::default();
    assert!(
        context
            .presented_pointer_hit_graph_for(crate::ViewportId::ROOT)
            .is_none()
    );

    let output = run_pass(&context, vec![], |ui| register_target(ui, BACKGROUND));
    let Some(candidate) = output.pointer_hit_graph_candidate else {
        panic!("completed pass must emit a hit graph candidate");
    };
    assert!(candidate.settle(&PaintOutcome::SubmittedToSwapchain));

    let graph = context
        .presented_pointer_hit_graph_for(crate::ViewportId::ROOT)
        .expect("successful settlement must install a graph");
    assert_eq!(graph.viewport_id(), crate::ViewportId::ROOT);
    assert_eq!(graph.widget_pass_nr(), 0);
    assert_eq!(graph.native_pixels_per_point(), 1.0);
    assert_eq!(graph.pixels_per_point(), 1.0);
    assert!(
        context
            .presented_pointer_hit_graph_for(crate::ViewportId::from_hash_of("other"))
            .is_none()
    );
}

#[test]
fn presented_graph_retains_the_exact_focused_widget_receiver() {
    let context = Context::default();
    let output = run_pass(&context, vec![], |ui| {
        ui.interact(target_rect(), Id::new(BACKGROUND), Sense::click_and_drag())
            .request_focus();
    });
    let candidate = output
        .pointer_hit_graph_candidate
        .expect("completed pass must emit a hit graph candidate");
    assert!(candidate.settle(&PaintOutcome::SubmittedToSwapchain));

    let focused = context
        .presented_pointer_hit_graph_for(crate::ViewportId::ROOT)
        .and_then(|graph| graph.focused_receiver())
        .expect("the presented widget roster contains the focused receiver");
    assert_eq!(focused.id, Id::new(BACKGROUND));
    assert_eq!(focused.interact_rect, target_rect());
    assert!(focused.sense.senses_click());
    assert!(focused.sense.senses_drag());
}

#[test]
fn presented_graph_resolves_an_accesskit_target_from_the_same_widget_roster() {
    let context = Context::default();
    let output = run_pass(&context, vec![], |ui| register_target(ui, BACKGROUND));
    let candidate = output
        .pointer_hit_graph_candidate
        .expect("completed pass must emit a hit graph candidate");
    assert!(candidate.settle(&PaintOutcome::SubmittedToSwapchain));

    let graph = context
        .presented_pointer_hit_graph_for(crate::ViewportId::ROOT)
        .expect("successful settlement must install a graph");
    let receiver = graph
        .receiver_for_accesskit_node(Id::new(BACKGROUND).accesskit_id())
        .expect("the exact presented widget roster contains the target");
    assert_eq!(receiver.id, Id::new(BACKGROUND));
    assert!(
        graph
            .receiver_for_accesskit_node(Id::new("foreign").accesskit_id())
            .is_none()
    );
}

#[test]
fn snapshot_keeps_native_scale_distinct_from_egui_zoom() {
    let context = Context::default();
    context.set_zoom_factor(1.25);
    let mut input = raw(Vec::new());
    input
        .viewports
        .entry(crate::ViewportId::ROOT)
        .or_default()
        .native_pixels_per_point = Some(2.0);

    let output = context.run_ui(input, |ui| register_target(ui, BACKGROUND));
    let graph = output
        .pointer_hit_graph_candidate
        .expect("completed pass must emit a hit graph candidate")
        .snapshot()
        .clone();

    assert_eq!(graph.native_pixels_per_point(), 2.0);
    assert_eq!(graph.pixels_per_point(), 2.5);
}

#[test]
fn failed_candidate_cannot_authorize_or_be_resurrected() {
    let context = Context::default();
    let first = run_pass(&context, vec![], |ui| register_target(ui, BACKGROUND));
    let Some(first_candidate) = first.pointer_hit_graph_candidate else {
        panic!("completed pass must emit a hit graph candidate");
    };
    assert!(first_candidate.settle(&PaintOutcome::Swapped));

    let failed = run_pass(&context, vec![], |ui| register_target(ui, REPLACEMENT));
    let Some(failed_candidate) = failed.pointer_hit_graph_candidate else {
        panic!("completed pass must emit a hit graph candidate");
    };
    let retained_clone = failed_candidate.clone();
    let retained_headless_clone = failed_candidate.clone();
    assert!(!failed_candidate.settle(&PaintOutcome::Failed(PaintFailure::RendererUnavailable)));
    assert!(!retained_clone.settle(&PaintOutcome::Swapped));
    assert!(!retained_headless_clone.accept_for_headless_host());

    let next = run_pass(&context, vec![press(target_rect().center())], |ui| {
        register_target(ui, REPLACEMENT);
    });
    let Some(record) = next.pointer_receiver_journal.records.first() else {
        panic!("expected one pointer receiver record");
    };
    assert_eq!(
        record.hit,
        PointerReceiverAuthority::Unknown(
            PointerReceiverUnavailableReason::PreviousPassNotPresented
        )
    );
    assert_eq!(record.route_after, PointerRoute::default());
}

#[test]
fn skipped_candidate_leaves_the_following_pass_unknown() {
    let context = Context::default();
    let skipped = run_pass(&context, vec![], |ui| register_target(ui, BACKGROUND));
    let Some(candidate) = skipped.pointer_hit_graph_candidate else {
        panic!("completed pass must emit a hit graph candidate");
    };
    let retained_headless_clone = candidate.clone();
    assert!(!candidate.settle(&PaintOutcome::Skipped(PaintSkipReason::NotVisible)));
    assert!(!retained_headless_clone.accept_for_headless_host());

    let next = run_pass(&context, vec![press(target_rect().center())], |ui| {
        register_target(ui, BACKGROUND);
    });
    let Some(record) = next.pointer_receiver_journal.records.first() else {
        panic!("expected one pointer receiver record");
    };
    assert_eq!(
        record.hit,
        PointerReceiverAuthority::Unknown(
            PointerReceiverUnavailableReason::PreviousPassNotPresented
        )
    );
    assert_eq!(record.route_after, PointerRoute::default());
}

#[test]
fn snapshot_prefers_real_foreground_area_for_each_receiver_lane() {
    let context = Context::default();
    let overlay_area = Id::new("presented_hit_graph_overlay_area");
    let output = run_pass(&context, vec![], |ui| {
        register_target(ui, BACKGROUND);
        Area::new(overlay_area)
            .order(Order::Foreground)
            .movable(true)
            .sense(Sense::click_and_drag())
            .fixed_pos(pos2(20.0, 20.0))
            .show(ui.ctx(), |ui| {
                ui.set_min_size(vec2(50.0, 50.0));
                let rect = Rect::from_min_size(ui.min_rect().min, vec2(50.0, 50.0));
                let _ = ui.interact(rect, Id::new(OVERLAY), Sense::click_and_drag());
            });
    });
    let Some(candidate) = output.pointer_hit_graph_candidate else {
        panic!("completed pass must emit a hit graph candidate");
    };
    let PointerReceiverAuthority::Known(hit) = candidate.snapshot().probe(pos2(30.0, 30.0)) else {
        panic!("finite point probe must be known");
    };
    let overlay_layer = LayerId::new(Order::Foreground, overlay_area);

    assert_eq!(hit.blocking_layer, Some(overlay_layer));
    assert_eq!(
        hit.top_widget.map(|receiver| receiver.id),
        Some(Id::new(OVERLAY))
    );
    assert_eq!(
        hit.click_receiver.map(|receiver| receiver.layer_id),
        Some(overlay_layer)
    );
    assert_eq!(
        hit.drag_receiver.map(|receiver| receiver.layer_id),
        Some(overlay_layer)
    );
}

#[test]
fn snapshot_retains_same_layer_receivers_back_to_front() {
    let context = Context::default();
    let output = run_pass(&context, vec![], |ui| {
        register_target(ui, BACKGROUND);
        register_target(ui, OVERLAY);
    });
    let Some(candidate) = output.pointer_hit_graph_candidate else {
        panic!("completed pass must emit a hit graph candidate");
    };
    let PointerReceiverAuthority::Known(hit) = candidate.snapshot().probe(target_rect().center())
    else {
        panic!("finite point probe must be known");
    };
    let containing_ids = hit
        .containing_receivers
        .iter()
        .map(|receiver| receiver.id)
        .collect::<Vec<_>>();
    let background_index = containing_ids
        .iter()
        .position(|id| *id == Id::new(BACKGROUND))
        .expect("the lower receiver remains in the hit stack");
    let overlay_index = containing_ids
        .iter()
        .position(|id| *id == Id::new(OVERLAY))
        .expect("the upper receiver remains in the hit stack");

    assert!(
        background_index < overlay_index,
        "the receiver stack must be ordered back-to-front"
    );
    assert_eq!(
        hit.top_widget.map(|receiver| receiver.id),
        Some(Id::new(OVERLAY))
    );
}

#[test]
fn snapshot_freezes_modal_backdrop_as_the_blocker() {
    let context = Context::default();
    let modal_id = Id::new("presented_hit_graph_modal");
    let show = |ui: &mut crate::Ui| {
        register_target(ui, BACKGROUND);
        Modal::new(modal_id).show(ui.ctx(), |ui| {
            ui.label("modal content");
        });
    };

    let warmup = run_pass(&context, vec![], show);
    let Some(warmup_candidate) = warmup.pointer_hit_graph_candidate else {
        panic!("completed pass must emit a hit graph candidate");
    };
    assert!(warmup_candidate.settle(&PaintOutcome::Swapped));
    let output = run_pass(&context, vec![], show);
    let Some(candidate) = output.pointer_hit_graph_candidate else {
        panic!("completed pass must emit a hit graph candidate");
    };
    let PointerReceiverAuthority::Known(hit) = candidate.snapshot().probe(pos2(15.0, 15.0)) else {
        panic!("finite point probe must be known");
    };
    let modal_layer = LayerId::new(Order::Foreground, modal_id);

    assert_eq!(hit.blocking_layer, Some(modal_layer));
    assert_ne!(
        hit.click_receiver.map(|receiver| receiver.id),
        Some(Id::new(BACKGROUND))
    );
    assert_ne!(
        hit.drag_receiver.map(|receiver| receiver.id),
        Some(Id::new(BACKGROUND))
    );
}

#[test]
fn snapshot_is_immutable_after_context_advances() {
    let context = Context::default();
    let first = run_pass(&context, vec![], |ui| register_target(ui, BACKGROUND));
    let Some(first_candidate) = first.pointer_hit_graph_candidate else {
        panic!("completed pass must emit a hit graph candidate");
    };
    let first_snapshot = first_candidate.snapshot().clone();
    assert!(first_candidate.settle(&PaintOutcome::Swapped));

    let second = run_pass(&context, vec![], |ui| register_target(ui, REPLACEMENT));
    let Some(second_candidate) = second.pointer_hit_graph_candidate else {
        panic!("completed pass must emit a hit graph candidate");
    };

    assert_eq!(
        click_receiver_id(first_snapshot.probe(target_rect().center())),
        Some(Id::new(BACKGROUND))
    );
    assert_eq!(
        click_receiver_id(second_candidate.snapshot().probe(target_rect().center())),
        Some(Id::new(REPLACEMENT))
    );
}

#[test]
fn full_output_append_keeps_only_the_latest_candidate() {
    let context = Context::default();
    let mut first = run_pass(&context, vec![], |ui| register_target(ui, BACKGROUND));
    let Some(superseded) = first.pointer_hit_graph_candidate.clone() else {
        panic!("completed pass must emit a hit graph candidate");
    };
    let superseded_headless_clone = superseded.clone();
    let second = run_pass(&context, vec![], |ui| register_target(ui, REPLACEMENT));

    first.append(second);

    assert!(!superseded.settle(&PaintOutcome::Swapped));
    assert!(!superseded_headless_clone.accept_for_headless_host());
    let Some(candidate) = first.pointer_hit_graph_candidate else {
        panic!("combined output must retain its latest candidate");
    };
    assert_eq!(candidate.snapshot().widget_pass_nr(), 1);
    assert_eq!(
        click_receiver_id(candidate.snapshot().probe(target_rect().center())),
        Some(Id::new(REPLACEMENT))
    );
}

#[test]
fn presentation_result_exposes_snapshot_only_after_success() {
    let successful_context = Context::default();
    let successful = run_pass(&successful_context, vec![], |ui| {
        register_target(ui, BACKGROUND);
    });
    let successful_result = crate::PresentationResult::new(
        crate::ViewportId::ROOT,
        UserData::new(1_u64),
        PaintOutcome::Swapped,
        successful.pointer_hit_graph_candidate,
    );
    assert!(successful_result.presented_pointer_hit_graph().is_some());

    let failed_context = Context::default();
    let failed = run_pass(&failed_context, vec![], |ui| {
        register_target(ui, BACKGROUND);
    });
    let failed_result = crate::PresentationResult::new(
        crate::ViewportId::ROOT,
        UserData::new(2_u64),
        PaintOutcome::Failed(PaintFailure::RendererUnavailable),
        failed.pointer_hit_graph_candidate,
    );
    assert!(failed_result.presented_pointer_hit_graph().is_none());
}

#[test]
fn presentation_result_rejects_a_candidate_from_another_viewport() {
    let context = Context::default();
    let output = run_pass(&context, vec![], |ui| register_target(ui, BACKGROUND));
    let Some(candidate) = output.pointer_hit_graph_candidate else {
        panic!("completed pass must emit a hit graph candidate");
    };
    let retained_candidate = candidate.clone();
    let other_viewport = crate::ViewportId::from_hash_of("another viewport");

    let result = crate::PresentationResult::new(
        other_viewport,
        UserData::new(3_u64),
        PaintOutcome::Swapped,
        Some(candidate),
    );

    assert!(result.presented_pointer_hit_graph().is_none());
    assert!(!retained_candidate.settle(&PaintOutcome::Swapped));
}

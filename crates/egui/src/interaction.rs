//! How mouse and touch interzcts with widgets.

use crate::{
    EventCorrelation, Id, InputState, Key, LayerId, PointerButton, Pos2, Rect, Sense, ViewportId,
    WidgetRect, WidgetRects, hit_test, id, input_state, memory,
};

use self::{hit_test::WidgetHits, id::IdSet, input_state::PointerEvent, memory::InteractionState};

/// The kind of raw pointer event represented by a [`PointerReceiverRecord`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PointerEventKind {
    /// The pointer moved.
    Moved,

    /// A pointer button was pressed.
    Pressed(PointerButton),

    /// A pointer button was released.
    Released(PointerButton),
}

/// Identity and backend correlation of one pointer event within an egui viewport pass.
///
/// The correlation is copied from the source [`crate::EventEnvelope`] after application input
/// hooks have been sanitized. Neither it nor `ViewportId` proves a native-window incarnation,
/// pointer capture, or receiver authority. A native integration must bind those facts separately.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PointerEventId {
    /// Viewport whose input contained the event.
    pub viewport_id: ViewportId,

    /// Cumulative pass number in `viewport_id`.
    pub cumulative_pass_nr: u64,

    /// Index of the source event in [`crate::RawInput::events`].
    ///
    /// This is a viewport-pass-local diagnostic identity. Consumers do not need the original
    /// `RawInput` to recover backend correlation; use [`Self::correlation`] directly.
    pub raw_event_index: usize,

    /// Backend derivative correlation copied from the source event envelope.
    ///
    /// This narrow claim does not prove window incarnation, capture, coordinates, or receiver
    /// identity. [`EventCorrelation::Unknown`] is preserved without inference.
    pub correlation: EventCorrelation,
}

/// Stable description of an egui widget that can receive pointer interaction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WidgetReceiver {
    /// Globally unique widget id.
    pub id: Id,

    /// Layer that owned the widget when the receiver fact was produced.
    pub layer_id: LayerId,

    /// Pointer interaction rectangle in the layer's coordinate space.
    pub interact_rect: Rect,

    /// Pointer capabilities registered by the widget.
    pub sense: Sense,

    /// Whether the widget was enabled.
    pub enabled: bool,
}

impl From<WidgetRect> for WidgetReceiver {
    fn from(widget: WidgetRect) -> Self {
        Self {
            id: widget.id,
            layer_id: widget.layer_id,
            interact_rect: widget.interact_rect,
            sense: widget.sense,
            enabled: widget.enabled,
        }
    }
}

/// Why egui cannot authoritatively report a pointer hit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PointerReceiverUnavailableReason {
    /// No prior widget pass exists for the viewport.
    NoCompletedWidgetPass,

    /// The immediately preceding logical pass was not successfully presented.
    PreviousPassNotPresented,

    /// The pointer position was non-finite.
    InvalidPosition,
}

/// Whether a receiver fact is authoritative for this egui pass.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PointerReceiverAuthority<T> {
    /// Egui evaluated the fact against the frozen previous-pass widget graph.
    Known(T),

    /// Egui could not evaluate the fact.
    Unknown(PointerReceiverUnavailableReason),
}

/// Event-time point hit against egui's frozen previous-pass widget graph.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PointerHit {
    /// Cumulative pass number of the frozen widget graph used for this hit test.
    ///
    /// This is always the pass immediately preceding the event's pass. Consumers must bind
    /// receiver identities to this exact generation instead of assuming a stable [`Id`] names
    /// the same rendered control forever.
    pub widget_pass_nr: u64,

    /// Topmost interactable Area or modal layer at the event position.
    pub blocking_layer: Option<LayerId>,

    /// Topmost widget whose interaction rectangle contains the event position.
    pub top_widget: Option<WidgetReceiver>,

    /// Widgets whose interaction rectangles contain the event position.
    ///
    /// Entries are ordered back-to-front so consumers can resolve lanes, such
    /// as scrolling, which intentionally pass through non-owning widgets while
    /// still respecting higher foreign receivers and modal blockers.
    pub containing_receivers: Vec<WidgetReceiver>,

    /// Widget selected by egui's click lane.
    pub click_receiver: Option<WidgetReceiver>,

    /// Widget selected by egui's drag lane.
    pub drag_receiver: Option<WidgetReceiver>,
}

/// A widget route captured by a physical pointer press.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExactCapturedReceiver {
    /// Widget receiver frozen at capture time.
    pub receiver: WidgetReceiver,

    /// Press event that established this route.
    pub started_at: PointerEventId,

    /// Button that established this route.
    pub button: PointerButton,
}

/// Egui's retained route for one click or drag lane.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CapturedReceiver {
    /// Route established by an exact raw pointer event.
    Exact(ExactCapturedReceiver),

    /// Route started programmatically through [`crate::Context::set_dragged_id`].
    Programmatic(Id),
}

impl CapturedReceiver {
    pub(crate) fn id(self) -> Id {
        match self {
            Self::Exact(captured) => captured.receiver.id,
            Self::Programmatic(id) => id,
        }
    }
}

/// The widget candidates retained by egui while a click or drag gesture is in progress.
///
/// This is widget-level routing state, not proof of native window pointer capture.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PointerRoute {
    /// Route that may receive a click on release.
    pub click: Option<CapturedReceiver>,

    /// Route that may become or remain the active drag receiver.
    pub drag: Option<CapturedReceiver>,
}

/// How egui routed one pointer event to widget interaction lanes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PointerDelivery {
    /// Movement without a captured widget route.
    None,

    /// A press established these potential click and drag routes.
    Press(PointerRoute),

    /// A move or release was routed through the routes retained before the event.
    Captured(PointerRoute),
}

/// Event-time receiver record for one raw pointer event.
///
/// Unlike [`InteractionSnapshot`], this does not collapse a batch to the final pointer position.
/// The source envelope's sanitized backend correlation is available as
/// `record.event.correlation`, so consumers do not need to retain or recover the original
/// [`crate::RawInput`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PointerReceiverRecord {
    /// Exact local identity of the source event.
    pub event: PointerEventId,

    /// Event kind and pointer button, when applicable.
    pub kind: PointerEventKind,

    /// Event-time position in the active viewport's logical coordinate space.
    pub position: Pos2,

    /// Point hit at [`Self::position`].
    pub hit: PointerReceiverAuthority<PointerHit>,

    /// Widget route immediately before reducing this event.
    pub route_before: PointerRoute,

    /// Widget delivery performed for this event.
    pub delivery: PointerDelivery,

    /// Widget route immediately after reducing this event.
    pub route_after: PointerRoute,
}

/// Ordered receiver records emitted by one egui viewport update.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PointerReceiverJournal {
    /// Records in their original [`crate::RawInput::events`] order.
    pub records: Vec<PointerReceiverRecord>,
}

impl PointerReceiverJournal {
    pub(crate) fn append(&mut self, mut newer: Self) {
        self.records.append(&mut newer.records);
    }
}

pub(crate) struct PointerEventState {
    pub record: PointerReceiverRecord,
    pub hits: WidgetHits,
}

/// Calculated at the start of each frame
/// based on:
/// * Widget rects from precious frame
/// * Mouse/touch input
/// * Current [`InteractionState`].
#[derive(Clone, Default)]
pub struct InteractionSnapshot {
    /// The widget that got clicked this frame.
    pub clicked: Option<Id>,

    /// This widget was long-pressed on a touch screen,
    /// so trigger a secondary click on it (context menu).
    pub long_touched: Option<Id>,

    /// Drag started on this widget this frame.
    ///
    /// This will also be found in `dragged` this frame.
    pub drag_started: Option<Id>,

    /// This widget is being dragged this frame.
    ///
    /// Set the same frame a drag starts,
    /// but unset the frame a drag ends.
    ///
    /// NOTE: this may not have a corresponding [`crate::WidgetRect`],
    /// if this for instance is a drag-and-drop widget which
    /// isn't painted whilst being dragged
    pub dragged: Option<Id>,

    /// This widget was let go this frame,
    /// after having been dragged.
    ///
    /// The widget will not be found in [`Self::dragged`] this frame.
    pub drag_stopped: Option<Id>,

    /// A small set of widgets (usually 0-1) that the pointer is hovering over.
    ///
    /// Show these widgets as highlighted, if they are interactive.
    ///
    /// While dragging or clicking something, nothing else is hovered.
    ///
    /// Use [`Self::contains_pointer`] to find a drop-zone for drag-and-drop.
    pub hovered: IdSet,

    /// All widgets that contain the pointer this frame,
    /// regardless if the user is currently clicking or dragging.
    ///
    /// This is usually a larger set than [`Self::hovered`],
    /// and can be used for e.g. drag-and-drop zones.
    pub contains_pointer: IdSet,
}

impl InteractionSnapshot {
    pub fn ui(&self, ui: &mut crate::Ui) {
        let Self {
            clicked,
            long_touched,
            drag_started,
            dragged,
            drag_stopped,
            hovered,
            contains_pointer,
        } = self;

        fn id_ui<'a>(ui: &mut crate::Ui, widgets: impl IntoIterator<Item = &'a Id>) {
            for id in widgets {
                ui.label(id.short_debug_format());
            }
        }

        crate::Grid::new("interaction").show(ui, |ui| {
            ui.label("clicked");
            id_ui(ui, clicked);
            ui.end_row();

            ui.label("long_touched");
            id_ui(ui, long_touched);
            ui.end_row();

            ui.label("drag_started");
            id_ui(ui, drag_started);
            ui.end_row();

            ui.label("dragged");
            id_ui(ui, dragged);
            ui.end_row();

            ui.label("drag_stopped");
            id_ui(ui, drag_stopped);
            ui.end_row();

            ui.label("hovered");
            id_ui(ui, hovered);
            ui.end_row();

            ui.label("contains_pointer");
            id_ui(ui, contains_pointer);
            ui.end_row();
        });
    }
}

pub(crate) fn interact(
    prev_snapshot: &InteractionSnapshot,
    widgets: &WidgetRects,
    hits: &WidgetHits,
    pointer_events: &mut [PointerEventState],
    input: &InputState,
    interaction: &mut InteractionState,
) -> InteractionSnapshot {
    profiling::function_scope!();

    if let Some(captured) = interaction.potential_click {
        let is_current = match captured {
            CapturedReceiver::Exact(captured) => widgets
                .get(captured.receiver.id)
                .is_some_and(|widget| widget.layer_id == captured.receiver.layer_id),
            CapturedReceiver::Programmatic(id) => widgets.contains(id),
        };
        if !is_current {
            // The widget we were interested in clicking is gone or was recreated in another layer.
            interaction.potential_click = None;
        }
    }
    if let Some(CapturedReceiver::Exact(captured)) = interaction.potential_drag
        && widgets
            .get(captured.receiver.id)
            .is_some_and(|widget| widget.layer_id != captured.receiver.layer_id)
    {
        // A temporarily absent drag source is valid, but the same id in another layer is not the
        // captured receiver.
        interaction.potential_drag = None;
    }

    let mut clicked = None;
    let mut dragged = prev_snapshot.dragged;
    let mut long_touched = None;

    if input.key_pressed(Key::Escape) {
        // Abort dragging on escape
        dragged = None;
        interaction.potential_drag = None;
    }

    if input.is_long_touch() {
        // We implement "press-and-hold for context menu" on touch screens here
        if let Some(widget) = interaction
            .potential_click
            .and_then(|captured| widgets.get(captured.id()))
        {
            dragged = None;
            clicked = Some(widget.id);
            long_touched = Some(widget.id);
            interaction.potential_click = None;
            interaction.potential_drag = None;
        }
    }

    // Note: in the current code a press-release in the same frame is NOT considered a drag.
    // A missing event-time state must fail closed instead of crashing the host application.
    for (pointer_event, pointer_state) in input
        .pointer
        .pointer_events
        .iter()
        .zip(pointer_events.iter_mut())
    {
        let route_before = PointerRoute {
            click: interaction.potential_click,
            drag: interaction.potential_drag,
        };
        pointer_state.record.route_before = route_before;
        let hits = &pointer_state.hits;
        match pointer_event {
            PointerEvent::Moved { .. } => {
                pointer_state.record.delivery = if route_before == PointerRoute::default() {
                    PointerDelivery::None
                } else {
                    PointerDelivery::Captured(route_before)
                };
            }

            PointerEvent::Pressed { button, .. } => {
                // Maybe new click?
                if interaction.potential_click.is_none() {
                    interaction.potential_click = hits.click.map(|widget| {
                        CapturedReceiver::Exact(ExactCapturedReceiver {
                            receiver: widget.into(),
                            started_at: pointer_state.record.event,
                            button: *button,
                        })
                    });
                }

                // Maybe new drag?
                if interaction.potential_drag.is_none() {
                    interaction.potential_drag = hits.drag.map(|widget| {
                        CapturedReceiver::Exact(ExactCapturedReceiver {
                            receiver: widget.into(),
                            started_at: pointer_state.record.event,
                            button: *button,
                        })
                    });
                }
            }

            PointerEvent::Released { click, .. } => {
                pointer_state.record.delivery = if route_before == PointerRoute::default() {
                    PointerDelivery::None
                } else {
                    PointerDelivery::Captured(route_before)
                };
                if click.is_some()
                    && !input.pointer.is_decidedly_dragging()
                    && let Some(widget) = interaction
                        .potential_click
                        .and_then(|captured| widgets.get(captured.id()))
                {
                    clicked = Some(widget.id);
                }

                interaction.potential_drag = None;
                interaction.potential_click = None;
                dragged = None;
            }
        }
        let route_after = PointerRoute {
            click: interaction.potential_click,
            drag: interaction.potential_drag,
        };
        if matches!(pointer_event, PointerEvent::Pressed { .. }) {
            pointer_state.record.delivery = PointerDelivery::Press(route_after);
        }
        pointer_state.record.route_after = route_after;
    }

    if dragged.is_none() {
        // Check if we started dragging something new:
        if let Some(widget) = interaction
            .potential_drag
            .and_then(|captured| widgets.get(captured.id()))
            && widget.enabled
        {
            let is_dragged = if widget.sense.senses_click() && widget.sense.senses_drag() {
                // This widget is sensitive to both clicks and drags.
                // When the mouse first is pressed, it could be either,
                // so we postpone the decision until we know.
                input.pointer.is_decidedly_dragging()
            } else {
                // This widget is just sensitive to drags, so we can mark it as dragged right away:
                widget.sense.senses_drag()
            };

            if is_dragged {
                dragged = Some(widget.id);
            }
        }
    }

    if !input.pointer.could_any_button_be_click() {
        interaction.potential_click = None;
    }

    if !input.pointer.any_down() {
        interaction.potential_click = None;
        interaction.potential_drag = None;
    }

    // ------------------------------------------------------------------------

    let drag_changed = dragged != prev_snapshot.dragged;
    let drag_stopped = drag_changed.then_some(prev_snapshot.dragged).flatten();
    let drag_started = drag_changed.then_some(dragged).flatten();

    // if let Some(drag_started) = drag_started {
    //     eprintln!(
    //         "Started dragging {} {:?}",
    //         drag_started.id.short_debug_format(),
    //         drag_started.rect
    //     );
    // }

    let contains_pointer: IdSet =
        itertools::chain!(&hits.contains_pointer, &hits.click, &hits.drag)
            .map(|w| w.id)
            .collect();

    let hovered = if clicked.is_some() || dragged.is_some() || long_touched.is_some() {
        // If currently clicking or dragging, only that and nothing else is hovered.
        itertools::chain!(&clicked, &dragged, &long_touched)
            .copied()
            .collect()
    } else {
        // We may be hovering an interactive widget or two.
        // We must also consider the case where non-interactive widgets
        // are _on top_ of an interactive widget.
        // For instance: a label in a draggable window.
        // In that case we want to hover _both_ widgets,
        // otherwise we won't see tooltips for the label.
        //
        // So: we want to hover _all_ widgets above the interactive widget (if any),
        // but none below it (an interactive widget stops the hover search).
        //
        // To know when to stop we need to first know the order of the widgets,
        // which luckily we already have in `hits.close`.

        let order = |id| hits.close.iter().position(|w| w.id == id);

        let click_order = hits.click.and_then(|w| order(w.id)).unwrap_or(0);
        let drag_order = hits.drag.and_then(|w| order(w.id)).unwrap_or(0);
        let top_interactive_order = click_order.max(drag_order);

        let mut hovered: IdSet = std::iter::chain(&hits.click, &hits.drag)
            .map(|w| w.id)
            .collect();

        for w in &hits.contains_pointer {
            let is_interactive = w.sense.senses_click() || w.sense.senses_drag();
            if is_interactive {
                // The only interactive widgets we mark as hovered are the ones
                // in `hits.click` and `hits.drag`!
            } else {
                let is_on_top_of_the_interactive_widget =
                    top_interactive_order <= order(w.id).unwrap_or(0);
                if is_on_top_of_the_interactive_widget {
                    hovered.insert(w.id);
                }
            }
        }

        hovered
    };

    InteractionSnapshot {
        clicked,
        long_touched,
        drag_started,
        dragged,
        drag_stopped,
        hovered,
        contains_pointer,
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        BackendEventDerivation, BackendEventSequence, Context, Event, EventCorrelation,
        EventEnvelope, Id, LayerId, Modifiers, Order, Plugin, PointerButton, RawInput, Rect, Sense,
        Ui, UiBuilder, pos2,
    };

    use super::{
        CapturedReceiver, PointerDelivery, PointerEventKind, PointerHit, PointerReceiverAuthority,
        PointerReceiverJournal, PointerReceiverRecord, PointerRoute,
    };

    const A: &str = "pointer_receiver_a";
    const B: &str = "pointer_receiver_b";
    const OVERLAY: &str = "pointer_receiver_overlay";

    fn a_rect() -> Rect {
        Rect::from_min_max(pos2(10.0, 10.0), pos2(80.0, 80.0))
    }

    fn b_rect() -> Rect {
        Rect::from_min_max(pos2(120.0, 10.0), pos2(190.0, 80.0))
    }

    fn raw(events: Vec<Event>) -> RawInput {
        RawInput {
            screen_rect: Some(Rect::from_min_max(pos2(0.0, 0.0), pos2(240.0, 120.0))),
            events: events.into_iter().map(Into::into).collect(),
            ..Default::default()
        }
    }

    fn button(position: crate::Pos2, pressed: bool) -> Event {
        Event::PointerButton {
            pos: position,
            button: PointerButton::Primary,
            pressed,
            modifiers: Modifiers::NONE,
        }
    }

    fn register_targets(ui: &mut Ui) {
        let _ = ui.interact(a_rect(), Id::new(A), Sense::click_and_drag());
        let _ = ui.interact(b_rect(), Id::new(B), Sense::click_and_drag());
    }

    fn run(
        context: &Context,
        events: Vec<Event>,
        register: impl FnMut(&mut Ui),
    ) -> PointerReceiverJournal {
        run_envelopes(
            context,
            events.into_iter().map(EventEnvelope::unknown).collect(),
            register,
        )
    }

    fn run_envelopes(
        context: &Context,
        events: Vec<EventEnvelope>,
        mut register: impl FnMut(&mut Ui),
    ) -> PointerReceiverJournal {
        let mut journal = PointerReceiverJournal::default();
        let input = RawInput {
            screen_rect: Some(Rect::from_min_max(pos2(0.0, 0.0), pos2(240.0, 120.0))),
            events,
            ..Default::default()
        };
        let output = context.run_ui(input, |ui| {
            register(ui);
            journal = ui.ctx().pointer_receiver_journal(Clone::clone);
        });
        if let Some(candidate) = output.pointer_hit_graph_candidate {
            candidate.settle(&crate::PaintOutcome::Swapped);
        }
        journal
    }

    fn known_hit(record: &PointerReceiverRecord) -> &PointerHit {
        match &record.hit {
            PointerReceiverAuthority::Known(hit) => hit,
            PointerReceiverAuthority::Unknown(reason) => {
                panic!("expected a known hit, got {reason:?}")
            }
        }
    }

    fn exact_click(route: PointerRoute) -> Option<Id> {
        match route.click {
            Some(CapturedReceiver::Exact(captured)) => Some(captured.receiver.id),
            Some(CapturedReceiver::Programmatic(id)) => {
                panic!("expected an exact physical capture, got programmatic {id:?}")
            }
            None => None,
        }
    }

    #[test]
    fn first_pass_pointer_hit_is_unknown_instead_of_known_none() {
        let context = Context::default();
        let journal = run(
            &context,
            vec![button(a_rect().center(), true)],
            register_targets,
        );

        assert_eq!(journal.records.len(), 1);
        assert!(matches!(
            journal.records[0].hit,
            PointerReceiverAuthority::Unknown(
                super::PointerReceiverUnavailableReason::NoCompletedWidgetPass
            )
        ));
        assert_eq!(
            journal.records[0].event.correlation,
            EventCorrelation::Unknown
        );
        assert_eq!(journal.records[0].route_after, PointerRoute::default());
    }

    #[test]
    fn release_then_press_preserves_each_derivative_correlation() {
        let context = Context::default();
        let _ = run(&context, vec![], register_targets);
        let _ = run(
            &context,
            vec![button(a_rect().center(), true)],
            register_targets,
        );

        let sequence = BackendEventSequence::new(73);
        let mut derivation = BackendEventDerivation::known(sequence);
        let journal = run_envelopes(
            &context,
            vec![
                derivation.envelope(button(a_rect().center(), false)),
                derivation.envelope(button(b_rect().center(), true)),
            ],
            register_targets,
        );

        assert_eq!(journal.records.len(), 2);
        assert_eq!(
            journal.records[0].event.correlation,
            EventCorrelation::Known {
                sequence,
                derivative_ordinal: 0,
            }
        );
        assert_eq!(
            journal.records[1].event.correlation,
            EventCorrelation::Known {
                sequence,
                derivative_ordinal: 1,
            }
        );
    }

    struct SwapPointerEventsPlugin;

    impl Plugin for SwapPointerEventsPlugin {
        fn debug_name(&self) -> &'static str {
            "swap pointer events"
        }

        fn input_hook(&mut self, _ctx: &Context, input: &mut RawInput) {
            if input.events.len() == 2 {
                input.events.swap(0, 1);
            }
        }
    }

    #[test]
    fn receiver_journal_uses_hook_sanitized_correlation() {
        let context = Context::default();
        context.add_plugin(SwapPointerEventsPlugin);
        let _ = run(&context, vec![], register_targets);

        let mut derivation = BackendEventDerivation::known(BackendEventSequence::new(89));
        let journal = run_envelopes(
            &context,
            vec![
                derivation.envelope(button(a_rect().center(), true)),
                derivation.envelope(button(b_rect().center(), false)),
            ],
            register_targets,
        );

        assert_eq!(journal.records.len(), 2);
        assert!(
            journal
                .records
                .iter()
                .all(|record| record.event.correlation == EventCorrelation::Unknown)
        );
    }

    #[test]
    fn release_a_then_press_b_preserves_event_hits_and_capture_order() {
        let context = Context::default();
        let _ = run(&context, vec![], register_targets);
        let press_a = run(
            &context,
            vec![button(a_rect().center(), true)],
            register_targets,
        );
        assert_eq!(
            exact_click(press_a.records[0].route_after),
            Some(Id::new(A))
        );

        let journal = run(
            &context,
            vec![
                button(a_rect().center(), false),
                button(b_rect().center(), true),
            ],
            register_targets,
        );

        assert_eq!(journal.records.len(), 2);
        let release = &journal.records[0];
        assert_eq!(release.event.raw_event_index, 0);
        assert_eq!(
            release.kind,
            PointerEventKind::Released(PointerButton::Primary)
        );
        assert_eq!(exact_click(release.route_before), Some(Id::new(A)));
        assert_eq!(release.route_after, PointerRoute::default());
        assert_eq!(
            known_hit(release)
                .click_receiver
                .map(|receiver| receiver.id),
            Some(Id::new(A))
        );

        let press = &journal.records[1];
        assert_eq!(press.event.raw_event_index, 1);
        assert_eq!(
            press.kind,
            PointerEventKind::Pressed(PointerButton::Primary)
        );
        assert_eq!(press.route_before, PointerRoute::default());
        assert_eq!(exact_click(press.route_after), Some(Id::new(B)));
        assert_eq!(
            known_hit(press).click_receiver.map(|receiver| receiver.id),
            Some(Id::new(B))
        );
    }

    #[test]
    fn three_edges_in_one_batch_use_their_own_positions() {
        let context = Context::default();
        let _ = run(&context, vec![], register_targets);
        let journal = run(
            &context,
            vec![
                button(a_rect().center(), true),
                button(a_rect().center(), false),
                button(b_rect().center(), true),
            ],
            register_targets,
        );

        let receivers = journal
            .records
            .iter()
            .map(|record| known_hit(record).click_receiver.map(|receiver| receiver.id))
            .collect::<Vec<_>>();
        assert_eq!(
            receivers,
            vec![Some(Id::new(A)), Some(Id::new(A)), Some(Id::new(B))]
        );
        assert_eq!(
            exact_click(journal.records[2].route_after),
            Some(Id::new(B))
        );
    }

    #[test]
    fn foreground_widget_is_the_receiver_instead_of_background_dock_target() {
        let context = Context::default();
        let register = |ui: &mut Ui| {
            let _ = ui.interact(a_rect(), Id::new(A), Sense::click_and_drag());
            let layer = LayerId::new(Order::Foreground, Id::new("receiver_overlay_layer"));
            let _ = ui.scope_builder(UiBuilder::new().layer_id(layer), |ui| {
                let _ = ui.interact(a_rect(), Id::new(OVERLAY), Sense::click_and_drag());
            });
        };
        let _ = run(&context, vec![], register);
        let journal = run(&context, vec![button(a_rect().center(), true)], register);

        let hit = known_hit(&journal.records[0]);
        assert_eq!(
            hit.top_widget.map(|receiver| receiver.id),
            Some(Id::new(OVERLAY))
        );
        assert_eq!(
            hit.click_receiver.map(|receiver| receiver.id),
            Some(Id::new(OVERLAY))
        );
        assert_eq!(
            exact_click(journal.records[0].route_after),
            Some(Id::new(OVERLAY))
        );
    }

    #[test]
    fn captured_a_and_point_hit_b_are_reported_at_the_same_time() {
        let context = Context::default();
        let _ = run(&context, vec![], register_targets);
        let _ = run(
            &context,
            vec![button(a_rect().center(), true)],
            register_targets,
        );
        let journal = run(
            &context,
            vec![Event::PointerMoved(b_rect().center())],
            register_targets,
        );

        let movement = &journal.records[0];
        assert_eq!(exact_click(movement.route_before), Some(Id::new(A)));
        assert!(matches!(
            movement.delivery,
            PointerDelivery::Captured(route) if exact_click(route) == Some(Id::new(A))
        ));
        assert_eq!(
            known_hit(movement)
                .click_receiver
                .map(|receiver| receiver.id),
            Some(Id::new(B))
        );
        assert_eq!(exact_click(movement.route_after), Some(Id::new(A)));
    }

    #[test]
    fn pointer_gone_does_not_fabricate_release_or_clear_capture() {
        let context = Context::default();
        let _ = run(&context, vec![], register_targets);
        let _ = run(
            &context,
            vec![button(a_rect().center(), true)],
            register_targets,
        );
        let gone = run(&context, vec![Event::PointerGone], register_targets);
        assert!(gone.records.is_empty());

        let movement = run(
            &context,
            vec![Event::PointerMoved(b_rect().center())],
            register_targets,
        );
        assert_eq!(
            exact_click(movement.records[0].route_before),
            Some(Id::new(A))
        );
    }

    #[test]
    fn same_widget_id_in_another_layer_does_not_rebind_capture() {
        let context = Context::default();
        let register_background = |ui: &mut Ui| {
            let _ = ui.interact(a_rect(), Id::new(A), Sense::click_and_drag());
        };
        let register_foreground = |ui: &mut Ui| {
            let layer = LayerId::new(Order::Foreground, Id::new("replacement_layer"));
            let _ = ui.scope_builder(UiBuilder::new().layer_id(layer), |ui| {
                let _ = ui.interact(a_rect(), Id::new(A), Sense::click_and_drag());
            });
        };
        let _ = run(&context, vec![], register_background);
        let press = run(
            &context,
            vec![button(a_rect().center(), true)],
            register_background,
        );
        assert_eq!(exact_click(press.records[0].route_after), Some(Id::new(A)));

        let _ = run(&context, vec![], register_foreground);
        let movement = run(
            &context,
            vec![Event::PointerMoved(a_rect().center())],
            register_foreground,
        );
        assert_eq!(movement.records[0].route_before, PointerRoute::default());
        assert_eq!(
            known_hit(&movement.records[0])
                .click_receiver
                .map(|receiver| receiver.layer_id.order),
            Some(Order::Foreground)
        );
    }

    #[test]
    fn multipass_output_retains_each_pointer_record_exactly_once() {
        let context = Context::default();
        context.options_mut(|options| options.max_passes = 2.try_into().unwrap());
        let _ = run(&context, vec![], register_targets);
        let mut pass = 0;
        let output = context.run_ui(raw(vec![button(a_rect().center(), true)]), |ui| {
            register_targets(ui);
            if pass == 0 {
                ui.request_discard("pointer receiver journal multipass test");
            }
            pass += 1;
        });

        assert_eq!(pass, 2);
        assert_eq!(output.pointer_receiver_journal.records.len(), 1);
        assert_eq!(
            output.pointer_receiver_journal.records[0]
                .event
                .raw_event_index,
            0
        );
    }
}

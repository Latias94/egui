use crate::{OrderedViewportIdMap, Theme, ViewportId, ViewportIdMap, emath::Rect};

use super::{
    DroppedFile, Event, EventEnvelope, EventEnvelopeClaim, HoveredFile, Modifiers, SafeAreaInsets,
    ViewportInfo,
};

/// Snapshot used by framework integrations to preserve event identity across user hooks.
///
/// The contents are intentionally private. Capture it immediately before calling a user input hook
/// and pass it to [`RawInput::sanitize_hook_events`] afterwards.
#[doc(hidden)]
#[derive(Clone, Debug)]
pub struct EventProvenanceSnapshot {
    events: Vec<EventEnvelope>,
}

/// What the integrations provides to egui at the start of each frame.
///
/// Set the values that make sense, leave the rest at their `Default::default()`.
///
/// You can check if `egui` is using the inputs using
/// [`crate::Context::egui_wants_pointer_input`] and [`crate::Context::egui_wants_keyboard_input`].
///
/// All coordinates are in points (logical pixels) with origin (0, 0) in the top left .corner.
///
/// Ii "points" can be calculated from native physical pixels
/// using `pixels_per_point` = [`crate::Context::zoom_factor`] * `native_pixels_per_point`;
#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
pub struct RawInput {
    /// The id of the active viewport.
    pub viewport_id: ViewportId,

    /// Information about all egui viewports.
    pub viewports: ViewportIdMap<ViewportInfo>,

    /// The insets used to only render content in a mobile safe area
    ///
    /// `None` will be treated as "same as last frame"
    pub safe_area_insets: Option<SafeAreaInsets>,

    /// Position and size of the area that egui should use, in points.
    /// Usually you would set this to
    ///
    /// `Some(Rect::from_min_size(Default::default(), screen_size_in_points))`.
    ///
    /// but you could also constrain egui to some smaller portion of your window if you like.
    ///
    /// `None` will be treated as "same as last frame", with the default being a very big area.
    pub screen_rect: Option<Rect>,

    /// Maximum size of one side of the font texture.
    ///
    /// Ask your graphics drivers about this. This corresponds to `GL_MAX_TEXTURE_SIZE`.
    ///
    /// The default is a very small (but very portable) 2048.
    pub max_texture_side: Option<usize>,

    /// Monotonically increasing time, in seconds. Relative to whatever. Used for animations.
    /// If `None` is provided, egui will assume a time delta of `predicted_dt` (default 1/60 seconds).
    pub time: Option<f64>,

    /// Should be set to the expected time between frames when painting at vsync speeds.
    /// The default for this is 1/60.
    /// Can safely be left at its default value.
    pub predicted_dt: f32,

    /// Which modifier keys are down at the start of the frame?
    pub modifiers: Modifiers,

    /// In-order events received this frame.
    ///
    /// There is currently no way to know if egui handles a particular event,
    /// but you can check if egui is using the keyboard with [`crate::Context::egui_wants_keyboard_input`]
    /// and/or the pointer (mouse/touch) with [`crate::Context::egui_is_using_pointer`].
    pub events: Vec<EventEnvelope>,

    /// Dragged files hovering over egui.
    pub hovered_files: Vec<HoveredFile>,

    /// Dragged files dropped into egui.
    ///
    /// Note: when using `eframe` on Windows, this will always be empty if drag-and-drop support has
    /// been disabled in [`crate::viewport::ViewportBuilder`].
    pub dropped_files: Vec<DroppedFile>,

    /// The native window has the keyboard focus (i.e. is receiving key presses).
    ///
    /// False when the user alt-tab away from the application, for instance.
    pub focused: bool,

    /// Does the OS use dark or light mode?
    ///
    /// `None` means "don't know".
    pub system_theme: Option<Theme>,
}

impl Default for RawInput {
    fn default() -> Self {
        Self {
            viewport_id: ViewportId::ROOT,
            viewports: std::iter::once((ViewportId::ROOT, Default::default())).collect(),
            screen_rect: None,
            max_texture_side: None,
            time: None,
            predicted_dt: 1.0 / 60.0,
            modifiers: Modifiers::default(),
            events: vec![],
            hovered_files: Default::default(),
            dropped_files: Default::default(),
            focused: true, // integrations opt into global focus tracking
            system_theme: None,
            safe_area_insets: Default::default(),
        }
    }
}

impl RawInput {
    /// Push an application-generated event with explicitly unknown backend provenance.
    #[inline]
    pub fn push_event(&mut self, event: Event) {
        self.events.push(EventEnvelope::unknown(event));
    }

    /// Verify that an affine claim names the exact immutable envelope retained here.
    ///
    /// Native integrations use this before binding the envelope's public correlation claim to
    /// their private provider journal. A matching index and sequence without the process-local
    /// envelope identity is insufficient.
    #[doc(hidden)]
    pub fn contains_event_envelope_claim(&self, claim: &EventEnvelopeClaim) -> bool {
        self.events
            .get(claim.raw_event_index())
            .is_some_and(|envelope| claim.matches_envelope(envelope))
    }

    /// Capture every event identity that exists before invoking an application input hook.
    ///
    /// The identities are process-local capabilities. This normalizes missing or duplicated
    /// identities before exposing the input to the hook, so every pre-hook envelope can authorize
    /// at most one position in the post-hook sequence.
    #[doc(hidden)]
    pub fn event_provenance_snapshot(&mut self) -> EventProvenanceSnapshot {
        let mut identities =
            ahash::HashSet::with_capacity_and_hasher(self.events.len(), Default::default());
        for event in &mut self.events {
            let identity_is_unique = event
                .hook_identity_ptr()
                .is_some_and(|identity| identities.insert(identity));
            if !identity_is_unique {
                let identity = event.reset_hook_identity();
                let inserted = identities.insert(identity);
                debug_assert!(inserted, "a fresh hook identity must be unique");
            }
        }

        EventProvenanceSnapshot {
            events: self.events.clone(),
        }
    }

    /// Downgrade backend correlation that was added, duplicated, or reordered by an input hook.
    ///
    /// Only the longest post-hook prefix that is exactly equal to the pre-hook prefix may retain
    /// `Known` correlation. Original `Unknown` envelopes therefore remain distinguishable from
    /// injected `Unknown` envelopes. The first insertion, deletion, duplicate, replacement, or
    /// reorder starts a causal-pollution suffix in which every `Known` envelope is downgraded.
    /// Events before that point remain valid, so appending an event cannot retroactively invalidate
    /// input already ordered before it. Removing a tail has no effect because no later event can
    /// cross the removed input.
    #[doc(hidden)]
    pub fn sanitize_hook_events(&mut self, before: &EventProvenanceSnapshot) {
        let unchanged_prefix_len = self
            .events
            .iter()
            .zip(&before.events)
            .take_while(|(event, original)| {
                event.has_same_hook_identity(original) && event == original
            })
            .count();

        for event in &mut self.events[unchanged_prefix_len..] {
            if event.correlation().is_known() {
                event.forget_correlation();
            }
        }
    }

    /// Info about the active viewport
    #[inline]
    pub fn viewport(&self) -> &ViewportInfo {
        self.viewports.get(&self.viewport_id).expect("Failed to find current viewport in egui RawInput. This is the fault of the egui backend")
    }

    /// Helper: move volatile (deltas and events), clone the rest.
    ///
    /// * [`Self::hovered_files`] is cloned.
    /// * [`Self::dropped_files`] is moved.
    pub fn take(&mut self) -> Self {
        Self {
            viewport_id: self.viewport_id,
            viewports: self
                .viewports
                .iter_mut()
                .map(|(id, info)| (*id, info.take()))
                .collect(),
            screen_rect: self.screen_rect.take(),
            safe_area_insets: self.safe_area_insets.take(),
            max_texture_side: self.max_texture_side.take(),
            time: self.time,
            predicted_dt: self.predicted_dt,
            modifiers: self.modifiers,
            events: std::mem::take(&mut self.events),
            hovered_files: self.hovered_files.clone(),
            dropped_files: std::mem::take(&mut self.dropped_files),
            focused: self.focused,
            system_theme: self.system_theme,
        }
    }

    /// Add on new input.
    pub fn append(&mut self, newer: Self) {
        let Self {
            viewport_id: viewport_ids,
            viewports,
            screen_rect,
            max_texture_side,
            time,
            predicted_dt,
            modifiers,
            mut events,
            mut hovered_files,
            mut dropped_files,
            focused,
            system_theme,
            safe_area_insets: safe_area,
        } = newer;

        self.viewport_id = viewport_ids;
        self.viewports = viewports;
        self.screen_rect = screen_rect.or(self.screen_rect);
        self.max_texture_side = max_texture_side.or(self.max_texture_side);
        self.time = time; // use latest time
        self.predicted_dt = predicted_dt; // use latest dt
        self.modifiers = modifiers; // use latest
        self.events.append(&mut events);
        self.hovered_files.append(&mut hovered_files);
        self.dropped_files.append(&mut dropped_files);
        self.focused = focused;
        self.system_theme = system_theme;
        self.safe_area_insets = safe_area;
    }

    pub fn ui(&self, ui: &mut crate::Ui) {
        let Self {
            viewport_id,
            viewports,
            screen_rect,
            max_texture_side,
            time,
            predicted_dt,
            modifiers,
            events,
            hovered_files,
            dropped_files,
            focused,
            system_theme,
            safe_area_insets: safe_area,
        } = self;

        ui.label(format!("Active viewport: {viewport_id:?}"));
        let ordered_viewports = viewports
            .iter()
            .map(|(id, value)| (*id, value))
            .collect::<OrderedViewportIdMap<_>>();
        for (id, viewport) in ordered_viewports {
            ui.group(|ui| {
                ui.label(format!("Viewport {id:?}"));
                ui.push_id(id, |ui| {
                    viewport.ui(ui);
                });
            });
        }
        ui.label(format!("screen_rect: {screen_rect:?} points"));

        ui.label(format!("max_texture_side: {max_texture_side:?}"));
        if let Some(time) = time {
            ui.label(format!("time: {time:.3} s"));
        } else {
            ui.label("time: None");
        }
        ui.label(format!("predicted_dt: {:.1} ms", 1e3 * predicted_dt));
        ui.label(format!("modifiers: {modifiers:#?}"));
        ui.label(format!("hovered_files: {}", hovered_files.len()));
        ui.label(format!("dropped_files: {}", dropped_files.len()));
        ui.label(format!("focused: {focused}"));
        ui.label(format!("system_theme: {system_theme:?}"));
        ui.label(format!("safe_area: {safe_area:?}"));
        ui.scope(|ui| {
            ui.set_min_height(150.0);
            ui.label(format!("events: {events:#?}"))
                .on_hover_text("key presses etc");
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{BackendEventDerivation, BackendEventSequence, EventCorrelation};

    #[test]
    fn hook_filtering_known_event_pollutes_following_known_events() {
        let mut derivation = BackendEventDerivation::known(BackendEventSequence::new(7));
        let mut input = RawInput {
            events: vec![
                derivation.envelope(Event::Copy),
                derivation.envelope(Event::Cut),
                derivation.envelope(Event::Paste("retained".to_owned())),
            ],
            ..Default::default()
        };
        let before = input.event_provenance_snapshot();

        input.events.remove(1);
        input.sanitize_hook_events(&before);

        assert!(input.events[0].correlation().is_known());
        assert_eq!(input.events[1].correlation(), EventCorrelation::Unknown);
        assert_eq!(input.events[0].event(), &Event::Copy);
        assert_eq!(
            input.events[1].event(),
            &Event::Paste("retained".to_owned())
        );
    }

    #[test]
    fn hook_snapshot_assigns_each_existing_envelope_a_unique_identity() {
        let event = EventEnvelope::unknown(Event::Copy);
        let mut input = RawInput {
            events: vec![event.clone(), event],
            ..Default::default()
        };

        let _before = input.event_provenance_snapshot();

        assert!(!input.events[0].has_same_hook_identity(&input.events[1]));
    }

    #[test]
    fn hook_snapshot_preserves_order_while_rekeying_duplicates() {
        let first = EventEnvelope::unknown(Event::Copy);
        let mut input = RawInput {
            events: vec![first.clone(), EventEnvelope::unknown(Event::Cut), first],
            ..Default::default()
        };

        let _before = input.event_provenance_snapshot();

        assert_eq!(input.events[0].event(), &Event::Copy);
        assert_eq!(input.events[1].event(), &Event::Cut);
        assert_eq!(input.events[2].event(), &Event::Copy);
        assert!(!input.events[0].has_same_hook_identity(&input.events[2]));
    }

    #[test]
    fn hook_snapshot_normalizes_a_large_unique_batch_without_reordering() {
        const EVENT_COUNT: usize = 8_192;
        let mut input = RawInput {
            events: (0..EVENT_COUNT)
                .map(|index| {
                    EventEnvelope::unknown(Event::PointerMoved(crate::pos2(index as f32, 1.0)))
                })
                .collect(),
            ..Default::default()
        };

        let _before = input.event_provenance_snapshot();

        assert_eq!(input.events.len(), EVENT_COUNT);
        for (index, event) in input.events.iter().enumerate() {
            assert_eq!(
                event.event(),
                &Event::PointerMoved(crate::pos2(index as f32, 1.0))
            );
        }
        assert!(
            input
                .events
                .windows(2)
                .all(|events| !events[0].has_same_hook_identity(&events[1]))
        );
    }

    #[test]
    fn hook_cannot_inject_or_duplicate_known_provenance() {
        let mut derivation = BackendEventDerivation::known(BackendEventSequence::new(11));
        let mut input = RawInput {
            events: vec![derivation.envelope(Event::Copy)],
            ..Default::default()
        };
        let before = input.event_provenance_snapshot();

        input.events.push(input.events[0].clone());
        let mut forged = BackendEventDerivation::known(BackendEventSequence::new(12));
        input.events.push(forged.envelope(Event::Cut));
        input.sanitize_hook_events(&before);

        assert!(input.events[0].correlation().is_known());
        assert_eq!(input.events[1].correlation(), EventCorrelation::Unknown);
        assert_eq!(input.events[2].correlation(), EventCorrelation::Unknown);
    }

    #[test]
    fn hook_filtering_first_known_event_downgrades_the_next_known_event() {
        let mut derivation = BackendEventDerivation::known(BackendEventSequence::new(16));
        let mut input = RawInput {
            events: vec![
                derivation.envelope(Event::Copy),
                derivation.envelope(Event::Paste("after deletion".to_owned())),
            ],
            ..Default::default()
        };
        let before = input.event_provenance_snapshot();

        input.events.remove(0);
        input.sanitize_hook_events(&before);

        assert_eq!(input.events[0].correlation(), EventCorrelation::Unknown);
    }

    #[test]
    fn hook_injected_unknown_between_known_events_pollutes_the_known_suffix() {
        let mut derivation = BackendEventDerivation::known(BackendEventSequence::new(12));
        let mut input = RawInput {
            events: vec![
                derivation.envelope(Event::Copy),
                derivation.envelope(Event::Paste("after injection".to_owned())),
            ],
            ..Default::default()
        };
        let before = input.event_provenance_snapshot();

        input.events.insert(
            1,
            EventEnvelope::unknown(Event::PointerMoved(crate::pos2(20.0, 30.0))),
        );
        input.sanitize_hook_events(&before);

        assert!(input.events[0].correlation().is_known());
        assert_eq!(input.events[1].correlation(), EventCorrelation::Unknown);
        assert_eq!(input.events[2].correlation(), EventCorrelation::Unknown);
    }

    #[test]
    fn hook_duplicate_between_known_events_pollutes_the_known_suffix() {
        let mut derivation = BackendEventDerivation::known(BackendEventSequence::new(13));
        let mut input = RawInput {
            events: vec![
                derivation.envelope(Event::Copy),
                derivation.envelope(Event::Paste("after duplicate".to_owned())),
            ],
            ..Default::default()
        };
        let before = input.event_provenance_snapshot();

        input.events.insert(1, input.events[0].clone());
        input.sanitize_hook_events(&before);

        assert!(input.events[0].correlation().is_known());
        assert_eq!(input.events[1].correlation(), EventCorrelation::Unknown);
        assert_eq!(input.events[2].correlation(), EventCorrelation::Unknown);
    }

    #[test]
    fn original_unknown_event_does_not_pollute_following_known_events() {
        let mut derivation = BackendEventDerivation::known(BackendEventSequence::new(14));
        let mut input = RawInput {
            events: vec![
                derivation.envelope(Event::Copy),
                EventEnvelope::unknown(Event::PointerMoved(crate::pos2(40.0, 50.0))),
                derivation.envelope(Event::Paste("after original unknown".to_owned())),
            ],
            ..Default::default()
        };
        let before = input.event_provenance_snapshot();

        input.sanitize_hook_events(&before);

        assert!(input.events[0].correlation().is_known());
        assert_eq!(input.events[1].correlation(), EventCorrelation::Unknown);
        assert!(input.events[2].correlation().is_known());
    }

    #[test]
    fn hook_filtering_original_unknown_downgrades_the_next_known_event() {
        let mut derivation = BackendEventDerivation::known(BackendEventSequence::new(15));
        let mut input = RawInput {
            events: vec![
                derivation.envelope(Event::Copy),
                EventEnvelope::unknown(Event::PointerMoved(crate::pos2(45.0, 55.0))),
                derivation.envelope(Event::Paste("after unknown deletion".to_owned())),
            ],
            ..Default::default()
        };
        let before = input.event_provenance_snapshot();

        input.events.remove(1);
        input.sanitize_hook_events(&before);

        assert!(input.events[0].correlation().is_known());
        assert_eq!(input.events[1].correlation(), EventCorrelation::Unknown);
    }

    #[test]
    fn hook_appended_unknown_does_not_retroactively_pollute_known_events() {
        let mut derivation = BackendEventDerivation::known(BackendEventSequence::new(15));
        let mut input = RawInput {
            events: vec![
                derivation.envelope(Event::Copy),
                derivation.envelope(Event::Paste("before append".to_owned())),
            ],
            ..Default::default()
        };
        let before = input.event_provenance_snapshot();

        input
            .events
            .push(EventEnvelope::unknown(Event::PointerMoved(crate::pos2(
                60.0, 70.0,
            ))));
        input.sanitize_hook_events(&before);

        assert!(input.events[0].correlation().is_known());
        assert!(input.events[1].correlation().is_known());
        assert_eq!(input.events[2].correlation(), EventCorrelation::Unknown);
    }

    #[test]
    fn hook_swapping_release_then_press_downgrades_the_known_batch() {
        let mut derivation = BackendEventDerivation::known(BackendEventSequence::new(17));
        let mut input = RawInput {
            events: vec![
                derivation.envelope(Event::PointerButton {
                    pos: crate::pos2(10.0, 20.0),
                    button: crate::PointerButton::Primary,
                    pressed: false,
                    modifiers: crate::Modifiers::NONE,
                }),
                derivation.envelope(Event::PointerButton {
                    pos: crate::pos2(30.0, 40.0),
                    button: crate::PointerButton::Secondary,
                    pressed: true,
                    modifiers: crate::Modifiers::NONE,
                }),
            ],
            ..Default::default()
        };
        let before = input.event_provenance_snapshot();

        input.events.swap(0, 1);
        input.sanitize_hook_events(&before);

        assert_eq!(input.events[0].correlation(), EventCorrelation::Unknown);
        assert_eq!(input.events[1].correlation(), EventCorrelation::Unknown);
    }

    #[cfg(feature = "persistence")]
    #[test]
    fn serialized_events_do_not_restore_live_backend_provenance() {
        let mut derivation = BackendEventDerivation::known(BackendEventSequence::new(13));
        let input = RawInput {
            events: vec![derivation.envelope(Event::Copy)],
            ..Default::default()
        };

        let encoded = ron::to_string(&input).unwrap();
        let restored: RawInput = ron::from_str(&encoded).unwrap();

        assert_eq!(restored.events[0].correlation(), EventCorrelation::Unknown);
        assert!(restored.events[0].hook_identity_ptr().is_none());
    }
}

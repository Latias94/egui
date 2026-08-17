use ahash::HashMap;

use emath::TSTransform;

use crate::{LayerId, Pos2, Rect, Sense, Vec2, Vec2b, WidgetRect, WidgetRects, emath, id::IdSet};

/// Opaque identity of one widget selected by a completed-pass hit test.
///
/// The layer is part of the identity so integrations cannot accidentally bind
/// a result to an identically named widget in another layer.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct WidgetHitIdentity {
    id: crate::Id,
    layer_id: LayerId,
}

impl WidgetHitIdentity {
    /// Returns the widget id.
    pub const fn id(self) -> crate::Id {
        self.id
    }

    /// Returns the layer containing the widget.
    pub const fn layer_id(self) -> LayerId {
        self.layer_id
    }

    fn from_widget(widget: WidgetRect) -> Self {
        Self {
            id: widget.id,
            layer_id: widget.layer_id,
        }
    }

    pub(crate) const fn new(id: crate::Id, layer_id: LayerId) -> Self {
        Self { id, layer_id }
    }
}

/// Opaque top-widget result from one completed-pass hit test.
///
/// This records widget identities only. It does not expose the retained widget
/// graph, layer transforms, or interaction state, and it does not imply that a
/// pointer button or capture transition occurred.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WidgetHitSnapshot {
    cumulative_pass_nr: u64,
    click: Option<WidgetHitIdentity>,
    drag: Option<WidgetHitIdentity>,
    contains_pointer: Option<WidgetHitIdentity>,
}

impl WidgetHitSnapshot {
    /// Returns the completed-pass generation used for this hit test.
    pub const fn cumulative_pass_nr(self) -> u64 {
        self.cumulative_pass_nr
    }

    /// Returns the widget which would receive a click at the queried point.
    pub const fn click(self) -> Option<WidgetHitIdentity> {
        self.click
    }

    /// Returns the widget which would receive a drag at the queried point.
    pub const fn drag(self) -> Option<WidgetHitIdentity> {
        self.drag
    }

    /// Returns the frontmost widget whose interaction rectangle contains the point.
    pub const fn contains_pointer(self) -> Option<WidgetHitIdentity> {
        self.contains_pointer
    }

    pub(crate) fn from_hits(cumulative_pass_nr: u64, hits: &WidgetHits) -> Self {
        Self {
            cumulative_pass_nr,
            click: hits.click.map(WidgetHitIdentity::from_widget),
            drag: hits.drag.map(WidgetHitIdentity::from_widget),
            contains_pointer: hits
                .contains_pointer
                .last()
                .copied()
                .map(WidgetHitIdentity::from_widget),
        }
    }
}

/// A finite scroll direction projected through the platform modifiers that
/// apply to one native scroll edge.
///
/// The values are used for direction and axis admission only. Their units do
/// not need to match egui points.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct WidgetScrollDelta {
    x: f64,
    y: f64,
}

impl WidgetScrollDelta {
    /// Constructs a projected scroll direction, returning `None` for non-finite input.
    #[must_use]
    pub fn new(x: f64, y: f64) -> Option<Self> {
        if x.is_finite() && y.is_finite() {
            Some(Self { x, y })
        } else {
            None
        }
    }

    /// Returns horizontal content movement.
    #[must_use]
    pub const fn x(self) -> f64 {
        self.x
    }

    /// Returns vertical content movement.
    #[must_use]
    pub const fn y(self) -> f64 {
        self.y
    }
}

/// Scroll receiver requirement for one completed-pass hit test.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum WidgetScrollHitChallenge {
    /// Resolve the frontmost receiver at the queried point.
    Spatial {
        /// Modifier-projected direction, when the edge carries a directional sample.
        projected_delta: Option<WidgetScrollDelta>,
    },

    /// Prove that the candidate frozen at sequence start still owns delivery.
    Locked {
        /// Exact candidate identity frozen for the sequence.
        receiver: WidgetHitIdentity,
        /// Modifier-projected direction, when the edge carries a directional sample.
        projected_delta: Option<WidgetScrollDelta>,
    },
}

impl WidgetScrollHitChallenge {
    /// Returns the projected direction used for receiver admission.
    #[must_use]
    pub const fn projected_delta(self) -> Option<WidgetScrollDelta> {
        match self {
            Self::Spatial { projected_delta }
            | Self::Locked {
                projected_delta, ..
            } => projected_delta,
        }
    }

    pub(crate) fn classify_candidate(
        self,
        candidate: WidgetHitIdentity,
        presented_candidates: &[WidgetHitIdentity],
    ) -> WidgetScrollHit {
        let is_presented = presented_candidates.contains(&candidate);
        match self {
            Self::Spatial { .. } => {
                if is_presented {
                    WidgetScrollHit::Candidate(candidate)
                } else {
                    WidgetScrollHit::Blocked
                }
            }
            Self::Locked { receiver, .. } => {
                if candidate == receiver {
                    if is_presented {
                        WidgetScrollHit::Candidate(candidate)
                    } else {
                        WidgetScrollHit::Blocked
                    }
                } else if is_presented {
                    WidgetScrollHit::NoReceiver
                } else {
                    WidgetScrollHit::Blocked
                }
            }
        }
    }
}

/// Authoritative scroll receiver result from one completed pass.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WidgetScrollHit {
    /// The frontmost registered product candidate accepted the edge.
    Candidate(WidgetHitIdentity),
    /// A framework receiver or an unrecognized candidate blocked product delivery.
    Blocked,
    /// No receiver accepted the edge.
    NoReceiver,
}

/// Result of a scroll hit test against one viewport's final completed pass.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WidgetScrollHitSnapshot {
    cumulative_pass_nr: u64,
    hit: WidgetScrollHit,
}

impl WidgetScrollHitSnapshot {
    /// Returns the completed-pass generation used for this hit test.
    #[must_use]
    pub const fn cumulative_pass_nr(self) -> u64 {
        self.cumulative_pass_nr
    }

    /// Returns the authoritative receiver result.
    #[must_use]
    pub const fn hit(self) -> WidgetScrollHit {
        self.hit
    }

    pub(crate) const fn new(cumulative_pass_nr: u64, hit: WidgetScrollHit) -> Self {
        Self {
            cumulative_pass_nr,
            hit,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct WidgetScrollHitRegistration(usize);

#[derive(Clone, Copy, Debug)]
pub(crate) enum WidgetScrollHitRoute {
    Candidate(WidgetHitIdentity),
    Framework,
}

#[derive(Clone, Copy, Debug)]
enum WidgetScrollHitRecordKind {
    Candidate(WidgetHitIdentity),
    ScrollArea(ScrollAreaScrollHitState),
    PendingScrollArea,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct WidgetScrollHitRecord {
    layer_id: LayerId,
    rect: Rect,
    enabled: bool,
    kind: WidgetScrollHitRecordKind,
}

impl WidgetScrollHitRecord {
    pub(crate) const fn layer_id(self) -> LayerId {
        self.layer_id
    }

    pub(crate) fn contains_global_position(
        self,
        position: Pos2,
        layer_to_global: Option<emath::TSTransform>,
    ) -> bool {
        let rect = layer_to_global.map_or(self.rect, |to_global| to_global * self.rect);
        rect.is_positive() && rect.is_finite() && rect.contains(position)
    }

    pub(crate) fn route(
        self,
        projected_delta: Option<WidgetScrollDelta>,
    ) -> Option<WidgetScrollHitRoute> {
        if !self.enabled {
            return None;
        }

        match self.kind {
            WidgetScrollHitRecordKind::Candidate(candidate) => {
                Some(WidgetScrollHitRoute::Candidate(candidate))
            }
            WidgetScrollHitRecordKind::ScrollArea(state) => state
                .accepts_projected_delta(projected_delta)
                .then_some(WidgetScrollHitRoute::Framework),
            WidgetScrollHitRecordKind::PendingScrollArea => None,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub(crate) struct WidgetScrollHitRecords {
    records: Vec<WidgetScrollHitRecord>,
}

impl WidgetScrollHitRecords {
    pub(crate) fn register_candidate(
        &mut self,
        rect: Rect,
        candidate: WidgetHitIdentity,
        enabled: bool,
    ) {
        self.records.push(WidgetScrollHitRecord {
            layer_id: candidate.layer_id(),
            rect,
            enabled,
            kind: WidgetScrollHitRecordKind::Candidate(candidate),
        });
    }

    pub(crate) fn reserve_scroll_area(&mut self, layer_id: LayerId) -> WidgetScrollHitRegistration {
        let registration = WidgetScrollHitRegistration(self.records.len());
        self.records.push(WidgetScrollHitRecord {
            layer_id,
            rect: Rect::NOTHING,
            enabled: false,
            kind: WidgetScrollHitRecordKind::PendingScrollArea,
        });
        registration
    }

    pub(crate) fn finish_scroll_area(
        &mut self,
        registration: WidgetScrollHitRegistration,
        rect: Rect,
        enabled: bool,
        state: ScrollAreaScrollHitState,
    ) {
        debug_assert!(
            registration.0 < self.records.len(),
            "scroll-area hit registration must belong to this pass"
        );
        let Some(record) = self.records.get_mut(registration.0) else {
            return;
        };
        debug_assert!(
            matches!(record.kind, WidgetScrollHitRecordKind::PendingScrollArea),
            "scroll-area hit registration must be completed exactly once"
        );
        record.rect = rect;
        record.enabled = enabled;
        record.kind = WidgetScrollHitRecordKind::ScrollArea(state);
    }

    pub(crate) fn front_to_back(&self) -> impl Iterator<Item = WidgetScrollHitRecord> + '_ {
        self.records.iter().rev().copied()
    }

    pub(crate) fn clear(&mut self) {
        self.records.clear();
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct ScrollAreaScrollHitState {
    direction_enabled: Vec2b,
    offset: Vec2,
    max_offset: Vec2,
    wheel_scroll_multiplier: Vec2,
    always_scroll_enabled_direction: bool,
}

impl ScrollAreaScrollHitState {
    pub(crate) const fn new(
        direction_enabled: Vec2b,
        offset: Vec2,
        max_offset: Vec2,
        wheel_scroll_multiplier: Vec2,
        always_scroll_enabled_direction: bool,
    ) -> Self {
        Self {
            direction_enabled,
            offset,
            max_offset,
            wheel_scroll_multiplier,
            always_scroll_enabled_direction,
        }
    }

    pub(crate) fn live_scroll_delta(self, input_delta: Vec2, axis: usize) -> f32 {
        let projected_delta = if self.always_scroll_enabled_direction {
            input_delta.x + input_delta.y
        } else {
            input_delta[axis]
        };
        projected_delta * self.wheel_scroll_multiplier[axis]
    }

    pub(crate) fn accepts_live_axis(self, axis: usize, scroll_delta: f32) -> bool {
        self.accepts_axis(axis, f64::from(scroll_delta))
    }

    fn accepts_projected_delta(self, projected_delta: Option<WidgetScrollDelta>) -> bool {
        match projected_delta {
            Some(projected_delta) => (0..2).any(|axis| {
                let projected_delta = if self.always_scroll_enabled_direction {
                    projected_delta.x + projected_delta.y
                } else if axis == 0 {
                    projected_delta.x
                } else {
                    projected_delta.y
                };
                let scroll_delta = projected_delta * f64::from(self.wheel_scroll_multiplier[axis]);
                self.accepts_axis(axis, scroll_delta)
            }),
            None => (0..2).any(|axis| self.has_potential_capacity(axis)),
        }
    }

    fn accepts_axis(self, axis: usize, scroll_delta: f64) -> bool {
        if !self.direction_enabled[axis] {
            return false;
        }

        let scrolling_up = self.offset[axis] > 0.0 && scroll_delta > 0.0;
        let scrolling_down = self.offset[axis] < self.max_offset[axis] && scroll_delta < 0.0;
        scrolling_up || scrolling_down
    }

    fn has_potential_capacity(self, axis: usize) -> bool {
        if !self.direction_enabled[axis] {
            return false;
        }

        let multiplier = self.wheel_scroll_multiplier[axis];
        let multiplier_can_project = multiplier != 0.0 && !multiplier.is_nan();
        multiplier_can_project
            && (self.offset[axis] > 0.0 || self.offset[axis] < self.max_offset[axis])
    }
}

/// Result of a hit-test against [`WidgetRects`].
///
/// Answers the question "what is under the mouse pointer?".
///
/// Note that this doesn't care if the mouse button is pressed or not,
/// or if we're currently already dragging something.
#[derive(Clone, Debug, Default)]
pub struct WidgetHits {
    /// All widgets close to the pointer, back-to-front.
    ///
    /// This is a superset of all other widgets in this struct.
    pub close: Vec<WidgetRect>,

    /// All widgets that contains the pointer, back-to-front.
    ///
    /// i.e. both a Window and the Button in it can contain the pointer.
    ///
    /// Some of these may be widgets in a layer below the top-most layer.
    ///
    /// This will be used for hovering.
    pub contains_pointer: Vec<WidgetRect>,

    /// If the user would start a clicking now, this is what would be clicked.
    ///
    /// This is the top one under the pointer, or closest one of the top-most.
    pub click: Option<WidgetRect>,

    /// If the user would start a dragging now, this is what would be dragged.
    ///
    /// This is the top one under the pointer, or closest one of the top-most.
    pub drag: Option<WidgetRect>,
}

/// Find the top or closest widgets to the given position,
/// none which is closer than `search_radius`.
pub fn hit_test(
    widgets: &WidgetRects,
    layer_order: &[LayerId],
    layer_to_global: &HashMap<LayerId, TSTransform>,
    pos: Pos2,
    search_radius: f32,
) -> WidgetHits {
    profiling::function_scope!();

    let search_radius_sq = search_radius * search_radius;

    // Transform the position into the local coordinate space of each layer:
    let pos_in_layers: HashMap<LayerId, Pos2> = layer_to_global
        .iter()
        .map(|(layer_id, to_global)| (*layer_id, to_global.inverse() * pos))
        .collect();

    let mut closest_dist_sq = f32::INFINITY;
    let mut closest_hit = None;

    // First pass: find the few widgets close to the given position, sorted back-to-front.
    let mut close: Vec<WidgetRect> = layer_order
        .iter()
        .filter(|layer| layer.order.allow_interaction())
        .flat_map(|&layer_id| widgets.get_layer(layer_id))
        .filter(|&w| {
            if w.interact_rect.is_negative() || w.interact_rect.any_nan() {
                return false;
            }

            let pos_in_layer = pos_in_layers.get(&w.layer_id).copied().unwrap_or(pos);
            // TODO(emilk): we should probably do the distance testing in global space instead
            let dist_sq = w.interact_rect.distance_sq_to_pos(pos_in_layer);

            // In tie, pick last = topmost.
            if dist_sq <= closest_dist_sq {
                closest_dist_sq = dist_sq;
                closest_hit = Some(w);
            }

            dist_sq <= search_radius_sq
        })
        .copied()
        .collect();

    // Transform to global coordinates:
    for hit in &mut close {
        if let Some(to_global) = layer_to_global.get(&hit.layer_id).copied() {
            *hit = hit.transform(to_global);
        }
    }

    close.retain(|rect| !rect.interact_rect.any_nan()); // Protect against bad input and transforms

    // When using layer transforms it is common to stack layers close to each other.
    // For instance, you may have a resize-separator on a panel, with two
    // transform-layers on either side.
    // The resize-separator is technically in a layer _behind_ the transform-layers,
    // but the user doesn't perceive it as such.
    // So how do we handle this case?
    //
    // If we just allow interactions with ALL close widgets,
    // then we might accidentally allow clicks through windows and other bad stuff.
    //
    // Let's try this:
    // * Set up a hit-area (based on search_radius)
    // * Iterate over all hits top-to-bottom
    //   * Stop if any hit covers the whole hit-area, otherwise keep going
    //   * Collect the layers ids in a set
    // * Remove all widgets not in the above layer set
    //
    // This will most often result in only one layer,
    // but if the pointer is at the edge of a layer, we might include widgets in
    // a layer behind it.

    let mut included_layers: ahash::HashSet<LayerId> = Default::default();
    for hit in close.iter().rev() {
        included_layers.insert(hit.layer_id);
        let hit_covers_search_area = contains_circle(hit.interact_rect, pos, search_radius);
        if hit_covers_search_area {
            break; // nothing behind this layer could ever be interacted with
        }
    }

    close.retain(|hit| included_layers.contains(&hit.layer_id));

    // If a widget is disabled, treat it as if it isn't sensing anything.
    // This simplifies the code in `hit_test_on_close` so it doesn't have to check
    // the `enabled` flag everywhere:
    for w in &mut close {
        if !w.enabled {
            w.sense -= Sense::CLICK;
            w.sense -= Sense::DRAG;
        }
    }

    // Find widgets which are hidden behind another widget and discard them.
    // This is the case when a widget fully contains another widget and is on a different layer.
    // It prevents "hovering through" widgets when there is a clickable widget behind.

    let mut hidden = IdSet::default();
    for (i, current) in close.iter().enumerate().rev() {
        for next in &close[i + 1..] {
            if next.interact_rect.contains_rect(current.interact_rect)
                && current.layer_id != next.layer_id
            {
                hidden.insert(current.id);
            }
        }
    }

    close.retain(|c| !hidden.contains(&c.id));

    let mut hits = hit_test_on_close(&close, pos);

    hits.contains_pointer = close
        .iter()
        .filter(|widget| widget.interact_rect.contains(pos))
        .copied()
        .collect();

    hits.close = close;

    {
        // Undo the to_global-transform we applied earlier,
        // go back to local layer-coordinates:

        let restore_widget_rect = |w: &mut WidgetRect| {
            *w = widgets.get(w.id).copied().unwrap_or(*w);
        };

        for wr in &mut hits.close {
            restore_widget_rect(wr);
        }
        for wr in &mut hits.contains_pointer {
            restore_widget_rect(wr);
        }
        if let Some(wr) = &mut hits.drag {
            debug_assert!(
                wr.sense.senses_drag(),
                "We should only return drag hits if they sense drag"
            );
            restore_widget_rect(wr);
        }
        if let Some(wr) = &mut hits.click {
            debug_assert!(
                wr.sense.senses_click(),
                "We should only return click hits if they sense click"
            );
            restore_widget_rect(wr);
        }
    }

    hits
}

/// Returns true if the rectangle contains the whole circle.
fn contains_circle(interact_rect: emath::Rect, pos: Pos2, radius: f32) -> bool {
    interact_rect.shrink(radius).contains(pos)
}

fn hit_test_on_close(close: &[WidgetRect], pos: Pos2) -> WidgetHits {
    // First find the best direct hits:
    let hit_click = find_closest_within(
        close.iter().copied().filter(|w| w.sense.senses_click()),
        pos,
        0.0,
    );
    let hit_drag = find_closest_within(
        close.iter().copied().filter(|w| w.sense.senses_drag()),
        pos,
        0.0,
    );

    match (hit_click, hit_drag) {
        (None, None) => {
            // No direct hit on anything. Find the closest interactive widget.

            let closest = find_closest(
                close
                    .iter()
                    .copied()
                    .filter(|w| w.sense.senses_click() || w.sense.senses_drag()),
                pos,
            );

            if let Some(closest) = closest {
                WidgetHits {
                    click: closest.sense.senses_click().then_some(closest),
                    drag: closest.sense.senses_drag().then_some(closest),
                    ..Default::default()
                }
            } else {
                // Found nothing
                WidgetHits {
                    click: None,
                    drag: None,
                    ..Default::default()
                }
            }
        }

        (None, Some(hit_drag)) => {
            // We have a perfect hit on a drag, but not on click.

            // We have a direct hit on something that implements drag.
            // This could be a big background thing, like a `ScrollArea` background,
            // or a moveable window.
            // It could also be something small, like a slider, or panel resize handle.

            let closest_click = find_closest(
                close.iter().copied().filter(|w| w.sense.senses_click()),
                pos,
            );
            if let Some(closest_click) = closest_click {
                if closest_click.sense.senses_drag() {
                    // We have something close that sense both clicks and drag.
                    // Should we use it over the direct drag-hit?
                    if hit_drag
                        .interact_rect
                        .contains_rect(closest_click.interact_rect)
                    {
                        // This is a smaller thing on a big background - help the user hit it,
                        // and ignore the big drag background.
                        WidgetHits {
                            click: Some(closest_click),
                            drag: Some(closest_click),
                            ..Default::default()
                        }
                    } else {
                        // The drag-widget is separate from the click-widget,
                        // so return only the drag-widget
                        WidgetHits {
                            click: None,
                            drag: Some(hit_drag),
                            ..Default::default()
                        }
                    }
                } else {
                    // This is a close pure-click widget.
                    // However, we should be careful to only return two different widgets
                    // when it is absolutely not going to confuse the user.
                    if hit_drag
                        .interact_rect
                        .contains_rect(closest_click.interact_rect)
                    {
                        // The drag widget is a big background thing (scroll area),
                        // so returning a separate click widget should not be confusing
                        WidgetHits {
                            click: Some(closest_click),
                            drag: Some(hit_drag),
                            ..Default::default()
                        }
                    } else {
                        // The two widgets are just two normal small widgets close to each other.
                        // Highlighting both would be very confusing.
                        WidgetHits {
                            click: None,
                            drag: Some(hit_drag),
                            ..Default::default()
                        }
                    }
                }
            } else {
                // No close clicks.
                // Maybe there is a close drag widget, that is a smaller
                // widget floating on top of a big background?
                // If so, it would be nice to help the user click that.
                let closest_drag = find_closest(
                    close
                        .iter()
                        .copied()
                        .filter(|w| w.sense.senses_drag() && w.id != hit_drag.id),
                    pos,
                );

                if let Some(closest_drag) = closest_drag
                    && hit_drag
                        .interact_rect
                        .contains_rect(closest_drag.interact_rect)
                {
                    // `hit_drag` is a big background thing and `closest_drag` is something small on top of it.
                    // Be helpful and return the small things:
                    return WidgetHits {
                        click: None,
                        drag: Some(closest_drag),
                        ..Default::default()
                    };
                }

                WidgetHits {
                    click: None,
                    drag: Some(hit_drag),
                    ..Default::default()
                }
            }
        }

        (Some(hit_click), None) => {
            // We have a perfect hit on a click-widget, but not on a drag-widget.
            //
            // Note that we don't look for a close drag widget in this case,
            // because I can't think of a case where that would be helpful.
            // This is in contrast with the opposite case,
            // where when hovering directly over a drag-widget (like a big ScrollArea),
            // we look for close click-widgets (e.g. buttons).
            // This is because big background drag-widgets (ScrollArea, Window) are common,
            // but big clickable things aren't.
            // Even if they were, I think it would be confusing for a user if clicking
            // a drag-only widget would click something _behind_ it.

            WidgetHits {
                click: Some(hit_click),
                drag: None,
                ..Default::default()
            }
        }

        (Some(hit_click), Some(hit_drag)) => {
            // We have a perfect hit on both click and drag. Which is the topmost?
            #[expect(clippy::unwrap_used)]
            let click_idx = close.iter().position(|w| *w == hit_click).unwrap();

            #[expect(clippy::unwrap_used)]
            let drag_idx = close.iter().position(|w| *w == hit_drag).unwrap();

            let click_is_on_top_of_drag = drag_idx < click_idx;
            if click_is_on_top_of_drag {
                if hit_click.sense.senses_drag() {
                    // The top thing senses both clicks and drags.
                    WidgetHits {
                        click: Some(hit_click),
                        drag: Some(hit_click),
                        ..Default::default()
                    }
                } else {
                    // They are interested in different things,
                    // and click is on top. Report both hits,
                    // e.g. the top Button and the ScrollArea behind it.
                    WidgetHits {
                        click: Some(hit_click),
                        drag: Some(hit_drag),
                        ..Default::default()
                    }
                }
            } else {
                if hit_drag.sense.senses_click() {
                    // The top thing senses both clicks and drags.
                    WidgetHits {
                        click: Some(hit_drag),
                        drag: Some(hit_drag),
                        ..Default::default()
                    }
                } else {
                    // The top things senses only drags,
                    // so we ignore the click-widget, because it would be confusing
                    // if clicking a drag-widget would actually click something else below it.
                    WidgetHits {
                        click: None,
                        drag: Some(hit_drag),
                        ..Default::default()
                    }
                }
            }
        }
    }
}

fn find_closest(widgets: impl Iterator<Item = WidgetRect>, pos: Pos2) -> Option<WidgetRect> {
    find_closest_within(widgets, pos, f32::INFINITY)
}

fn find_closest_within(
    widgets: impl Iterator<Item = WidgetRect>,
    pos: Pos2,
    max_dist: f32,
) -> Option<WidgetRect> {
    let mut closest: Option<WidgetRect> = None;
    let mut closest_dist_sq = max_dist * max_dist;
    for widget in widgets {
        if widget.interact_rect.is_negative() {
            continue;
        }

        let dist_sq = widget.interact_rect.distance_sq_to_pos(pos);

        // In case of a tie, take the last one = the one on top.
        if dist_sq <= closest_dist_sq {
            closest_dist_sq = dist_sq;
            closest = Some(widget);
        }
    }

    closest
}

#[cfg(test)]
mod tests {
    #![expect(clippy::print_stdout)]

    use emath::{Rect, pos2, vec2};

    use crate::{Id, Sense};

    use super::*;

    fn wr(id: Id, sense: Sense, rect: Rect) -> WidgetRect {
        WidgetRect {
            id,
            parent_id: Id::NULL,
            layer_id: LayerId::background(),
            rect,
            interact_rect: rect,
            sense,
            enabled: true,
        }
    }

    #[test]
    fn buttons_on_window() {
        let widgets = vec![
            wr(
                Id::new("bg-area"),
                Sense::drag(),
                Rect::from_min_size(pos2(0.0, 0.0), vec2(100.0, 100.0)),
            ),
            wr(
                Id::new("click"),
                Sense::click(),
                Rect::from_min_size(pos2(10.0, 10.0), vec2(10.0, 10.0)),
            ),
            wr(
                Id::new("click-and-drag"),
                Sense::click_and_drag(),
                Rect::from_min_size(pos2(100.0, 10.0), vec2(10.0, 10.0)),
            ),
        ];

        // Perfect hit:
        let hits = hit_test_on_close(&widgets, pos2(15.0, 15.0));
        assert_eq!(hits.click.unwrap().id, Id::new("click"));
        assert_eq!(hits.drag.unwrap().id, Id::new("bg-area"));

        // Close hit:
        let hits = hit_test_on_close(&widgets, pos2(5.0, 5.0));
        assert_eq!(hits.click.unwrap().id, Id::new("click"));
        assert_eq!(hits.drag.unwrap().id, Id::new("bg-area"));

        // Perfect hit:
        let hits = hit_test_on_close(&widgets, pos2(105.0, 15.0));
        assert_eq!(hits.click.unwrap().id, Id::new("click-and-drag"));
        assert_eq!(hits.drag.unwrap().id, Id::new("click-and-drag"));

        // Close hit - should still ignore the drag-background so as not to confuse the user:
        let hits = hit_test_on_close(&widgets, pos2(105.0, 5.0));
        assert_eq!(hits.click.unwrap().id, Id::new("click-and-drag"));
        assert_eq!(hits.drag.unwrap().id, Id::new("click-and-drag"));
    }

    #[test]
    fn thin_resize_handle_next_to_label() {
        let widgets = vec![
            wr(
                Id::new("bg-area"),
                Sense::drag(),
                Rect::from_min_size(pos2(0.0, 0.0), vec2(100.0, 100.0)),
            ),
            wr(
                Id::new("bg-left-label"),
                Sense::click_and_drag(),
                Rect::from_min_size(pos2(0.0, 0.0), vec2(40.0, 100.0)),
            ),
            wr(
                Id::new("thin-drag-handle"),
                Sense::drag(),
                Rect::from_min_size(pos2(30.0, 0.0), vec2(70.0, 100.0)),
            ),
            wr(
                Id::new("fg-right-label"),
                Sense::click_and_drag(),
                Rect::from_min_size(pos2(60.0, 0.0), vec2(50.0, 100.0)),
            ),
        ];

        for (i, w) in widgets.iter().enumerate() {
            println!("Widget {i}: {:?}", w.id);
        }

        // In the middle of the bg-left-label:
        let hits = hit_test_on_close(&widgets, pos2(25.0, 50.0));
        assert_eq!(hits.click.unwrap().id, Id::new("bg-left-label"));
        assert_eq!(hits.drag.unwrap().id, Id::new("bg-left-label"));

        // On both the left click-and-drag and thin handle, but the thin handle is on top and should win:
        let hits = hit_test_on_close(&widgets, pos2(35.0, 50.0));
        assert_eq!(hits.click, None);
        assert_eq!(hits.drag.unwrap().id, Id::new("thin-drag-handle"));

        // Only on the thin-drag-handle:
        let hits = hit_test_on_close(&widgets, pos2(50.0, 50.0));
        assert_eq!(hits.click, None);
        assert_eq!(hits.drag.unwrap().id, Id::new("thin-drag-handle"));

        // On both the thin handle and right label. The label is on top and should win
        let hits = hit_test_on_close(&widgets, pos2(65.0, 50.0));
        assert_eq!(hits.click.unwrap().id, Id::new("fg-right-label"));
        assert_eq!(hits.drag.unwrap().id, Id::new("fg-right-label"));
    }
}

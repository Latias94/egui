//! Paint-local append checks with lazy snapshots at mutable graphics escapes.

use alloc::sync::{Arc, Weak};
use std::collections::{HashMap, HashSet};

use epaint::{ClippedShape, Galley, Shape, TextShape, emath::TSTransform, mutex::Mutex};

use crate::{LayerId, Memory, layers::GraphicLayers};

pub(crate) type SharedSession = Arc<Mutex<Session>>;
pub(crate) type Checkpoint = Arc<Mutex<Option<Before>>>;

/// The viewport owns only weak registrations, never captured user payloads.
#[derive(Default)]
pub(crate) struct Registry {
    sessions: Vec<Weak<Mutex<Session>>>,
}

/// Keep upgraded owners alive until after the surrounding Context lock is gone.
#[derive(Default)]
pub(crate) struct Releases {
    sessions: Vec<SharedSession>,
    checkpoints: Vec<Checkpoint>,
}

#[derive(Default)]
pub(crate) struct Session {
    verified: HashMap<LayerId, usize>,
    checkpoints: Vec<Weak<Mutex<Option<Before>>>>,
}

impl Registry {
    pub(crate) fn register(&mut self, session: &SharedSession) {
        self.sessions.retain(|session| session.strong_count() != 0);
        let registration = Arc::downgrade(session);
        if !self.sessions.iter().any(|old| old.ptr_eq(&registration)) {
            self.sessions.push(registration);
        }
    }

    pub(crate) fn before_opaque_write(
        &mut self,
        graphics: &GraphicLayers,
        releases: &mut Releases,
    ) {
        self.before_write(graphics, None, releases);
    }

    pub(crate) fn before_index_write(
        &mut self,
        graphics: &GraphicLayers,
        layer: LayerId,
        index: usize,
        releases: &mut Releases,
    ) {
        self.before_write(graphics, Some((layer, index)), releases);
    }

    fn before_write(
        &mut self,
        graphics: &GraphicLayers,
        index: Option<(LayerId, usize)>,
        releases: &mut Releases,
    ) {
        self.sessions.retain(|session| session.strong_count() != 0);
        for registration in &self.sessions {
            let Some(session) = registration.upgrade() else {
                continue;
            };
            releases.sessions.push(session);
            let mut session = releases.sessions.last().unwrap().lock();
            if let Some((layer, index)) = index {
                if let Some(verified) = session.verified.get_mut(&layer) {
                    *verified = (*verified).min(index);
                }
            } else {
                session.verified.clear();
            }
            session
                .checkpoints
                .retain(|checkpoint| checkpoint.strong_count() != 0);
            for registration in &session.checkpoints {
                let Some(checkpoint) = registration.upgrade() else {
                    continue;
                };
                releases.checkpoints.push(checkpoint);
                let mut checkpoint = releases.checkpoints.last().unwrap().lock();
                if let Some(before) = checkpoint.as_mut()
                    && index.is_none_or(|(layer, index)| before.protects(layer, index))
                {
                    before.materialize(graphics);
                }
            }
        }
    }
}

pub(crate) fn checkpoint(session: &SharedSession, before: Option<Before>) -> Checkpoint {
    let checkpoint = Arc::new(Mutex::new(before));
    session.lock().checkpoints.push(Arc::downgrade(&checkpoint));
    checkpoint
}

struct LayerBefore {
    layer: LayerId,
    transform: Option<TSTransform>,
    length: usize,
}

struct Frozen {
    current: Vec<ClippedShape>,
    others: Vec<Vec<ClippedShape>>,
}

pub(crate) struct Before {
    current: LayerBefore,
    others: Vec<LayerBefore>,
    frozen: Option<Frozen>,
}

fn checked_layers(memory: &Memory, current: LayerId) -> Option<Vec<LayerId>> {
    let mut layers = memory.layer_ids().collect::<Vec<_>>();
    let mut unique = HashSet::with_capacity(layers.len());
    if !layers.iter().all(|layer| unique.insert(*layer)) {
        return None;
    }
    if !unique.contains(&current) {
        layers.push(current);
    }
    Some(layers)
}

impl Before {
    pub(crate) fn capture(
        memory: &Memory,
        graphics: &GraphicLayers,
        current: LayerId,
    ) -> Option<Self> {
        let layers = checked_layers(memory, current)?;
        if graphics.has_unknown_shapes(&layers) {
            return None;
        }
        let capture = |layer| LayerBefore {
            layer,
            transform: memory.to_global.get(&layer).copied(),
            length: graphics.get(layer).map_or(0, |list| list.next_idx().0),
        };
        Some(Self {
            current: capture(current),
            others: layers
                .into_iter()
                .filter(|layer| *layer != current)
                .map(capture)
                .collect(),
            frozen: None,
        })
    }

    fn protects(&self, layer: LayerId, index: usize) -> bool {
        std::iter::once(&self.current)
            .chain(&self.others)
            .any(|before| before.layer == layer && index < before.length)
    }

    fn materialize(&mut self, graphics: &GraphicLayers) {
        if self.frozen.is_some() {
            return;
        }
        let shapes = |before: &LayerBefore| {
            graphics.get(before.layer).map_or_else(Vec::new, |list| {
                list.all_entries().take(before.length).cloned().collect()
            })
        };
        self.frozen = Some(Frozen {
            current: shapes(&self.current),
            others: self.others.iter().map(shapes).collect(),
        });
    }

    pub(crate) fn is_preserved(
        &self,
        memory: &Memory,
        graphics: &GraphicLayers,
        current: LayerId,
        session: &mut Session,
        comparison: &mut Comparison,
    ) -> bool {
        let Some(layers) = checked_layers(memory, current) else {
            return false;
        };
        if graphics.has_unknown_shapes(&layers) {
            return false;
        }
        let mut verified = Vec::new();
        if current != self.current.layer
            || memory.to_global.get(&current).copied() != self.current.transform
            || !self.layer_matches(
                graphics,
                &self.current,
                self.frozen.as_ref().map(|frozen| frozen.current.as_slice()),
                true,
                session,
                comparison,
            )
        {
            return false;
        }
        verified.push((current, self.current.length));

        // Preserve the old comparison order: current contents precede the other roster.
        let others = layers.iter().filter(|layer| **layer != current);
        if others.clone().count() != self.others.len() {
            return false;
        }
        for (index, (&layer, before)) in others.zip(&self.others).enumerate() {
            if layer != before.layer
                || memory.to_global.get(&layer).copied() != before.transform
                || !self.layer_matches(
                    graphics,
                    before,
                    self.frozen
                        .as_ref()
                        .map(|frozen| frozen.others[index].as_slice()),
                    false,
                    session,
                    comparison,
                )
            {
                return false;
            }
            verified.push((layer, before.length));
        }
        if self.frozen.is_none() {
            for (layer, length) in verified {
                let frontier = session.verified.entry(layer).or_default();
                *frontier = (*frontier).max(length);
            }
        }
        true
    }

    fn layer_matches(
        &self,
        graphics: &GraphicLayers,
        before: &LayerBefore,
        frozen: Option<&[ClippedShape]>,
        allow_suffix: bool,
        session: &Session,
        comparison: &mut Comparison,
    ) -> bool {
        let list = graphics.get(before.layer);
        let length = list.map_or(0, |list| list.next_idx().0);
        if length < before.length || (!allow_suffix && length != before.length) {
            return false;
        }
        if let Some(frozen) = frozen {
            return frozen.len() == before.length
                && list.is_none_or(|list| {
                    list.all_entries()
                        .take(before.length)
                        .zip(frozen)
                        .all(|(live, old)| comparison.clipped_shapes_equal(live, old))
                });
        }
        let start = session
            .verified
            .get(&before.layer)
            .copied()
            .unwrap_or_default()
            .min(before.length);
        list.is_none_or(|list| {
            list.all_entries()
                .skip(start)
                .take(before.length - start)
                .all(|shape| comparison.clipped_shapes_equal(shape, shape))
        })
    }
}

/// The same-owner memo is paint-local and pins every memoized galley.
#[derive(Default)]
pub(crate) struct Comparison {
    galleys: HashMap<usize, (Arc<Galley>, bool)>,
}

#[cfg(test)]
thread_local! {
    static SHAPE_COMPARISONS: core::cell::Cell<usize> = const { core::cell::Cell::new(0) };
}

impl Comparison {
    fn clipped_shapes_equal(&mut self, left: &ClippedShape, right: &ClippedShape) -> bool {
        #[cfg(test)]
        SHAPE_COMPARISONS.with(|count| count.set(count.get() + 1));
        left.clip_rect == right.clip_rect
            && self.shapes_equal(&left.shape, &right.shape)
            && left.transform_after_tessellation == right.transform_after_tessellation
    }

    fn shapes_equal(&mut self, left: &Shape, right: &Shape) -> bool {
        match (left, right) {
            (Shape::Vec(left), Shape::Vec(right)) => {
                left.len() == right.len()
                    && left
                        .iter()
                        .zip(right)
                        .all(|(left, right)| self.shapes_equal(left, right))
            }
            (Shape::Text(left), Shape::Text(right)) => {
                let TextShape {
                    pos,
                    galley,
                    underline,
                    fallback_color,
                    override_text_color,
                    opacity_factor,
                    angle,
                } = left;
                *pos == right.pos
                    && self.galleys_equal(galley, &right.galley)
                    && *underline == right.underline
                    && *fallback_color == right.fallback_color
                    && *override_text_color == right.override_text_color
                    && *opacity_factor == right.opacity_factor
                    && *angle == right.angle
            }
            _ => left == right,
        }
    }

    fn galleys_equal(&mut self, left: &Arc<Galley>, right: &Arc<Galley>) -> bool {
        if !Arc::ptr_eq(left, right) {
            return left == right;
        }
        self.galleys
            .entry(Arc::as_ptr(left) as usize)
            .or_insert_with(|| {
                let equal = left.as_ref() == right.as_ref();
                (Arc::clone(left), equal)
            })
            .1
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Context, Id, Order, Painter, RawInput, Rect, pos2, vec2};
    use std::panic::{AssertUnwindSafe, catch_unwind};

    type Auditor = Box<dyn FnMut(&Painter, &mut dyn FnMut(), &mut bool)>;

    fn input() -> RawInput {
        RawInput {
            screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(100.0, 100.0))),
            ..RawInput::default()
        }
    }

    fn with_painter(mut check: impl FnMut(&Painter)) {
        Context::default()
            .run_ui(input(), |ui| check(ui.painter()))
            .drop_without_applying_deltas();
    }

    fn preserved(auditor: &mut Auditor, painter: &Painter, mut paint: impl FnMut()) -> bool {
        let mut valid = true;
        auditor(painter, &mut paint, &mut valid);
        valid
    }

    fn known_layer(context: &Context) -> LayerId {
        let layer = LayerId::new(Order::Middle, Id::unique("paint-audit-other"));
        context.memory_mut(|memory| {
            memory
                .areas_mut()
                .set_state(layer, crate::AreaState::default());
        });
        layer
    }

    #[test]
    fn append_and_suffix_set_preserve_the_existing_prefix() {
        with_painter(|painter| {
            painter.add(Shape::Noop);
            let mut auditor = painter.ctx().paint_append_auditor().unwrap();
            for _ in 0..3 {
                assert!(preserved(&mut auditor, painter, || {
                    let slot = painter.add(Shape::Noop);
                    painter.set(slot, Shape::Vec(Vec::new()));
                }));
            }
        });
    }

    #[test]
    fn repeated_append_checks_compare_a_linear_number_of_shapes_after_fallback() {
        for count in [512, 1024] {
            with_painter(|painter| {
                let mut auditor = painter.ctx().paint_append_auditor().unwrap();
                SHAPE_COMPARISONS.with(|comparisons| comparisons.set(0));
                let append = || {
                    let slot = painter.add(Shape::Noop);
                    painter.set(slot, Shape::Vec(Vec::new()));
                };
                for _ in 0..count {
                    assert!(preserved(&mut auditor, painter, append));
                }
                assert!(SHAPE_COMPARISONS.with(|comparisons| comparisons.get()) <= count + 32);
                assert!(preserved(&mut auditor, painter, || {
                    painter.ctx().graphics_mut(|graphics| {
                        *graphics = graphics.clone();
                    });
                }));
                for _ in 0..count {
                    assert!(preserved(&mut auditor, painter, append));
                }
                assert!(SHAPE_COMPARISONS.with(|comparisons| comparisons.get()) <= 5 * count + 32);
            });
        }
    }

    #[test]
    fn mutable_escapes_freeze_before_replace_swap_clone_transform_and_nested_mutation() {
        for mutation in 0..9 {
            with_painter(|painter| {
                let other = known_layer(painter.ctx());
                painter
                    .ctx()
                    .layer_painter(other)
                    .add(Shape::Vec(Vec::new()));
                let slot = painter.add(Shape::Vec(vec![Shape::Noop]));
                let mut auditor = painter.ctx().paint_append_auditor().unwrap();
                let actual = preserved(&mut auditor, painter, || match mutation {
                    0 => painter.set(slot, Shape::Noop),
                    1 => painter.ctx().graphics_mut(|graphics| {
                        *graphics.get_mut(painter.layer_id()).unwrap() = Default::default();
                    }),
                    2 => painter.ctx().graphics_mut(|graphics| {
                        *graphics = GraphicLayers::default();
                    }),
                    3 => painter.ctx().graphics_mut(|graphics| {
                        *graphics = graphics.clone();
                    }),
                    4 => painter.ctx().graphics_mut(|graphics| {
                        let mut current = std::mem::take(graphics.entry(painter.layer_id()));
                        std::mem::swap(&mut current, graphics.entry(other));
                        *graphics.entry(painter.layer_id()) = current;
                    }),
                    5 => painter.ctx().graphics_mut(|graphics| {
                        graphics
                            .entry(painter.layer_id())
                            .mutate_shape(slot, |shape| {
                                let Shape::Vec(children) = &mut shape.shape else {
                                    unreachable!();
                                };
                                children[0] = Shape::Vec(Vec::new());
                            });
                    }),
                    6 => painter.ctx().graphics_mut(|graphics| {
                        graphics
                            .entry(painter.layer_id())
                            .transform(TSTransform::from_translation(vec2(1.0, 0.0)));
                    }),
                    7 => painter
                        .ctx()
                        .set_transform_layer(painter.layer_id(), TSTransform::from_scaling(2.0)),
                    8 => painter.ctx().graphics_mut(|graphics| {
                        let _shapes = graphics.drain(&[], &Default::default());
                    }),
                    _ => unreachable!(),
                });
                assert_eq!(actual, mutation == 3, "mutation {mutation}");
            });
        }
    }

    #[test]
    fn graphics_replacement_between_checks_invalidates_the_session_frontier() {
        with_painter(|painter| {
            painter.add(Shape::Noop);
            let mut auditor = painter.ctx().paint_append_auditor().unwrap();
            assert!(preserved(&mut auditor, painter, || {}));
            painter.ctx().graphics_mut(|graphics| {
                let mut replacement = GraphicLayers::default();
                replacement.entry(painter.layer_id()).add(
                    painter.clip_rect(),
                    Shape::circle_filled(pos2(10.0, 10.0), f32::NAN, crate::Color32::WHITE),
                );
                *graphics = replacement;
            });
            assert!(!preserved(&mut auditor, painter, || {}));
            painter.set(crate::layers::ShapeIdx(0), Shape::Noop);
            assert!(preserved(&mut auditor, painter, || {}));
        });
    }

    #[test]
    fn lazy_checks_preserve_nan_results_and_debug_panics() {
        for nested in [false, true] {
            with_painter(|painter| {
                let mut clip = painter.clip_rect();
                if !nested {
                    clip.max.x = f32::NAN;
                }
                let shape = if nested {
                    Shape::Vec(vec![Shape::circle_filled(
                        pos2(10.0, 10.0),
                        f32::NAN,
                        crate::Color32::WHITE,
                    )])
                } else {
                    Shape::Noop
                };
                painter.ctx().graphics_mut(|graphics| {
                    graphics.entry(painter.layer_id()).add(clip, shape.clone());
                });
                let expected = catch_unwind(AssertUnwindSafe(|| {
                    let value = ClippedShape::new(clip, shape.clone());
                    value == value.clone()
                }))
                .map_err(|_| ());
                let mut auditor = painter.ctx().paint_append_auditor().unwrap();
                for _ in 0..2 {
                    let actual =
                        catch_unwind(AssertUnwindSafe(|| preserved(&mut auditor, painter, || {})))
                            .map_err(|_| ());
                    assert_eq!(actual, expected);
                }
            });
        }
    }

    #[test]
    fn unavailable_begin_and_unknown_finish_precede_nan_equality() {
        for unknown_at_begin in [false, true] {
            with_painter(|painter| {
                let unknown = LayerId::new(Order::Foreground, Id::unique("paint-audit-unknown"));
                let mut clip = painter.clip_rect();
                clip.max.x = f32::NAN;
                painter.ctx().graphics_mut(|graphics| {
                    graphics.entry(painter.layer_id()).add(clip, Shape::Noop);
                    if unknown_at_begin {
                        graphics
                            .entry(unknown)
                            .add(painter.clip_rect(), Shape::Noop);
                    }
                });
                let mut auditor = painter.ctx().paint_append_auditor().unwrap();
                assert!(!preserved(&mut auditor, painter, || {
                    if unknown_at_begin {
                        painter.ctx().graphics_mut(|graphics| {
                            *graphics.entry(unknown) = Default::default();
                        });
                    } else {
                        painter.ctx().layer_painter(unknown).add(Shape::Noop);
                    }
                }));
            });
        }
    }

    #[cfg(feature = "default_fonts")]
    #[test]
    fn text_galley_nan_is_compared_at_its_original_short_circuit_point() {
        for field in 0..3 {
            with_painter(|painter| {
                let mut galley = painter.layout_no_wrap(
                    "Paint audit".to_owned(),
                    crate::FontId::proportional(14.0),
                    crate::Color32::WHITE,
                );
                let value = Arc::make_mut(&mut galley);
                match field {
                    0 => value.pixels_per_point = f32::NAN,
                    1 => value.rect.max.x = f32::NAN,
                    2 => Arc::make_mut(&mut value.job).first_row_min_height = f32::NAN,
                    _ => unreachable!(),
                }
                let shape = Shape::Vec(vec![Shape::Text(TextShape::new(
                    pos2(10.0, 10.0),
                    galley,
                    crate::Color32::WHITE,
                ))]);
                let expected = catch_unwind(AssertUnwindSafe(|| {
                    let value = ClippedShape::new(painter.clip_rect(), shape.clone());
                    value == value.clone()
                }))
                .map_err(|_| ());
                painter.add(shape);
                let mut auditor = painter.ctx().paint_append_auditor().unwrap();
                for _ in 0..2 {
                    let actual =
                        catch_unwind(AssertUnwindSafe(|| preserved(&mut auditor, painter, || {})))
                            .map_err(|_| ());
                    assert_eq!(actual, expected, "field {field}");
                }
            });
        }
    }

    #[cfg(feature = "default_fonts")]
    #[test]
    fn galley_memo_lifetime_matches_same_owner_and_distinct_owner_comparisons() {
        for replace in [false, true] {
            with_painter(|painter| {
                let galley = Arc::new(
                    painter
                        .layout_no_wrap(
                            "Paint audit".to_owned(),
                            crate::FontId::proportional(14.0),
                            crate::Color32::WHITE,
                        )
                        .as_ref()
                        .clone(),
                );
                let weak = Arc::downgrade(&galley);
                let slot = painter.add(Shape::Text(TextShape::new(
                    pos2(10.0, 10.0),
                    galley,
                    crate::Color32::WHITE,
                )));
                let mut auditor = painter.ctx().paint_append_auditor().unwrap();
                assert!(preserved(&mut auditor, painter, || {
                    if replace {
                        let replacement = Arc::new(weak.upgrade().unwrap().as_ref().clone());
                        painter.set(
                            slot,
                            Shape::Text(TextShape::new(
                                pos2(10.0, 10.0),
                                replacement,
                                crate::Color32::WHITE,
                            )),
                        );
                    }
                }));
                let removed = painter
                    .ctx()
                    .graphics_mut(|graphics| std::mem::take(graphics.entry(painter.layer_id())));
                drop(removed);
                assert_eq!(weak.upgrade().is_some(), !replace);
                drop(auditor);
                assert!(weak.upgrade().is_none());
            });
        }
    }

    #[test]
    fn lengths_and_current_contents_keep_the_original_short_circuit_order() {
        for change in 0..3 {
            with_painter(|painter| {
                let other = known_layer(painter.ctx());
                let mut clip = painter.clip_rect();
                clip.max.x = f32::NAN;
                let current = painter.add(Shape::Noop);
                painter.ctx().graphics_mut(|graphics| {
                    let target = if change == 0 {
                        painter.layer_id()
                    } else {
                        other
                    };
                    graphics.entry(target).add(clip, Shape::Noop);
                    graphics.entry(target).add(painter.clip_rect(), Shape::Noop);
                });
                let mut auditor = painter.ctx().paint_append_auditor().unwrap();
                assert!(!preserved(&mut auditor, painter, || match change {
                    0 => painter.ctx().graphics_mut(|graphics| {
                        *graphics.entry(painter.layer_id()) = Default::default();
                    }),
                    1 => painter.set(current, Shape::Vec(Vec::new())),
                    2 => painter.ctx().graphics_mut(|graphics| {
                        let list = graphics.entry(other);
                        *list = Default::default();
                        list.add(clip, Shape::Noop);
                    }),
                    _ => unreachable!(),
                }));
            });
        }
    }

    struct ReenterOnDrop {
        context: Context,
        layer: LayerId,
        events: Arc<Mutex<Vec<&'static str>>>,
        panic: bool,
    }

    impl Drop for ReenterOnDrop {
        fn drop(&mut self) {
            self.events.lock().push("released");
            self.context.graphics_mut(|graphics| {
                graphics
                    .entry(self.layer)
                    .add(Rect::EVERYTHING, Shape::Noop);
            });
            assert!(!self.panic, "deliberate callback destructor panic");
        }
    }

    fn callback(
        painter: &Painter,
        events: &Arc<Mutex<Vec<&'static str>>>,
        panic: bool,
    ) -> Weak<ReenterOnDrop> {
        let owner = Arc::new(ReenterOnDrop {
            context: painter.ctx().clone(),
            layer: painter.layer_id(),
            events: Arc::clone(events),
            panic,
        });
        let weak = Arc::downgrade(&owner);
        painter.add(Shape::Callback(epaint::PaintCallback {
            rect: painter.clip_rect(),
            callback: owner,
        }));
        weak
    }

    #[test]
    fn removed_payloads_drop_outside_the_lock_after_the_aggregate_is_updated() {
        for panic in [false, true] {
            with_painter(|painter| {
                let events = Arc::new(Mutex::new(Vec::new()));
                let owner = callback(painter, &events, panic);
                let mut auditor = painter.ctx().paint_append_auditor().unwrap();
                let mut aggregate = true;
                let result = catch_unwind(AssertUnwindSafe(|| {
                    auditor(
                        painter,
                        &mut || {
                            let removed = painter.ctx().graphics_mut(|graphics| {
                                std::mem::take(graphics.entry(painter.layer_id()))
                            });
                            drop(removed);
                            assert!(owner.upgrade().is_some());
                            assert!(events.lock().is_empty());
                        },
                        &mut aggregate,
                    );
                }));
                assert_eq!(result.is_err(), panic);
                assert!(!aggregate);
                assert!(owner.upgrade().is_none());
                assert_eq!(*events.lock(), vec!["released"]);
                assert!(preserved(&mut auditor, painter, || {}));
            });
        }
    }

    #[test]
    fn paint_unwind_releases_before_without_leaving_a_live_registration() {
        with_painter(|painter| {
            let events = Arc::new(Mutex::new(Vec::new()));
            let owner = callback(painter, &events, false);
            let mut auditor = painter.ctx().paint_append_auditor().unwrap();
            let mut aggregate = true;
            assert!(
                catch_unwind(AssertUnwindSafe(|| {
                    auditor(
                        painter,
                        &mut || {
                            let removed = painter.ctx().graphics_mut(|graphics| {
                                std::mem::take(graphics.entry(painter.layer_id()))
                            });
                            drop(removed);
                            panic!("deliberate paint panic");
                        },
                        &mut aggregate,
                    );
                }))
                .is_err()
            );
            assert!(aggregate);
            assert!(owner.upgrade().is_none());
            assert_eq!(*events.lock(), vec!["released"]);
            assert!(preserved(&mut auditor, painter, || {}));
        });
    }

    #[test]
    fn end_pass_drain_retains_old_payloads_until_the_check_finishes() {
        let context = Context::default();
        context.begin_pass(input());
        let painter = context.layer_painter(LayerId::background());
        let events = Arc::new(Mutex::new(Vec::new()));
        let owner = callback(&painter, &events, false);
        let mut auditor = context.paint_append_auditor().unwrap();
        assert!(!preserved(&mut auditor, &painter, || {
            context.end_pass().drop_without_applying_deltas();
            assert!(owner.upgrade().is_some());
            assert!(events.lock().is_empty());
        }));
        assert!(owner.upgrade().is_none());
        assert_eq!(*events.lock(), vec!["released"]);
    }
}

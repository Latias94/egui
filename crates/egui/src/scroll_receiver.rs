use crate::{Id, LayerId, Modifiers, Rect, Vec2, ViewportId};

/// How a two-dimensional wheel sample is projected onto a receiver's semantic axes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScrollProjection {
    /// Preserve horizontal and vertical components independently.
    Independent,
    /// Add both input components and apply the result horizontally.
    SumToHorizontal,
    /// Add both input components and apply the result vertically.
    SumToVertical,
    /// Use the horizontal component when non-zero, otherwise use the vertical component.
    HorizontalElseVertical,
    /// Use the vertical component when non-zero, otherwise use the horizontal component.
    VerticalElseHorizontal,
}

impl ScrollProjection {
    pub(crate) fn apply(self, delta: Vec2) -> Vec2 {
        match self {
            Self::Independent => delta,
            Self::SumToHorizontal => crate::vec2(delta.x + delta.y, 0.0),
            Self::SumToVertical => crate::vec2(0.0, delta.x + delta.y),
            Self::HorizontalElseVertical => {
                crate::vec2(if delta.x != 0.0 { delta.x } else { delta.y }, 0.0)
            }
            Self::VerticalElseHorizontal => {
                crate::vec2(0.0, if delta.y != 0.0 { delta.y } else { delta.x })
            }
        }
    }
}

/// Exact movement capabilities for one semantic scroll axis.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScrollAxisCapabilities {
    negative: bool,
    positive: bool,
}

impl ScrollAxisCapabilities {
    /// The receiver cannot move on this axis.
    pub const NONE: Self = Self::new(false, false);

    /// The receiver can move in both directions on this axis.
    pub const BOTH: Self = Self::new(true, true);

    /// Creates exact negative and positive movement capabilities.
    #[must_use]
    pub const fn new(negative: bool, positive: bool) -> Self {
        Self { negative, positive }
    }

    const fn accepts(self, value: f32) -> bool {
        value < 0.0 && self.negative || value > 0.0 && self.positive
    }
}

/// Exact directional capabilities of one scroll receiver in a completed pass.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScrollReceiverConfig {
    projection: ScrollProjection,
    multiplier: Vec2,
    horizontal: ScrollAxisCapabilities,
    vertical: ScrollAxisCapabilities,
}

impl ScrollReceiverConfig {
    /// Creates an exact scroll capability description.
    #[must_use]
    pub const fn new(
        projection: ScrollProjection,
        multiplier: Vec2,
        horizontal: ScrollAxisCapabilities,
        vertical: ScrollAxisCapabilities,
    ) -> Self {
        Self {
            projection,
            multiplier,
            horizontal,
            vertical,
        }
    }

    /// Returns the configured semantic projection.
    #[must_use]
    pub const fn projection(self) -> ScrollProjection {
        self.projection
    }

    pub(crate) fn is_valid(self) -> bool {
        self.multiplier.x.is_finite() && self.multiplier.y.is_finite()
    }

    pub(crate) fn accepts(self, normalized_delta: Vec2) -> bool {
        let delta = self.projection.apply(normalized_delta) * self.multiplier;
        self.horizontal.accepts(delta.x) || self.vertical.accepts(delta.y)
    }
}

/// Immutable scroll receiver facts from one completed logical pass.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScrollReceiver {
    id: Id,
    layer_id: LayerId,
    interact_rect: Rect,
    enabled: bool,
    config: ScrollReceiverConfig,
}

impl ScrollReceiver {
    /// Returns the receiver identity.
    #[must_use]
    pub const fn id(self) -> Id {
        self.id
    }

    /// Returns the receiver layer.
    #[must_use]
    pub const fn layer_id(self) -> LayerId {
        self.layer_id
    }

    /// Returns the exact clipped interaction rectangle.
    #[must_use]
    pub const fn interact_rect(self) -> Rect {
        self.interact_rect
    }

    /// Returns whether the receiver was enabled in the completed pass.
    #[must_use]
    pub const fn enabled(self) -> bool {
        self.enabled
    }

    /// Returns the exact semantic scroll configuration.
    #[must_use]
    pub const fn config(self) -> ScrollReceiverConfig {
        self.config
    }
}

/// Opaque, affine slot reserved before nested scroll receivers are registered.
#[derive(Debug)]
#[must_use = "a scroll receiver reservation must be finalized in the same pass"]
pub struct ScrollReceiverReservation {
    viewport_id: ViewportId,
    pass_nr: u64,
    layer_id: LayerId,
    id: Id,
    slot: usize,
}

/// Why a scroll receiver reservation could not be finalized.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScrollReceiverFinalizeError {
    /// The reservation belongs to another viewport, pass, or layer.
    ReservationScopeChanged,
    /// The receiver rectangle or multiplier contains a non-finite value.
    InvalidGeometry,
    /// The slot was already finalized or conflicted with another reservation.
    ReservationUnavailable,
}

impl std::fmt::Display for ScrollReceiverFinalizeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::ReservationScopeChanged => "scroll receiver reservation scope changed",
            Self::InvalidGeometry => "scroll receiver geometry is invalid",
            Self::ReservationUnavailable => "scroll receiver reservation is unavailable",
        })
    }
}

impl std::error::Error for ScrollReceiverFinalizeError {}

/// Typed result of probing a frozen scroll hit graph.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScrollProbe {
    /// A receiver accepts the normalized sample.
    Receiver {
        /// The exact completed-pass receiver.
        receiver: ScrollReceiver,
        /// Delta after egui's event-time modifier-axis normalization.
        normalized_delta: Vec2,
    },
    /// A higher layer blocks receivers beneath it.
    Blocked,
    /// No receiver can consume this direction.
    NoReceiver,
    /// The sample has no non-zero component yet.
    AwaitingDelta,
    /// Egui's input options classify the sample as zoom rather than scroll.
    FrameworkOwned,
}

/// Egui's exact modifier-axis interpretation of one wheel sample.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScrollDeltaNormalization {
    /// The sample is a scroll delta after modifier-axis normalization.
    Scroll(Vec2),
    /// The phase has not supplied a non-zero delta yet.
    AwaitingDelta,
    /// The configured zoom modifier reserves this sample for framework zoom.
    FrameworkOwned,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum ScrollReceiverSlot {
    Reserved {
        viewport_id: ViewportId,
        pass_nr: u64,
        layer_id: LayerId,
        id: Id,
    },
    Ready(ScrollReceiver),
    Conflict,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct ScrollReceiverRoster {
    slots: Vec<ScrollReceiverSlot>,
}

impl ScrollReceiverRoster {
    pub(crate) fn clear(&mut self) {
        self.slots.clear();
    }

    pub(crate) fn reserve(
        &mut self,
        viewport_id: ViewportId,
        pass_nr: u64,
        layer_id: LayerId,
        id: Id,
    ) -> ScrollReceiverReservation {
        if let Some((slot, existing)) =
            self.slots
                .iter_mut()
                .enumerate()
                .find(|(_, slot)| match slot {
                    ScrollReceiverSlot::Reserved {
                        viewport_id: existing_viewport,
                        pass_nr: existing_pass,
                        layer_id: existing_layer,
                        id: existing_id,
                    } => {
                        *existing_viewport == viewport_id
                            && *existing_pass == pass_nr
                            && *existing_layer == layer_id
                            && *existing_id == id
                    }
                    ScrollReceiverSlot::Ready(receiver) => {
                        receiver.layer_id == layer_id && receiver.id == id
                    }
                    ScrollReceiverSlot::Conflict => false,
                })
        {
            *existing = ScrollReceiverSlot::Conflict;
            return ScrollReceiverReservation {
                viewport_id,
                pass_nr,
                layer_id,
                id,
                slot,
            };
        }
        let slot = self.slots.len();
        self.slots.push(ScrollReceiverSlot::Reserved {
            viewport_id,
            pass_nr,
            layer_id,
            id,
        });
        ScrollReceiverReservation {
            viewport_id,
            pass_nr,
            layer_id,
            id,
            slot,
        }
    }

    #[expect(
        clippy::needless_pass_by_value,
        reason = "consuming the reservation is the affine protocol boundary"
    )]
    pub(crate) fn finalize(
        &mut self,
        reservation: ScrollReceiverReservation,
        receiver: ScrollReceiver,
    ) -> Result<ScrollReceiver, ScrollReceiverFinalizeError> {
        let ScrollReceiverReservation {
            viewport_id: reservation_viewport,
            pass_nr: reservation_pass,
            layer_id: reservation_layer,
            id: reservation_id,
            slot: reservation_slot,
        } = reservation;
        let Some(slot) = self.slots.get_mut(reservation_slot) else {
            return Err(ScrollReceiverFinalizeError::ReservationUnavailable);
        };
        let ScrollReceiverSlot::Reserved {
            viewport_id,
            pass_nr,
            layer_id,
            id,
        } = slot
        else {
            return Err(ScrollReceiverFinalizeError::ReservationUnavailable);
        };
        if (*viewport_id, *pass_nr, *layer_id, *id)
            != (
                reservation_viewport,
                reservation_pass,
                reservation_layer,
                reservation_id,
            )
        {
            *slot = ScrollReceiverSlot::Conflict;
            return Err(ScrollReceiverFinalizeError::ReservationUnavailable);
        }
        *slot = ScrollReceiverSlot::Ready(receiver);
        Ok(receiver)
    }

    pub(crate) fn ready(&self) -> impl DoubleEndedIterator<Item = ScrollReceiver> + '_ {
        self.slots.iter().filter_map(|slot| match slot {
            ScrollReceiverSlot::Ready(receiver) => Some(*receiver),
            ScrollReceiverSlot::Reserved { .. } | ScrollReceiverSlot::Conflict => None,
        })
    }
}

impl ScrollReceiverReservation {
    pub(crate) fn scope_matches(&self, ui: &crate::Ui) -> bool {
        self.viewport_id == ui.ctx().viewport_id()
            && self.pass_nr == ui.ctx().cumulative_pass_nr()
            && self.layer_id == ui.layer_id()
    }
}

pub(crate) fn receiver_from_ui(
    ui: &crate::Ui,
    reservation: &ScrollReceiverReservation,
    rect: Rect,
    config: ScrollReceiverConfig,
) -> Result<ScrollReceiver, ScrollReceiverFinalizeError> {
    if !reservation.scope_matches(ui) {
        return Err(ScrollReceiverFinalizeError::ReservationScopeChanged);
    }
    let interact_rect = ui.clip_rect().intersect(rect);
    if !interact_rect.is_finite() || !config.is_valid() {
        return Err(ScrollReceiverFinalizeError::InvalidGeometry);
    }
    Ok(ScrollReceiver {
        id: reservation.id,
        layer_id: reservation.layer_id,
        interact_rect,
        enabled: ui.is_enabled(),
        config,
    })
}

pub(crate) fn normalize_scroll_delta(
    delta: Vec2,
    modifiers: Modifiers,
    options: crate::InputOptions,
) -> Option<Vec2> {
    if modifiers.matches_any(options.zoom_modifier) {
        return None;
    }
    let mut delta = delta;
    let horizontal = modifiers.matches_any(options.horizontal_scroll_modifier);
    let vertical = modifiers.matches_any(options.vertical_scroll_modifier);
    if horizontal && !vertical {
        delta = crate::vec2(delta.x + delta.y, 0.0);
    }
    if !horizontal && vertical {
        delta = crate::vec2(0.0, delta.x + delta.y);
    }
    Some(delta)
}

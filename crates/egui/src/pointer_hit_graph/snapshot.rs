use std::sync::Arc;

use ahash::HashMap;
use emath::TSTransform;

use crate::hit_test::WidgetHits;
use crate::{
    Id, InputOptions, LayerId, Modifiers, PointerHit, PointerReceiverAuthority,
    PointerReceiverUnavailableReason, Pos2, ScrollDeltaNormalization, ScrollProbe, ScrollReceiver,
    ViewportId, WidgetReceiver, WidgetRects, memory::Areas, scroll_receiver::ScrollReceiverRoster,
};

/// An immutable, completed-pass pointer hit graph.
///
/// This is a logical presentation candidate until its owning
/// [`crate::PointerHitGraphCandidate`] is settled by a successful renderer outcome.
#[derive(Clone)]
pub struct PointerHitGraphSnapshot {
    data: Arc<PointerHitGraphSnapshotData>,
}

struct PointerHitGraphSnapshotData {
    viewport_id: ViewportId,
    widget_pass_nr: u64,
    native_pixels_per_point: f32,
    pixels_per_point: f32,
    widgets: WidgetRects,
    interaction_layers: Vec<LayerId>,
    layer_to_global: HashMap<LayerId, TSTransform>,
    areas: Areas,
    top_modal_layer: Option<LayerId>,
    interact_radius: f32,
    focused_receiver: Option<WidgetReceiver>,
    scroll_receivers: Vec<ScrollReceiver>,
    input_options: InputOptions,
}

impl PointerHitGraphSnapshot {
    pub(crate) fn new(
        viewport_id: ViewportId,
        widget_pass_nr: u64,
        native_pixels_per_point: f32,
        pixels_per_point: f32,
        widgets: &WidgetRects,
        areas: &Areas,
        layer_to_global: &HashMap<LayerId, TSTransform>,
        top_modal_layer: Option<LayerId>,
        interact_radius: f32,
        focused_id: Option<Id>,
        scroll_receivers: &ScrollReceiverRoster,
        input_options: InputOptions,
    ) -> Self {
        let mut interaction_layers: Vec<_> = widgets.layer_ids().collect();
        interaction_layers.sort_by(|&a, &b| areas.compare_order(a, b));
        interaction_layers.retain(|&layer| {
            layer.order.allow_interaction()
                && top_modal_layer.is_none_or(|modal| {
                    !matches!(areas.compare_order(layer, modal), std::cmp::Ordering::Less)
                })
        });

        Self {
            data: Arc::new(PointerHitGraphSnapshotData {
                viewport_id,
                widget_pass_nr,
                native_pixels_per_point,
                pixels_per_point,
                widgets: widgets.clone(),
                interaction_layers,
                layer_to_global: layer_to_global.clone(),
                areas: areas.clone(),
                top_modal_layer,
                interact_radius,
                focused_receiver: focused_id
                    .and_then(|id| widgets.get(id))
                    .copied()
                    .map(WidgetReceiver::from),
                scroll_receivers: scroll_receivers.ready().collect(),
                input_options,
            }),
        }
    }

    /// Return the viewport that produced this hit graph.
    pub fn viewport_id(&self) -> ViewportId {
        self.data.viewport_id
    }

    /// Return the completed logical pass represented by this hit graph.
    pub fn widget_pass_nr(&self) -> u64 {
        self.data.widget_pass_nr
    }

    /// Return the operating-system pixel scale used by the rendered pass.
    ///
    /// This excludes egui's independent zoom factor. Native integrations use
    /// it to join a hit graph to an event-time window coordinate capture.
    pub fn native_pixels_per_point(&self) -> f32 {
        self.data.native_pixels_per_point
    }

    /// Return the physical-pixel scale used by the rendered widget pass.
    ///
    /// Native integrations must use this value, rather than a current window
    /// scale query, when translating an edge for this retained presentation.
    pub fn pixels_per_point(&self) -> f32 {
        self.data.pixels_per_point
    }

    /// Return the focused widget captured by this completed presented pass.
    ///
    /// `None` means the pass had no focused widget in this viewport's exact
    /// widget roster. Native integrations must not substitute current memory.
    pub fn focused_receiver(&self) -> Option<WidgetReceiver> {
        self.data.focused_receiver
    }

    /// Resolve an AccessKit node against this completed widget roster.
    ///
    /// Duplicate widget identifiers are ambiguous and therefore fail closed.
    /// The lookup never consults current [`crate::Context`] state.
    pub fn receiver_for_accesskit_node(&self, target: accesskit::NodeId) -> Option<WidgetReceiver> {
        let mut receiver = None;
        for widget in self
            .data
            .widgets
            .layers()
            .flat_map(|(_, widgets)| widgets)
            .filter(|widget| widget.id.accesskit_id() == target)
        {
            if receiver.is_some() {
                return None;
            }
            receiver = Some(WidgetReceiver::from(*widget));
        }
        receiver
    }

    /// Probe this frozen graph at one viewport-local logical position.
    ///
    /// This method is read-only and does not consult current [`crate::Context`] state.
    pub fn probe(&self, position: Pos2) -> PointerReceiverAuthority<PointerHit> {
        match self.probe_state(position) {
            Ok((hit, _)) => PointerReceiverAuthority::Known(hit),
            Err(reason) => PointerReceiverAuthority::Unknown(reason),
        }
    }

    /// Probe the frozen scroll roster with one exact wheel sample.
    ///
    /// Modifier-axis normalization and receiver projection happen here so native
    /// adapters cannot choose a receiver using a different event interpretation.
    pub fn probe_scroll(
        &self,
        position: Pos2,
        delta: crate::Vec2,
        modifiers: Modifiers,
    ) -> PointerReceiverAuthority<ScrollProbe> {
        if !position.x.is_finite() || !position.y.is_finite() {
            return PointerReceiverAuthority::Unknown(
                PointerReceiverUnavailableReason::InvalidPosition,
            );
        }
        let normalized_delta = match self.normalize_scroll_delta(delta, modifiers) {
            PointerReceiverAuthority::Known(ScrollDeltaNormalization::Scroll(delta)) => delta,
            PointerReceiverAuthority::Known(ScrollDeltaNormalization::AwaitingDelta) => {
                return PointerReceiverAuthority::Known(ScrollProbe::AwaitingDelta);
            }
            PointerReceiverAuthority::Known(ScrollDeltaNormalization::FrameworkOwned) => {
                return PointerReceiverAuthority::Known(ScrollProbe::FrameworkOwned);
            }
            PointerReceiverAuthority::Unknown(reason) => {
                return PointerReceiverAuthority::Unknown(reason);
            }
        };

        let blocking_layer = self.blocking_layer_at(position);
        let mut lower_receiver = false;
        for receiver in self.data.scroll_receivers.iter().rev() {
            if !receiver.enabled() || !self.receiver_contains(*receiver, position) {
                continue;
            }
            if blocking_layer.is_some_and(|layer| receiver.layer_id() != layer) {
                lower_receiver = true;
                continue;
            }
            if receiver.config().accepts(normalized_delta) {
                return PointerReceiverAuthority::Known(ScrollProbe::Receiver {
                    receiver: *receiver,
                    normalized_delta,
                });
            }
        }
        PointerReceiverAuthority::Known(if lower_receiver {
            ScrollProbe::Blocked
        } else {
            ScrollProbe::NoReceiver
        })
    }

    /// Normalize one wheel sample using the input options frozen with this pass.
    pub fn normalize_scroll_delta(
        &self,
        delta: crate::Vec2,
        modifiers: Modifiers,
    ) -> PointerReceiverAuthority<ScrollDeltaNormalization> {
        if !delta.x.is_finite() || !delta.y.is_finite() {
            return PointerReceiverAuthority::Unknown(
                PointerReceiverUnavailableReason::InvalidScrollDelta,
            );
        }
        let Some(delta) = crate::scroll_receiver::normalize_scroll_delta(
            delta,
            modifiers,
            self.data.input_options,
        ) else {
            return PointerReceiverAuthority::Known(ScrollDeltaNormalization::FrameworkOwned);
        };
        PointerReceiverAuthority::Known(if delta == crate::Vec2::ZERO {
            ScrollDeltaNormalization::AwaitingDelta
        } else {
            ScrollDeltaNormalization::Scroll(delta)
        })
    }

    pub(crate) fn probe_state(
        &self,
        position: Pos2,
    ) -> Result<(PointerHit, WidgetHits), PointerReceiverUnavailableReason> {
        if !position.x.is_finite() || !position.y.is_finite() {
            return Err(PointerReceiverUnavailableReason::InvalidPosition);
        }

        let hits = crate::hit_test::hit_test(
            &self.data.widgets,
            &self.data.interaction_layers,
            &self.data.layer_to_global,
            position,
            self.data.interact_radius,
        );

        let hit = PointerHit {
            widget_pass_nr: self.data.widget_pass_nr,
            blocking_layer: self.blocking_layer_at(position),
            top_widget: hits
                .contains_pointer
                .last()
                .copied()
                .map(WidgetReceiver::from),
            containing_receivers: hits
                .contains_pointer
                .iter()
                .copied()
                .map(WidgetReceiver::from)
                .collect(),
            click_receiver: hits.click.map(WidgetReceiver::from),
            drag_receiver: hits.drag.map(WidgetReceiver::from),
        };
        Ok((hit, hits))
    }

    fn blocking_layer_at(&self, position: Pos2) -> Option<LayerId> {
        let layer = self
            .data
            .areas
            .layer_id_at(position, &self.data.layer_to_global)?;
        let Some(modal) = self.data.top_modal_layer else {
            return Some(layer);
        };

        if matches!(
            self.data.areas.compare_order(layer, modal),
            std::cmp::Ordering::Less
        ) {
            Some(modal)
        } else {
            Some(layer)
        }
    }

    fn receiver_contains(&self, receiver: ScrollReceiver, position: Pos2) -> bool {
        let local = self
            .data
            .layer_to_global
            .get(&receiver.layer_id())
            .copied()
            .map_or(position, |transform| transform.inverse() * position);
        receiver.interact_rect().contains(local)
    }
}

impl std::fmt::Debug for PointerHitGraphSnapshot {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PointerHitGraphSnapshot")
            .field("viewport_id", &self.data.viewport_id)
            .field("widget_pass_nr", &self.data.widget_pass_nr)
            .field(
                "native_pixels_per_point",
                &self.data.native_pixels_per_point,
            )
            .field("pixels_per_point", &self.data.pixels_per_point)
            .field(
                "interaction_layer_count",
                &self.data.interaction_layers.len(),
            )
            .field("focused_receiver", &self.data.focused_receiver)
            .finish_non_exhaustive()
    }
}

impl PartialEq for PointerHitGraphSnapshot {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.data, &other.data)
    }
}

impl Eq for PointerHitGraphSnapshot {}

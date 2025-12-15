use emath::GuiRounding as _;
use epaint::CornerRadiusF32;

use crate::*;

use super::{Area, Frame, Resize, area, resize};

pub(crate) fn paint_resize_corner(
    ui: &Ui,
    possible: &PossibleInteractions,
    outer_rect: Rect,
    window_frame: &Frame,
    i: ResizeInteraction,
) {
    let cr = window_frame.corner_radius;

    let (corner, radius, corner_response) = if possible.resize_right && possible.resize_bottom {
        (Align2::RIGHT_BOTTOM, cr.se, i.right & i.bottom)
    } else if possible.resize_left && possible.resize_bottom {
        (Align2::LEFT_BOTTOM, cr.sw, i.left & i.bottom)
    } else if possible.resize_left && possible.resize_top {
        (Align2::LEFT_TOP, cr.nw, i.left & i.top)
    } else if possible.resize_right && possible.resize_top {
        (Align2::RIGHT_TOP, cr.ne, i.right & i.top)
    } else {
        // We're not in two directions, but it is still nice to tell the user
        // we're resizable by painting the resize corner in the expected place
        // (i.e. for windows only resizable in one direction):
        if possible.resize_right || possible.resize_bottom {
            (Align2::RIGHT_BOTTOM, cr.se, i.right & i.bottom)
        } else if possible.resize_left || possible.resize_bottom {
            (Align2::LEFT_BOTTOM, cr.sw, i.left & i.bottom)
        } else if possible.resize_left || possible.resize_top {
            (Align2::LEFT_TOP, cr.nw, i.left & i.top)
        } else if possible.resize_right || possible.resize_top {
            (Align2::RIGHT_TOP, cr.ne, i.right & i.top)
        } else {
            return;
        }
    };

    // Adjust the corner offset to accommodate for window rounding
    let radius = radius as f32;
    let offset =
        ((2.0_f32.sqrt() * (1.0 + radius) - radius) * 45.0_f32.to_radians().cos()).max(2.0);

    let stroke = if corner_response.drag {
        ui.visuals().widgets.active.fg_stroke
    } else if corner_response.hover {
        ui.visuals().widgets.hovered.fg_stroke
    } else {
        window_frame.stroke
    };

    let fill_rect = outer_rect.shrink(window_frame.stroke.width);
    let corner_size = Vec2::splat(ui.visuals().resize_corner_size);
    let corner_rect = corner.align_size_within_rect(corner_size, fill_rect);
    let corner_rect = corner_rect.translate(-offset * corner.to_sign()); // move away from corner
    crate::resize::paint_resize_corner_with_style(ui, &corner_rect, stroke.color, corner);
}

/// Which sides can be resized?
#[derive(Clone, Copy, Debug)]
pub(crate) struct PossibleInteractions {
    // Which sides can we drag to resize or move?
    pub(crate) resize_left: bool,
    pub(crate) resize_right: bool,
    pub(crate) resize_top: bool,
    pub(crate) resize_bottom: bool,
}

impl PossibleInteractions {
    pub(crate) fn new(area: &Area, resize: &Resize, is_collapsed: bool) -> Self {
        let movable = area.is_enabled() && area.is_movable();
        let resizable = resize
            .is_resizable()
            .and(area.is_enabled() && !is_collapsed);
        let pivot = area.get_pivot();
        Self {
            resize_left: resizable.x && (movable || pivot.x() != Align::LEFT),
            resize_right: resizable.x && (movable || pivot.x() != Align::RIGHT),
            resize_top: resizable.y && (movable || pivot.y() != Align::TOP),
            resize_bottom: resizable.y && (movable || pivot.y() != Align::BOTTOM),
        }
    }

    pub(crate) fn resizable(&self) -> bool {
        self.resize_left || self.resize_right || self.resize_top || self.resize_bottom
    }
}

/// Resizing the window edges.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ResizeInteraction {
    /// Outer rect (outside the stroke)
    pub(crate) outer_rect: Rect,

    pub(crate) window_frame: Frame,

    pub(crate) left: SideResponse,
    pub(crate) right: SideResponse,
    pub(crate) top: SideResponse,
    pub(crate) bottom: SideResponse,
}

/// A miniature version of `Response`, for each side of the window.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct SideResponse {
    pub(crate) hover: bool,
    pub(crate) drag: bool,
}

impl SideResponse {
    pub(crate) fn any(&self) -> bool {
        self.hover || self.drag
    }
}

impl std::ops::BitAnd for SideResponse {
    type Output = Self;

    fn bitand(self, rhs: Self) -> Self::Output {
        Self {
            hover: self.hover && rhs.hover,
            drag: self.drag && rhs.drag,
        }
    }
}

impl std::ops::BitOrAssign for SideResponse {
    fn bitor_assign(&mut self, rhs: Self) {
        *self = Self {
            hover: self.hover || rhs.hover,
            drag: self.drag || rhs.drag,
        };
    }
}

impl ResizeInteraction {
    pub(crate) fn set_cursor(&self, ctx: &Context) {
        let left = self.left.any();
        let right = self.right.any();
        let top = self.top.any();
        let bottom = self.bottom.any();

        // TODO(emilk): use one-sided cursors for when we reached the min/max size.
        if (left && top) || (right && bottom) {
            ctx.set_cursor_icon(CursorIcon::ResizeNwSe);
        } else if (right && top) || (left && bottom) {
            ctx.set_cursor_icon(CursorIcon::ResizeNeSw);
        } else if left || right {
            ctx.set_cursor_icon(CursorIcon::ResizeHorizontal);
        } else if bottom || top {
            ctx.set_cursor_icon(CursorIcon::ResizeVertical);
        }
    }

    pub(crate) fn any_hovered(&self) -> bool {
        self.left.hover || self.right.hover || self.top.hover || self.bottom.hover
    }

    pub(crate) fn any_dragged(&self) -> bool {
        self.left.drag || self.right.drag || self.top.drag || self.bottom.drag
    }
}

pub(crate) fn resize_response(
    resize_interaction: ResizeInteraction,
    ctx: &Context,
    margins: Vec2,
    area_layer_id: LayerId,
    area: &mut area::Prepared,
    resize_id: Id,
) {
    let Some(mut new_rect) = move_and_resize_window(ctx, resize_id, &resize_interaction) else {
        return;
    };

    if area.constrain() {
        new_rect = Context::constrain_window_rect_to_area(new_rect, area.constrain_rect());
    }

    // TODO(emilk): add this to a Window state instead as a command "move here next frame"
    area.state_mut().set_left_top_pos(new_rect.left_top());

    if resize_interaction.any_dragged()
        && let Some(mut state) = resize::State::load(ctx, resize_id)
    {
        state.requested_size = Some(new_rect.size() - margins);
        state.store(ctx, resize_id);
    }

    ctx.memory_mut(|mem| mem.areas_mut().move_to_top(area_layer_id));
}

/// Acts on outer rect (outside the stroke)
fn move_and_resize_window(ctx: &Context, id: Id, interaction: &ResizeInteraction) -> Option<Rect> {
    // Used to prevent drift
    let rect_at_start_of_drag_id = id.with("window_rect_at_drag_start");

    if !interaction.any_dragged() {
        ctx.data_mut(|data| {
            data.remove::<Rect>(rect_at_start_of_drag_id);
        });
        return None;
    }

    let total_drag_delta = ctx.input(|i| i.pointer.total_drag_delta())?;

    let rect_at_start_of_drag = ctx.data_mut(|data| {
        *data.get_temp_mut_or::<Rect>(rect_at_start_of_drag_id, interaction.outer_rect)
    });

    let mut rect = rect_at_start_of_drag; // prevent drift

    // Put the rect in the center of the stroke:
    rect = rect.shrink(interaction.window_frame.stroke.width / 2.0);

    if interaction.left.drag {
        rect.min.x += total_drag_delta.x;
    } else if interaction.right.drag {
        rect.max.x += total_drag_delta.x;
    }

    if interaction.top.drag {
        rect.min.y += total_drag_delta.y;
    } else if interaction.bottom.drag {
        rect.max.y += total_drag_delta.y;
    }

    // Return to having the rect outside the stroke:
    rect = rect.expand(interaction.window_frame.stroke.width / 2.0);

    Some(rect.round_ui())
}

pub(crate) fn resize_interaction(
    ctx: &Context,
    possible: PossibleInteractions,
    accessibility_parent: Id,
    layer_id: LayerId,
    outer_rect: Rect,
    window_frame: Frame,
) -> ResizeInteraction {
    if !possible.resizable() {
        return ResizeInteraction {
            outer_rect,
            window_frame,
            left: Default::default(),
            right: Default::default(),
            top: Default::default(),
            bottom: Default::default(),
        };
    }

    // The rect that is in the middle of the stroke:
    let rect = outer_rect.shrink(window_frame.stroke.width / 2.0);

    let side_response = |rect, id| {
        ctx.register_accesskit_parent(id, accessibility_parent);
        let response = ctx.create_widget(
            WidgetRect {
                layer_id,
                id,
                rect,
                interact_rect: rect,
                sense: Sense::drag(),
                enabled: true,
            },
            true,
        );
        SideResponse {
            hover: response.hovered(),
            drag: response.dragged(),
        }
    };

    let id = Id::new(layer_id).with("edge_drag");

    let side_grab_radius = ctx.global_style().interaction.resize_grab_radius_side;
    let corner_grab_radius = ctx.global_style().interaction.resize_grab_radius_corner;

    let vetrtical_rect = |a: Pos2, b: Pos2| {
        Rect::from_min_max(a, b).expand2(vec2(side_grab_radius, -corner_grab_radius))
    };
    let horizontal_rect = |a: Pos2, b: Pos2| {
        Rect::from_min_max(a, b).expand2(vec2(-corner_grab_radius, side_grab_radius))
    };
    let corner_rect =
        |center: Pos2| Rect::from_center_size(center, Vec2::splat(2.0 * corner_grab_radius));

    // What are we dragging/hovering?
    let [mut left, mut right, mut top, mut bottom] = [SideResponse::default(); 4];

    // ----------------------------------------
    // Check sides first, so that corners are on top, covering the sides (i.e. corners have priority)

    if possible.resize_right {
        let response = side_response(
            vetrtical_rect(rect.right_top(), rect.right_bottom()),
            id.with("right"),
        );
        right |= response;
    }
    if possible.resize_left {
        let response = side_response(
            vetrtical_rect(rect.left_top(), rect.left_bottom()),
            id.with("left"),
        );
        left |= response;
    }
    if possible.resize_bottom {
        let response = side_response(
            horizontal_rect(rect.left_bottom(), rect.right_bottom()),
            id.with("bottom"),
        );
        bottom |= response;
    }
    if possible.resize_top {
        let response = side_response(
            horizontal_rect(rect.left_top(), rect.right_top()),
            id.with("top"),
        );
        top |= response;
    }

    // ----------------------------------------
    // Now check corners.
    // We check any corner that has either side resizable,
    // because we shrink the side resize handled by the corner width.
    // Also, even if we can only change the width (or height) of a window,
    // we show one of the corners as a grab-handle, so it makes sense that
    // the whole corner is grabbable:

    if possible.resize_right || possible.resize_bottom {
        let response = side_response(corner_rect(rect.right_bottom()), id.with("right_bottom"));
        if possible.resize_right {
            right |= response;
        }
        if possible.resize_bottom {
            bottom |= response;
        }
    }

    if possible.resize_right || possible.resize_top {
        let response = side_response(corner_rect(rect.right_top()), id.with("right_top"));
        if possible.resize_right {
            right |= response;
        }
        if possible.resize_top {
            top |= response;
        }
    }

    if possible.resize_left || possible.resize_bottom {
        let response = side_response(corner_rect(rect.left_bottom()), id.with("left_bottom"));
        if possible.resize_left {
            left |= response;
        }
        if possible.resize_bottom {
            bottom |= response;
        }
    }

    if possible.resize_left || possible.resize_top {
        let response = side_response(corner_rect(rect.left_top()), id.with("left_top"));
        if possible.resize_left {
            left |= response;
        }
        if possible.resize_top {
            top |= response;
        }
    }

    let interaction = ResizeInteraction {
        outer_rect,
        window_frame,
        left,
        right,
        top,
        bottom,
    };
    interaction.set_cursor(ctx);
    interaction
}

/// Fill in parts of the window frame when we resize by dragging that part
pub(crate) fn paint_frame_interaction(ui: &Ui, rect: Rect, interaction: ResizeInteraction) {
    use epaint::tessellator::path::add_circle_quadrant;

    let visuals = if interaction.any_dragged() {
        ui.style().visuals.widgets.active
    } else if interaction.any_hovered() {
        ui.style().visuals.widgets.hovered
    } else {
        return;
    };

    let [left, right, top, bottom]: [bool; 4];

    if interaction.any_dragged() {
        left = interaction.left.drag;
        right = interaction.right.drag;
        top = interaction.top.drag;
        bottom = interaction.bottom.drag;
    } else {
        left = interaction.left.hover;
        right = interaction.right.hover;
        top = interaction.top.hover;
        bottom = interaction.bottom.hover;
    }

    let cr = CornerRadiusF32::from(ui.visuals().window_corner_radius);

    // Put the rect in the center of the fixed window stroke:
    let rect = rect.shrink(interaction.window_frame.stroke.width / 2.0);

    // Make sure the inner part of the stroke is at a pixel boundary:
    let stroke = visuals.bg_stroke;
    let half_stroke = stroke.width / 2.0;
    let rect = rect
        .shrink(half_stroke)
        .round_to_pixels(ui.pixels_per_point())
        .expand(half_stroke);

    let Rect { min, max } = rect;

    let mut points = Vec::new();

    if right && !bottom && !top {
        points.push(pos2(max.x, min.y + cr.ne));
        points.push(pos2(max.x, max.y - cr.se));
    }
    if right && bottom {
        points.push(pos2(max.x, min.y + cr.ne));
        points.push(pos2(max.x, max.y - cr.se));
        add_circle_quadrant(&mut points, pos2(max.x - cr.se, max.y - cr.se), cr.se, 0.0);
    }
    if bottom {
        points.push(pos2(max.x - cr.se, max.y));
        points.push(pos2(min.x + cr.sw, max.y));
    }
    if left && bottom {
        add_circle_quadrant(&mut points, pos2(min.x + cr.sw, max.y - cr.sw), cr.sw, 1.0);
    }
    if left {
        points.push(pos2(min.x, max.y - cr.sw));
        points.push(pos2(min.x, min.y + cr.nw));
    }
    if left && top {
        add_circle_quadrant(&mut points, pos2(min.x + cr.nw, min.y + cr.nw), cr.nw, 2.0);
    }
    if top {
        points.push(pos2(min.x + cr.nw, min.y));
        points.push(pos2(max.x - cr.ne, min.y));
    }
    if right && top {
        add_circle_quadrant(&mut points, pos2(max.x - cr.ne, min.y + cr.ne), cr.ne, 3.0);
        points.push(pos2(max.x, min.y + cr.ne));
        points.push(pos2(max.x, max.y - cr.se));
    }

    ui.painter().add(Shape::line(points, stroke));
}

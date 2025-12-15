use std::sync::Arc;

use emath::GuiRounding as _;
use epaint::RectShape;

use crate::collapsing_header::CollapsingState;
use crate::layers::ShapeIdx;
use crate::*;

/// Metrics required to lay out a window title bar, matching `egui::Window`.
///
/// This API is experimental in this fork.
#[derive(Clone, Copy, Debug)]
pub struct TitleBarMetrics {
    pub height_with_margin: f32,
    pub content_spacing: f32,
}

/// Compute title bar height and content spacing for a window, matching `egui::Window`.
///
/// This mutates `window_frame.corner_radius` to ensure it fits within the title bar height,
/// same as `egui::Window`.
///
/// This API is experimental in this fork.
pub fn title_bar_metrics(
    ctx: &Context,
    title: &WidgetText,
    window_frame: &mut Frame,
    with_title_bar: bool,
    is_collapsed: bool,
) -> TitleBarMetrics {
    if !with_title_bar {
        return TitleBarMetrics {
            height_with_margin: 0.0,
            content_spacing: 0.0,
        };
    }

    // Calculate roughly how much larger the full window inner size is compared to the content rect
    let style = ctx.global_style();
    let title_bar_inner_height = ctx
        .fonts_mut(|fonts| title.font_height(fonts, &style))
        .at_least(style.spacing.interact_size.y);
    let title_bar_inner_height = title_bar_inner_height + window_frame.inner_margin.sum().y;
    let half_height = (title_bar_inner_height / 2.0).round() as _;
    window_frame.corner_radius.ne = window_frame.corner_radius.ne.clamp(0, half_height);
    window_frame.corner_radius.nw = window_frame.corner_radius.nw.clamp(0, half_height);

    let title_content_spacing = if is_collapsed {
        0.0
    } else {
        window_frame.stroke.width
    };

    TitleBarMetrics {
        height_with_margin: title_bar_inner_height,
        content_spacing: title_content_spacing,
    }
}

/// Paint the highlighted title-bar background for the top-most window, matching `egui::Window`.
///
/// This API is experimental in this fork.
pub fn paint_title_bar_background(
    ui: &Ui,
    background: ShapeIdx,
    title_bar_rect: Rect,
    window_frame: &Frame,
    header_color: Color32,
    is_collapsed: bool,
    on_top: bool,
) {
    if !(on_top && ui.visuals().window_highlight_topmost) {
        return;
    }

    let mut round = window_frame.corner_radius - window_frame.stroke.width.round() as u8;

    if !is_collapsed {
        round.se = 0;
        round.sw = 0;
    }

    ui.painter().set(
        background,
        RectShape::filled(title_bar_rect, round, header_color),
    );
}

/// Paint the standard "close window" button (an `X`), matching `egui::Window`.
///
/// This API is experimental in this fork.
pub fn window_close_button(ui: &mut Ui, rect: Rect) -> Response {
    close_button(ui, rect)
}

/// Standard title-bar button rects matching `egui::Window`.
///
/// This API is experimental in this fork.
#[derive(Clone, Copy, Debug)]
pub struct TitleBarButtonRects {
    pub collapse: Rect,
    pub close: Rect,
}

/// Compute the standard title-bar button rects matching `egui::Window`.
///
/// This API is experimental in this fork.
pub fn title_bar_button_rects(ui: &Ui, title_bar_rect: Rect) -> TitleBarButtonRects {
    let button_size = Vec2::splat(ui.spacing().icon_width);

    let collapse_button_center = Align2::LEFT_CENTER
        .align_size_within_rect(Vec2::splat(title_bar_rect.height()), title_bar_rect)
        .center();
    let collapse = Rect::from_center_size(collapse_button_center, button_size)
        .round_to_pixels(ui.pixels_per_point());

    let close_button_center = Align2::RIGHT_CENTER
        .align_size_within_rect(Vec2::splat(title_bar_rect.height()), title_bar_rect)
        .center();
    let close = Rect::from_center_size(close_button_center, button_size)
        .round_to_pixels(ui.pixels_per_point());

    TitleBarButtonRects { collapse, close }
}

pub(crate) struct TitleBar {
    window_frame: Frame,

    /// Prepared text in the title
    title_galley: Arc<Galley>,

    /// Size of the title bar in an expanded state. This size become known only
    /// after expanding window and painting its content.
    ///
    /// Does not include the stroke, nor the separator line between the title bar and the window contents.
    inner_rect: Rect,
}

impl TitleBar {
    pub(crate) fn inner_rect(&self) -> Rect {
        self.inner_rect
    }

    pub(crate) fn set_outer_rect(&mut self, outer_rect: Rect, title_bar_height_with_margin: f32) {
        self.inner_rect = outer_rect.shrink(self.window_frame.stroke.width);
        self.inner_rect.max.y = self.inner_rect.min.y + title_bar_height_with_margin;
    }

    pub(crate) fn new(
        ui: &Ui,
        title: WidgetText,
        show_close_button: bool,
        collapsible: bool,
        window_frame: Frame,
        title_bar_height_with_margin: f32,
    ) -> Self {
        if false {
            ui.debug_painter()
                .debug_rect(ui.min_rect(), Color32::GREEN, "outer_min_rect");
        }

        let inner_height = title_bar_height_with_margin - window_frame.inner_margin.sum().y;

        let item_spacing = ui.spacing().item_spacing;
        let button_size = Vec2::splat(ui.spacing().icon_width.at_most(inner_height));

        let left_pad = ((inner_height - button_size.y) / 2.0).round_ui(); // calculated so that the icon is on the diagonal (if window padding is symmetrical)

        let title_galley = title.into_galley(
            ui,
            Some(crate::TextWrapMode::Extend),
            f32::INFINITY,
            TextStyle::Heading,
        );

        let minimum_width = if collapsible || show_close_button {
            // If at least one button is shown we make room for both buttons (since title should be centered):
            2.0 * (left_pad + button_size.x + item_spacing.x) + title_galley.size().x
        } else {
            left_pad + title_galley.size().x + left_pad
        };
        let min_inner_size = vec2(minimum_width, inner_height);
        let min_rect = Rect::from_min_size(ui.min_rect().min, min_inner_size);

        if false {
            ui.debug_painter()
                .debug_rect(min_rect, Color32::LIGHT_BLUE, "min_rect");
        }

        Self {
            window_frame,
            title_galley,
            inner_rect: min_rect, // First estimate - will be refined later
        }
    }

    /// Finishes painting of the title bar when the window content size already known.
    ///
    /// # Parameters
    ///
    /// - `ui`:
    /// - `outer_rect`:
    /// - `content_response`: if `None`, window is collapsed at this frame, otherwise contains
    ///   a result of rendering the window content
    /// - `open`: if `None`, no "Close" button will be rendered, otherwise renders and processes
    ///   the "Close" button and writes a `false` if window was closed
    /// - `collapsing`: holds the current expanding state. Can be changed by double click on the
    ///   title if `collapsible` is `true`
    /// - `collapsible`: if `true`, double click on the title bar will be handled for a change
    ///   of `collapsing` state
    pub(crate) fn ui(
        self,
        ui: &mut Ui,
        content_response: &Option<Response>,
        open: Option<&mut bool>,
        collapsing: &mut CollapsingState,
        collapsible: bool,
    ) {
        let window_frame = self.window_frame;
        let title_inner_rect = self.inner_rect;

        if false {
            ui.debug_painter()
                .debug_rect(self.inner_rect, Color32::RED, "TitleBar");
        }

        if collapsible {
            // Show collapse-button:
            let button_center = Align2::LEFT_CENTER
                .align_size_within_rect(Vec2::splat(self.inner_rect.height()), self.inner_rect)
                .center();
            let button_size = Vec2::splat(ui.spacing().icon_width);
            let button_rect = Rect::from_center_size(button_center, button_size);
            let button_rect = button_rect.round_ui();

            ui.scope_builder(UiBuilder::new().max_rect(button_rect), |ui| {
                collapsing.show_default_button_with_size(ui, button_size);
            });
        }

        if let Some(open) = open {
            // Add close button now that we know our full width:
            if self.close_button_ui(ui).clicked() {
                *open = false;
            }
        }

        let text_pos =
            emath::align::center_size_in_rect(self.title_galley.size(), title_inner_rect)
                .left_top();
        let text_pos = text_pos - self.title_galley.rect.min.to_vec2();
        ui.painter().galley(
            text_pos,
            self.title_galley.clone(),
            ui.visuals().text_color(),
        );

        if let Some(content_response) = &content_response {
            // Paint separator between title and content:
            let content_rect = content_response.rect;
            if false {
                ui.debug_painter()
                    .debug_rect(content_rect, Color32::RED, "content_rect");
            }
            let y = title_inner_rect.bottom() + window_frame.stroke.width / 2.0;

            // To verify the sanity of this, use a very wide window stroke
            ui.painter()
                .hline(title_inner_rect.x_range(), y, window_frame.stroke);
        }

        // Don't cover the close- and collapse buttons:
        let double_click_rect = title_inner_rect.shrink2(vec2(32.0, 0.0));

        if false {
            ui.debug_painter()
                .debug_rect(double_click_rect, Color32::GREEN, "double_click_rect");
        }

        let id = ui.unique_id().with("__window_title_bar");

        if ui
            .interact(double_click_rect, id, Sense::click())
            .double_clicked()
            && collapsible
        {
            collapsing.toggle(ui);
        }
    }

    /// Paints the "Close" button at the right side of the title bar
    /// and processes clicks on it.
    ///
    /// The button is square and its size is determined by the
    /// [`crate::style::Spacing::icon_width`] setting.
    fn close_button_ui(&self, ui: &mut Ui) -> Response {
        let button_center = Align2::RIGHT_CENTER
            .align_size_within_rect(Vec2::splat(self.inner_rect.height()), self.inner_rect)
            .center();
        let button_size = Vec2::splat(ui.spacing().icon_width);
        let button_rect = Rect::from_center_size(button_center, button_size);
        let button_rect = button_rect.round_to_pixels(ui.pixels_per_point());
        window_close_button(ui, button_rect)
    }
}

/// Paints the "Close" button of the window and processes clicks on it.
///
/// The close button is just an `X` symbol painted by a current stroke
/// for foreground elements (such as a label text).
///
/// # Parameters
/// - `ui`:
/// - `rect`: The rectangular area to fit the button in
///
/// Returns the result of a click on a button if it was pressed
fn close_button(ui: &mut Ui, rect: Rect) -> Response {
    let close_id = ui.auto_id_with("window_close_button");
    let response = ui.interact(rect, close_id, Sense::click());
    response
        .widget_info(|| WidgetInfo::labeled(WidgetType::Button, ui.is_enabled(), "Close window"));

    ui.expand_to_include_rect(response.rect);

    let visuals = ui.style().interact(&response);
    let rect = rect.shrink(2.0).expand(visuals.expansion);
    let stroke = visuals.fg_stroke;
    ui.painter() // paints \
        .line_segment([rect.left_top(), rect.right_bottom()], stroke);
    ui.painter() // paints /
        .line_segment([rect.right_top(), rect.left_bottom()], stroke);
    response
}

// WARNING: the code in here is horrible. It is a behemoth that needs breaking up into simpler parts.

use crate::collapsing_header::CollapsingState;
use crate::*;

use super::scroll_area::{ScrollBarVisibility, ScrollSource};
use super::window_chrome::{TitleBar, paint_title_bar_background, title_bar_metrics};
use super::window_interaction::{
    PossibleInteractions, paint_frame_interaction, paint_resize_corner, resize_interaction,
    resize_response,
};
use super::{Area, Frame, Resize, ScrollArea};

/// Builder for a floating window which can be dragged, closed, collapsed, resized and scrolled (off by default).
///
/// You can customize:
/// * title
/// * default, minimum, maximum and/or fixed size, collapsed/expanded
/// * if the window has a scroll area (off by default)
/// * if the window can be collapsed (minimized) to just the title bar (yes, by default)
/// * if there should be a close button (none by default)
///
/// ```
/// # egui::__run_test_ctx(|ctx| {
/// egui::Window::new("My Window").show(ctx, |ui| {
///    ui.label("Hello World!");
/// });
/// # });
/// ```
///
/// The previous rectangle used by this window can be obtained through [`crate::Memory::area_rect()`].
///
/// Note that this is NOT a native OS window.
/// To create a new native OS window, use [`crate::Context::show_viewport_deferred`].
#[must_use = "You should call .show()"]
pub struct Window<'open> {
    title: WidgetText,
    open: Option<&'open mut bool>,
    area: Area,
    frame: Option<Frame>,
    resize: Resize,
    scroll: ScrollArea,
    collapsible: bool,
    default_open: bool,
    with_title_bar: bool,
    fade_out: bool,
}

impl<'open> Window<'open> {
    /// The window title is used as a unique [`Id`] and must be unique, and should not change.
    /// This is true even if you disable the title bar with `.title_bar(false)`.
    /// If you need a changing title, you must call `window.id(…)` with a fixed id.
    pub fn new(title: impl Into<WidgetText>) -> Self {
        let title = title.into().fallback_text_style(TextStyle::Heading);
        let area = Area::new(Id::new(title.text())).kind(UiKind::Window);
        Self {
            title,
            open: None,
            area,
            frame: None,
            resize: Resize::default()
                .with_stroke(false)
                .min_size([96.0, 32.0])
                .default_size([340.0, 420.0]), // Default inner size of a window
            scroll: ScrollArea::neither().auto_shrink(false),
            collapsible: true,
            default_open: true,
            with_title_bar: true,
            fade_out: true,
        }
    }

    /// Assign a unique id to the Window. Required if the title changes, or is shared with another window.
    #[inline]
    pub fn id(mut self, id: Id) -> Self {
        self.area = self.area.id(id);
        self
    }

    /// Call this to add a close-button to the window title bar.
    ///
    /// * If `*open == false`, the window will not be visible.
    /// * If `*open == true`, the window will have a close button.
    /// * If the close button is pressed, `*open` will be set to `false`.
    #[inline]
    pub fn open(mut self, open: &'open mut bool) -> Self {
        self.open = Some(open);
        self
    }

    /// If `false` the window will be grayed out and non-interactive.
    #[inline]
    pub fn enabled(mut self, enabled: bool) -> Self {
        self.area = self.area.enabled(enabled);
        self
    }

    /// If false, clicks goes straight through to what is behind us.
    ///
    /// Can be used for semi-invisible areas that the user should be able to click through.
    ///
    /// Default: `true`.
    #[inline]
    pub fn interactable(mut self, interactable: bool) -> Self {
        self.area = self.area.interactable(interactable);
        self
    }

    /// If `false` the window will be immovable.
    #[inline]
    pub fn movable(mut self, movable: bool) -> Self {
        self.area = self.area.movable(movable);
        self
    }

    /// `order(Order::Foreground)` for a Window that should always be on top
    #[inline]
    pub fn order(mut self, order: Order) -> Self {
        self.area = self.area.order(order);
        self
    }

    /// If `true`, quickly fade in the `Window` when it first appears.
    ///
    /// Default: `true`.
    #[inline]
    pub fn fade_in(mut self, fade_in: bool) -> Self {
        self.area = self.area.fade_in(fade_in);
        self
    }

    /// If `true`, quickly fade out the `Window` when it closes.
    ///
    /// This only works if you use [`Self::open`] to close the window.
    ///
    /// Default: `true`.
    #[inline]
    pub fn fade_out(mut self, fade_out: bool) -> Self {
        self.fade_out = fade_out;
        self
    }

    /// Usage: `Window::new(…).mutate(|w| w.resize = w.resize.auto_expand_width(true))`
    // TODO(emilk): I'm not sure this is a good interface for this.
    #[inline]
    pub fn mutate(mut self, mutate: impl Fn(&mut Self)) -> Self {
        mutate(&mut self);
        self
    }

    /// Usage: `Window::new(…).resize(|r| r.auto_expand_width(true))`
    // TODO(emilk): I'm not sure this is a good interface for this.
    #[inline]
    pub fn resize(mut self, mutate: impl Fn(Resize) -> Resize) -> Self {
        self.resize = mutate(self.resize);
        self
    }

    /// Change the background color, margins, etc.
    #[inline]
    pub fn frame(mut self, frame: Frame) -> Self {
        self.frame = Some(frame);
        self
    }

    /// Set minimum width of the window.
    #[inline]
    pub fn min_width(mut self, min_width: f32) -> Self {
        self.resize = self.resize.min_width(min_width);
        self
    }

    /// Set minimum height of the window.
    #[inline]
    pub fn min_height(mut self, min_height: f32) -> Self {
        self.resize = self.resize.min_height(min_height);
        self
    }

    /// Set minimum size of the window, equivalent to calling both `min_width` and `min_height`.
    #[inline]
    pub fn min_size(mut self, min_size: impl Into<Vec2>) -> Self {
        self.resize = self.resize.min_size(min_size);
        self
    }

    /// Set maximum width of the window.
    #[inline]
    pub fn max_width(mut self, max_width: f32) -> Self {
        self.resize = self.resize.max_width(max_width);
        self
    }

    /// Set maximum height of the window.
    #[inline]
    pub fn max_height(mut self, max_height: f32) -> Self {
        self.resize = self.resize.max_height(max_height);
        self
    }

    /// Set maximum size of the window, equivalent to calling both `max_width` and `max_height`.
    #[inline]
    pub fn max_size(mut self, max_size: impl Into<Vec2>) -> Self {
        self.resize = self.resize.max_size(max_size);
        self
    }

    /// Set current position of the window.
    /// If the window is movable it is up to you to keep track of where it moved to!
    #[inline]
    pub fn current_pos(mut self, current_pos: impl Into<Pos2>) -> Self {
        self.area = self.area.current_pos(current_pos);
        self
    }

    /// Set initial position of the window.
    #[inline]
    pub fn default_pos(mut self, default_pos: impl Into<Pos2>) -> Self {
        self.area = self.area.default_pos(default_pos);
        self
    }

    /// Sets the window position and prevents it from being dragged around.
    #[inline]
    pub fn fixed_pos(mut self, pos: impl Into<Pos2>) -> Self {
        self.area = self.area.fixed_pos(pos);
        self
    }

    /// Constrains this window to [`Context::screen_rect`].
    ///
    /// To change the area to constrain to, use [`Self::constrain_to`].
    ///
    /// Default: `true`.
    #[inline]
    pub fn constrain(mut self, constrain: bool) -> Self {
        self.area = self.area.constrain(constrain);
        self
    }

    /// Constrain the movement of the window to the given rectangle.
    ///
    /// For instance: `.constrain_to(ctx.screen_rect())`.
    #[inline]
    pub fn constrain_to(mut self, constrain_rect: Rect) -> Self {
        self.area = self.area.constrain_to(constrain_rect);
        self
    }

    /// Where the "root" of the window is.
    ///
    /// For instance, if you set this to [`Align2::RIGHT_TOP`]
    /// then [`Self::fixed_pos`] will set the position of the right-top
    /// corner of the window.
    ///
    /// Default: [`Align2::LEFT_TOP`].
    #[inline]
    pub fn pivot(mut self, pivot: Align2) -> Self {
        self.area = self.area.pivot(pivot);
        self
    }

    /// Set anchor and distance.
    ///
    /// An anchor of `Align2::RIGHT_TOP` means "put the right-top corner of the window
    /// in the right-top corner of the screen".
    ///
    /// The offset is added to the position, so e.g. an offset of `[-5.0, 5.0]`
    /// would move the window left and down from the given anchor.
    ///
    /// Anchoring also makes the window immovable.
    ///
    /// It is an error to set both an anchor and a position.
    #[inline]
    pub fn anchor(mut self, align: Align2, offset: impl Into<Vec2>) -> Self {
        self.area = self.area.anchor(align, offset);
        self
    }

    /// Set initial collapsed state of the window
    #[inline]
    pub fn default_open(mut self, default_open: bool) -> Self {
        self.default_open = default_open;
        self
    }

    /// Set initial size of the window.
    #[inline]
    pub fn default_size(mut self, default_size: impl Into<Vec2>) -> Self {
        let default_size: Vec2 = default_size.into();
        self.resize = self.resize.default_size(default_size);
        self.area = self.area.default_size(default_size);
        self
    }

    /// Set initial width of the window.
    #[inline]
    pub fn default_width(mut self, default_width: f32) -> Self {
        self.resize = self.resize.default_width(default_width);
        self.area = self.area.default_width(default_width);
        self
    }

    /// Set initial height of the window.
    #[inline]
    pub fn default_height(mut self, default_height: f32) -> Self {
        self.resize = self.resize.default_height(default_height);
        self.area = self.area.default_height(default_height);
        self
    }

    /// Sets the window size and prevents it from being resized by dragging its edges.
    #[inline]
    pub fn fixed_size(mut self, size: impl Into<Vec2>) -> Self {
        self.resize = self.resize.fixed_size(size);
        self
    }

    /// Set initial position and size of the window.
    pub fn default_rect(self, rect: Rect) -> Self {
        self.default_pos(rect.min).default_size(rect.size())
    }

    /// Sets the window pos and size and prevents it from being moved and resized by dragging its edges.
    pub fn fixed_rect(self, rect: Rect) -> Self {
        self.fixed_pos(rect.min).fixed_size(rect.size())
    }

    /// Can the user resize the window by dragging its edges?
    ///
    /// Note that even if you set this to `false` the window may still auto-resize.
    ///
    /// You can set the window to only be resizable in one direction by using
    /// e.g. `[true, false]` as the argument,
    /// making the window only resizable in the x-direction.
    ///
    /// Default is `true`.
    #[inline]
    pub fn resizable(mut self, resizable: impl Into<Vec2b>) -> Self {
        let resizable = resizable.into();
        self.resize = self.resize.resizable(resizable);
        self
    }

    /// Can the window be collapsed by clicking on its title?
    #[inline]
    pub fn collapsible(mut self, collapsible: bool) -> Self {
        self.collapsible = collapsible;
        self
    }

    /// Show title bar on top of the window?
    /// If `false`, the window will not be collapsible nor have a close-button.
    #[inline]
    pub fn title_bar(mut self, title_bar: bool) -> Self {
        self.with_title_bar = title_bar;
        self
    }

    /// Not resizable, just takes the size of its contents.
    /// Also disabled scrolling.
    /// Text will not wrap, but will instead make your window width expand.
    #[inline]
    pub fn auto_sized(mut self) -> Self {
        self.resize = self.resize.auto_sized();
        self.scroll = ScrollArea::neither();
        self
    }

    /// Enable/disable horizontal/vertical scrolling. `false` by default.
    ///
    /// You can pass in `false`, `true`, `[false, true]` etc.
    #[inline]
    pub fn scroll(mut self, scroll: impl Into<Vec2b>) -> Self {
        self.scroll = self.scroll.scroll(scroll);
        self
    }

    /// Enable/disable horizontal scrolling. `false` by default.
    #[inline]
    pub fn hscroll(mut self, hscroll: bool) -> Self {
        self.scroll = self.scroll.hscroll(hscroll);
        self
    }

    /// Enable/disable vertical scrolling. `false` by default.
    #[inline]
    pub fn vscroll(mut self, vscroll: bool) -> Self {
        self.scroll = self.scroll.vscroll(vscroll);
        self
    }

    /// Enable/disable scrolling on the window by dragging with the pointer. `true` by default.
    ///
    /// See [`ScrollArea::drag_to_scroll`] for more.
    #[inline]
    pub fn drag_to_scroll(mut self, drag_to_scroll: bool) -> Self {
        self.scroll = self.scroll.scroll_source(ScrollSource {
            drag: drag_to_scroll,
            ..Default::default()
        });
        self
    }

    /// Sets the [`ScrollBarVisibility`] of the window.
    #[inline]
    pub fn scroll_bar_visibility(mut self, visibility: ScrollBarVisibility) -> Self {
        self.scroll = self.scroll.scroll_bar_visibility(visibility);
        self
    }
}

impl Window<'_> {
    /// Returns `None` if the window is not open (if [`Window::open`] was called with `&mut false`).
    /// Returns `Some(InnerResponse { inner: None })` if the window is collapsed.
    #[inline]
    pub fn show<R>(
        self,
        ctx: &Context,
        add_contents: impl FnOnce(&mut Ui) -> R,
    ) -> Option<InnerResponse<Option<R>>> {
        self.show_dyn(ctx, Box::new(add_contents))
    }

    fn show_dyn<'c, R>(
        self,
        ctx: &Context,
        add_contents: Box<dyn FnOnce(&mut Ui) -> R + 'c>,
    ) -> Option<InnerResponse<Option<R>>> {
        let Window {
            title,
            mut open,
            area,
            frame,
            resize,
            scroll,
            collapsible,
            default_open,
            with_title_bar,
            fade_out,
        } = self;

        let header_color = frame.map_or_else(
            || ctx.global_style().visuals.widgets.open.weak_bg_fill,
            |f| f.fill,
        );
        let mut window_frame = frame.unwrap_or_else(|| Frame::window(&ctx.global_style()));

        let is_explicitly_closed = matches!(open, Some(false));
        let is_open = !is_explicitly_closed || ctx.memory(|mem| mem.everything_is_visible());
        let opacity = ctx.animate_bool_with_easing(
            area.id.with("fade-out"),
            is_open,
            emath::easing::cubic_out,
        );
        if opacity <= 0.0 {
            return None;
        }

        let area_id = area.id;
        let area_layer_id = area.layer();
        let resize_id = area_id.with("resize");
        let mut collapsing =
            CollapsingState::load_with_default_open(ctx, area_id.with("collapsing"), default_open);

        let is_collapsed = with_title_bar && !collapsing.is_open();
        let possible = PossibleInteractions::new(&area, &resize, is_collapsed);

        let resize = resize.resizable(false); // We resize it manually
        let mut resize = resize.id(resize_id);

        let on_top = Some(area_layer_id) == ctx.top_layer_id();
        let mut area = area.begin(ctx);

        area.with_widget_info(|| WidgetInfo::labeled(WidgetType::Window, true, title.text()));

        let title_bar_metrics =
            title_bar_metrics(ctx, &title, &mut window_frame, with_title_bar, is_collapsed);
        let title_bar_height_with_margin = title_bar_metrics.height_with_margin;
        let title_content_spacing = title_bar_metrics.content_spacing;

        {
            // Prevent window from becoming larger than the constrain rect.
            let constrain_rect = area.constrain_rect();
            let max_width = constrain_rect.width();
            let max_height =
                constrain_rect.height() - title_bar_height_with_margin - title_content_spacing;
            resize.max_size.x = resize.max_size.x.min(max_width);
            resize.max_size.y = resize.max_size.y.min(max_height);
        }

        // First check for resize to avoid frame delay:
        let last_frame_outer_rect = area.state().rect();
        let resize_interaction = resize_interaction(
            ctx,
            possible,
            area.id(),
            area_layer_id,
            last_frame_outer_rect,
            window_frame,
        );

        {
            let margins = window_frame.total_margin().sum()
                + vec2(0.0, title_bar_height_with_margin + title_content_spacing);

            resize_response(
                resize_interaction,
                ctx,
                margins,
                area_layer_id,
                &mut area,
                resize_id,
            );
        }

        let mut area_content_ui = area.content_ui(ctx);
        if is_open {
            // `Area` already takes care of fade-in animations,
            // so we only need to handle fade-out animations here.
        } else if fade_out {
            area_content_ui.multiply_opacity(opacity);
        }

        let content_inner = {
            // BEGIN FRAME --------------------------------
            let mut frame = window_frame.begin(&mut area_content_ui);

            let show_close_button = open.is_some();

            let where_to_put_header_background = &area_content_ui.painter().add(Shape::Noop);

            let title_bar = if with_title_bar {
                let title_bar = TitleBar::new(
                    &frame.content_ui,
                    title,
                    show_close_button,
                    collapsible,
                    window_frame,
                    title_bar_height_with_margin,
                );
                resize.min_size.x = resize.min_size.x.at_least(title_bar.inner_rect().width()); // Prevent making window smaller than title bar width

                frame.content_ui.set_min_size(title_bar.inner_rect().size());

                // Skip the title bar (and separator):
                if is_collapsed {
                    frame.content_ui.add_space(title_bar.inner_rect().height());
                } else {
                    frame.content_ui.add_space(
                        title_bar.inner_rect().height()
                            + title_content_spacing
                            + window_frame.inner_margin.sum().y,
                    );
                }

                Some(title_bar)
            } else {
                None
            };

            let (content_inner, content_response) = collapsing
                .show_body_unindented(&mut frame.content_ui, |ui| {
                    resize.show(ui, |ui| {
                        if scroll.is_any_scroll_enabled() {
                            scroll.show(ui, add_contents).inner
                        } else {
                            add_contents(ui)
                        }
                    })
                })
                .map_or((None, None), |ir| (Some(ir.inner), Some(ir.response)));

            let outer_rect = frame.end(&mut area_content_ui).rect;
            paint_resize_corner(
                &area_content_ui,
                &possible,
                outer_rect,
                &window_frame,
                resize_interaction,
            );

            // END FRAME --------------------------------

            if let Some(mut title_bar) = title_bar {
                title_bar.set_outer_rect(outer_rect, title_bar_height_with_margin);

                paint_title_bar_background(
                    &area_content_ui,
                    *where_to_put_header_background,
                    title_bar.inner_rect(),
                    &window_frame,
                    header_color,
                    is_collapsed,
                    on_top,
                );

                if false {
                    ctx.debug_painter().debug_rect(
                        title_bar.inner_rect(),
                        Color32::LIGHT_BLUE,
                        "title_bar.rect",
                    );
                }

                title_bar.ui(
                    &mut area_content_ui,
                    &content_response,
                    open.as_deref_mut(),
                    &mut collapsing,
                    collapsible,
                );
            }

            collapsing.store(ctx);

            paint_frame_interaction(&area_content_ui, outer_rect, resize_interaction);

            content_inner
        };

        let full_response = area.end(ctx, area_content_ui);

        if full_response.should_close()
            && let Some(open) = open
        {
            *open = false;
        }

        let inner_response = InnerResponse {
            inner: content_inner,
            response: full_response,
        };
        Some(inner_response)
    }
}
// Window resize/move interaction helpers were extracted into `window_interaction.rs`.

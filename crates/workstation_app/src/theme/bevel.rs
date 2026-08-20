//! The bevel primitives: the part of the Win95 grammar egui's uniform-stroke
//! widgets cannot express, offered as functions so other modules compose the
//! language instead of copying colours.
//!
//! The grammar is the four-edge 3D border of "The Windows Interface
//! Guidelines for Software Design", Microsoft Press, 1995, ch. 13, as
//! implemented by the Win32 `DrawEdge` API: light falls from the top-left, a
//! raised block is lit on its top/left edges and shaded on its bottom/right,
//! a sunken well is the exact inversion, and an etched line is a shade line
//! with a lit line one pixel below-right. The modern execution here: every
//! bevel line is exactly one *physical* pixel at any DPI — snapped to the
//! device pixel grid via [`GuiRounding`], so a 2× display gets two crisp
//! hairlines, never a blurry 1.5-point stroke — and the outer shade edge is a
//! deep neutral rather than Win95's pure black.
//!
//! Mobile is a standing requirement, so every interactive helper here lays
//! out a hit target of at least [`MIN_TOUCH_POINTS`] points per side (WCAG
//! 2.2, SC 2.5.8 "Target Size (Minimum)", 24 CSS px) and nothing depends on
//! hover to be discoverable: a latched toolbar toggle is sunken and tinted
//! whether or not a pointer exists. That floor holds in every
//! [`super::Density`]: a denser instrument buys its density from the gaps
//! between controls and from the padding inside oversized ones, never from
//! the size of the target.
//!
//! Two of the appearance axes land here rather than in egui's `Style`,
//! because egui has nowhere to put them: [`super::Density`] supplies the
//! paddings and margins these helpers lay out with, and
//! [`super::ChromeEdges`] chooses what [`paint_bevel`] paints inside a rect.
//! Both are read from the egui context by [`super::chrome`], so a call site
//! never threads them through - and `Flat` is a change of paint, not of
//! geometry, so switching it cannot move a control by a pixel.

use eframe::egui::containers::menu::MenuConfig;
use eframe::egui::emath::GuiRounding as _;
use eframe::egui::{
    Color32, InnerResponse, Margin, Painter, Popup, Rect, Response, Sense, TextStyle, TextWrapMode,
    Ui, UiKind, UiStackInfo, WidgetInfo, WidgetText, WidgetType, pos2, vec2,
};

use super::appearance::ChromeEdges;
use super::palette::Palette;

/// Minimum side of an interactive element, in points. WCAG 2.2 SC 2.5.8.
pub const MIN_TOUCH_POINTS: f32 = 24.0;

/// The four bevel treatments of the language.
///
/// `Raised`/`Sunken` are the chunky two-line forms for structural chrome — a
/// toolbar strip, a content well. The `*Thin` forms are the one-line versions
/// small controls use, exactly as Win95's flat toolbars did. `Etched` is the
/// grooved line of a group box.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Bevel {
    /// Two-line raised block: outer lit/shade, inner lit/shade.
    Raised,
    /// One-line raised edge, for small controls (hovered toolbar buttons).
    RaisedThin,
    /// Two-line inset well: the inversion of `Raised`.
    Sunken,
    /// One-line sunken edge, for small controls (pressed toolbar buttons).
    SunkenThin,
    /// A groove: shade ring with a lit ring nested inside it.
    Etched,
}

/// The four one-pixel edge strips of a rectangular ring.
///
/// Split out as pure geometry so the pixel-alignment contract is testable
/// without a GPU: each strip is `px` thick, the top/left pair and the
/// bottom/right pair tile the ring exactly, and the bottom/right pair is
/// painted last so it owns the two contested corners — the same corner
/// arbitration `DrawEdge` used.
pub fn ring_rects(rect: Rect, px: f32) -> [Rect; 4] {
    [
        // Top, then left (lit side first).
        Rect::from_min_max(rect.min, pos2(rect.max.x, rect.min.y + px)),
        Rect::from_min_max(rect.min, pos2(rect.min.x + px, rect.max.y)),
        // Bottom, then right (shade side, painted over the corners).
        Rect::from_min_max(pos2(rect.min.x, rect.max.y - px), rect.max),
        Rect::from_min_max(pos2(rect.max.x - px, rect.min.y), rect.max),
    ]
}

/// Paint one ring: top/left in `lit`, bottom/right in `shade`.
fn ring(painter: &Painter, rect: Rect, px: f32, lit: Color32, shade: Color32) {
    let [top, left, bottom, right] = ring_rects(rect, px);
    painter.rect_filled(top, 0.0, lit);
    painter.rect_filled(left, 0.0, lit);
    painter.rect_filled(bottom, 0.0, shade);
    painter.rect_filled(right, 0.0, shade);
}

/// Paint a bevel on the *inside* of `rect`, one physical pixel per line.
///
/// The rect is snapped to the device pixel grid first, so the lines land on
/// whole pixels at any scale factor and never anti-alias into grey mush.
///
/// `edges` picks the language, and it changes only what is painted: both
/// modes take the same rect, snap it the same way and paint inside it, so a
/// layout is identical in either. In [`ChromeEdges::Flat`] the two-line 3D
/// ring becomes one plain border, and the raised/sunken distinction is
/// carried by the fill the caller already painted (a button is
/// `face_raised`, a well is `well`). The border colour is `border_strong`
/// rather than `border` because in flat mode that line IS the affordance -
/// it is pinned at ≥ 3:1 against both face and well (W3C WCAG 2.2, 2023,
/// SC 1.4.11 non-text contrast) by `tests/theme_catalog.rs`, where the
/// bevelled edges only have to clear 1.3:1.
pub fn paint_bevel(
    painter: &Painter,
    rect: Rect,
    bevel: Bevel,
    palette: &Palette,
    edges: ChromeEdges,
) {
    let ppp = painter.pixels_per_point();
    let px = 1.0 / ppp;
    let rect = rect.round_to_pixels(ppp);
    if edges == ChromeEdges::Flat {
        let color = match bevel {
            // A groove is decoration between things, not the edge of a
            // thing: it stays the quieter line.
            Bevel::Etched => palette.border,
            _ => palette.border_strong,
        };
        ring(painter, rect, px, color, color);
        return;
    }
    match bevel {
        Bevel::Raised => {
            ring(painter, rect, px, palette.hi_outer, palette.sh_outer);
            ring(
                painter,
                rect.shrink(px),
                px,
                palette.hi_inner,
                palette.sh_inner,
            );
        }
        Bevel::RaisedThin => {
            ring(painter, rect, px, palette.hi_outer, palette.sh_inner);
        }
        Bevel::Sunken => {
            ring(painter, rect, px, palette.sh_inner, palette.hi_outer);
            ring(painter, rect.shrink(px), px, palette.sh_outer, palette.face);
        }
        Bevel::SunkenThin => {
            ring(painter, rect, px, palette.sh_inner, palette.hi_outer);
        }
        Bevel::Etched => {
            // BDR_SUNKENOUTER | BDR_RAISEDINNER: shade ring outside, lit ring
            // inside — the groove of a group box or separator.
            ring(painter, rect, px, palette.sh_inner, palette.hi_outer);
            ring(
                painter,
                rect.shrink(px),
                px,
                palette.hi_outer,
                palette.sh_inner,
            );
        }
    }
}

/// A raised structural strip: a toolbar row, a status bar, a dialog body.
///
/// Fills with the panel face and paints the chunky two-line bevel around the
/// contents. The inner margin keeps content clear of the bevel lines.
pub fn raised_frame<R>(ui: &mut Ui, add_contents: impl FnOnce(&mut Ui) -> R) -> InnerResponse<R> {
    let chrome = super::chrome(ui);
    let response = eframe::egui::Frame::NONE
        .fill(chrome.palette.face)
        .inner_margin(Margin::same(chrome.density.metrics().frame_margin))
        .show(ui, add_contents);
    paint_bevel(
        ui.painter(),
        response.response.rect,
        Bevel::Raised,
        &chrome.palette,
        chrome.edges,
    );
    response
}

/// An inset content well: the home of data, not controls.
///
/// Fills with the well colour — paper-light in the light variant, deeper than
/// the chrome in the dark one — and paints the sunken bevel, so the area
/// reads as *behind* the panel surface the way every Win95 list box did.
pub fn sunken_well<R>(ui: &mut Ui, add_contents: impl FnOnce(&mut Ui) -> R) -> InnerResponse<R> {
    let chrome = super::chrome(ui);
    let response = eframe::egui::Frame::NONE
        .fill(chrome.palette.well)
        .inner_margin(Margin::same(chrome.density.metrics().frame_margin))
        .show(ui, add_contents);
    paint_bevel(
        ui.painter(),
        response.response.rect,
        Bevel::Sunken,
        &chrome.palette,
        chrome.edges,
    );
    response
}

/// A group box: an etched border with a caption interrupting its top edge.
///
/// The caption sits on the panel face and the groove passes behind it, which
/// is the classic construction. The box is at least as wide as its caption,
/// so a short-contented group cannot amputate its own title.
pub fn group_box<R>(
    ui: &mut Ui,
    caption: &str,
    add_contents: impl FnOnce(&mut Ui) -> R,
) -> InnerResponse<R> {
    let chrome = super::chrome(ui);
    let palette = &chrome.palette;
    let metrics = chrome.density.metrics();
    let galley = WidgetText::from(caption).into_galley(
        ui,
        Some(TextWrapMode::Extend),
        f32::INFINITY,
        TextStyle::Body,
    );
    let caption_height = galley.size().y;
    let caption_width = galley.size().x;
    // Room above the content for the caption, and enough on the other sides
    // that content never collides with the groove.
    let top_margin = (caption_height + metrics.group_caption_gap).ceil() as i8;
    let response = eframe::egui::Frame::NONE
        .inner_margin(Margin {
            left: metrics.group_margin_x,
            right: metrics.group_margin_x,
            top: top_margin,
            bottom: metrics.group_margin_bottom,
        })
        .show(ui, |ui| {
            ui.set_min_width(caption_width + f32::from(metrics.group_margin_x) * 1.6);
            add_contents(ui)
        });
    let rect = response.response.rect;
    // The groove's top edge runs through the caption's vertical midpoint.
    let box_rect = Rect::from_min_max(
        pos2(rect.min.x, rect.min.y + caption_height * 0.5),
        rect.max,
    );
    let painter = ui.painter();
    paint_bevel(painter, box_rect, Bevel::Etched, palette, chrome.edges);
    // A face-coloured band under the caption hides the groove behind the
    // text; painted after the bevel so it wins.
    let caption_pos = pos2(rect.min.x + f32::from(metrics.group_margin_x), rect.min.y);
    let band = Rect::from_min_size(
        pos2(caption_pos.x - 3.0, rect.min.y),
        vec2(caption_width + 6.0, caption_height),
    );
    painter.rect_filled(band, 0.0, palette.face);
    painter.galley(caption_pos, galley, palette.text);
    response
}

/// An etched separator line, horizontal or vertical to match the layout —
/// the two-hairline groove Win95 drew between toolbar groups and menu items.
pub fn etched_separator(ui: &mut Ui) {
    let chrome = super::chrome(ui);
    let palette = &chrome.palette;
    let thickness = chrome.density.metrics().separator_thickness;
    // Flat chrome draws ONE plain line where the groove's shade-then-lit
    // pair goes; the allocation is unchanged either way, so a bar does not
    // re-flow when the edge language changes.
    let (near, far) = match chrome.edges {
        ChromeEdges::Bevelled => (palette.sh_inner, Some(palette.hi_outer)),
        ChromeEdges::Flat => (palette.border, None),
    };
    let ppp = ui.painter().pixels_per_point();
    let px = 1.0 / ppp;
    // In a horizontal layout the separator is a vertical line, exactly as
    // `egui::Separator` decides it.
    let vertical_line = ui.layout().is_horizontal();
    // Span the container, but never ENLARGE it. "Available" space cannot be
    // trusted as a length here: in a sizing pass (the first frame of every
    // auto-sized window and popup) it is a probe that would become the
    // measured size; in an auto-sized window's later frames it is the
    // `Resize` container's screen-sized opening bid (`Resize::auto_sized`
    // starts from `default_size = INFINITY`, clamped to the screen, and its
    // `desired_size` never shrinks); in a scroll area it is literally
    // infinite. A separator that allocates all of that becomes the widest
    // content in the container and ratchets an auto-sized window out to the
    // screen edge — stock `egui::Separator` does exactly this. So the fill
    // is bounded by the one number only real content can enlarge: this
    // `Ui`'s own extent from the previous pass (`Context::read_response`
    // documents that reading a `Ui` from inside it returns last pass's
    // rect).
    let bound = ui
        .ctx()
        .read_response(ui.unique_id())
        .map(|response| {
            if vertical_line {
                response.rect.height()
            } else {
                response.rect.width()
            }
        })
        .unwrap_or(f32::INFINITY);
    let length = |available: f32| {
        if ui.is_sizing_pass() {
            return 0.0;
        }
        let length = available.min(bound);
        if length.is_finite() {
            length.max(MIN_TOUCH_POINTS)
        } else {
            // First-ever pass of an unbounded container: hold the minimum
            // rather than allocate infinity.
            MIN_TOUCH_POINTS
        }
    };
    let (rect, _) = if vertical_line {
        let height = length(ui.available_size_before_wrap().y);
        ui.allocate_exact_size(vec2(thickness, height), Sense::hover())
    } else {
        let width = length(ui.available_size_before_wrap().x);
        ui.allocate_exact_size(vec2(width, thickness), Sense::hover())
    };
    if !ui.is_rect_visible(rect) {
        return;
    }
    let rect = rect.round_to_pixels(ppp);
    let painter = ui.painter();
    if vertical_line {
        let x = (rect.center().x).round_to_pixels(ppp);
        painter.rect_filled(
            Rect::from_min_max(pos2(x, rect.min.y), pos2(x + px, rect.max.y)),
            0.0,
            near,
        );
        if let Some(far) = far {
            painter.rect_filled(
                Rect::from_min_max(pos2(x + px, rect.min.y), pos2(x + 2.0 * px, rect.max.y)),
                0.0,
                far,
            );
        }
    } else {
        let y = (rect.center().y).round_to_pixels(ppp);
        painter.rect_filled(
            Rect::from_min_max(pos2(rect.min.x, y), pos2(rect.max.x, y + px)),
            0.0,
            near,
        );
        if let Some(far) = far {
            painter.rect_filled(
                Rect::from_min_max(pos2(rect.min.x, y + px), pos2(rect.max.x, y + 2.0 * px)),
                0.0,
                far,
            );
        }
    }
}

/// A flat toolbar button: invisible at rest, raised under the pointer,
/// sunken while pressed — the Office 97 evolution of the Win95 toolbar,
/// which is exactly the "structural language, modern execution" bargain.
///
/// At rest the button still has its full ≥ 24-point hit target; flatness is
/// a visual state, not a smaller control.
pub fn toolbar_button(ui: &mut Ui, text: impl Into<WidgetText>) -> Response {
    toolbar_control(ui, text.into(), false)
}

/// A latching toolbar button. When `selected`, it is sunken and tinted
/// toward the accent — visible without hover, so it works under a finger.
pub fn toolbar_toggle(ui: &mut Ui, selected: bool, text: impl Into<WidgetText>) -> Response {
    toolbar_control(ui, text.into(), selected)
}

/// A menu title in the bar: the same flat-until-hover control, latched for as
/// long as its menu is down — the Win95 menu bar's inverted title, in this
/// theme's grammar rather than by inverting the colours.
///
/// `egui::Ui::menu_button` is not used because it hard-codes a stock
/// `egui::Button`, which paints a raised face with a border at rest: four of
/// those in a row read as four pushbuttons, not as a menu bar. This is the
/// composition `egui::containers::menu::MenuButton` performs — the button,
/// then `Popup::menu` over its response, tagged as a menu so `ui.close()`,
/// submenus and menu styling inside `content` behave exactly as they do under
/// `menu_button` — with the button swapped for [`toolbar_button`]'s painter.
///
/// The latch reads the popup's state from the previous frame, which is when
/// it was last written: the click that opens the menu is processed after this
/// button has already painted. One frame is not perceptible, and egui is
/// repainting continuously while a menu is open.
pub fn toolbar_menu<R>(
    ui: &mut Ui,
    text: impl Into<WidgetText>,
    content: impl FnOnce(&mut Ui) -> R,
) -> Response {
    // The id `toolbar_control`'s `allocate_exact_size` is about to take, and
    // from it the id `Popup::menu` will derive (`Popup::default_response_id`
    // is `response.id.with("popup")`). Pinned by
    // `tests/theme_contract.rs::a_toolbar_menu_latches_while_its_menu_is_down`,
    // so a change in either egui's id derivation or in `toolbar_control`'s
    // allocation order fails a test instead of silently un-latching the bar.
    let open = Popup::is_id_open(ui.ctx(), ui.next_auto_id().with("popup"));
    let response = toolbar_control(ui, text.into(), open);
    let config = MenuConfig::new();
    Popup::menu(&response)
        .close_behavior(config.close_behavior)
        .style(config.style.clone())
        .info(UiStackInfo::new(UiKind::Menu).with_tag_value(MenuConfig::MENU_CONFIG_TAG, config))
        .show(content);
    response
}

/// A one-line data readout inset into the chrome: the toolbar's equivalent of
/// [`sunken_well`], sized to stand in a row of [`toolbar_button`]s.
///
/// Readouts are the reason this exists as its own helper rather than as a
/// `ui.label` on the bar. A bare label inherits whatever ground it happens to
/// land on, which is how a tilt value or a live status ends up as dark ink on
/// a dark window; here the ground is always [`Palette::well`], and the ink
/// defaults to [`Palette::text`] — a pair the contrast contract pins at ≥ 7:1
/// in both variants. It is never disabled and never weak.
///
/// A caller that needs to shout may name its own colour
/// (`RichText::new(…).color(ui.visuals().error_fg_color)`); that colour wins,
/// because `Painter::galley` treats [`Palette::text`] here as the *fallback*
/// for text that did not name one. The theme's `warn` and `error` inks are
/// the intended overrides and both clear 4.5:1 on the well in both variants
/// (6.7 – 8.1:1); an arbitrary colour is the caller's contrast to answer for.
///
/// The width range is explicit because a readout's text changes as data
/// changes: `min_width` stops the whole bar from re-flowing every time the
/// elevation gains a digit, and `max_width` truncates rather than wrapping,
/// because a second line would change the height of the bar itself.
pub fn sunken_readout(
    ui: &mut Ui,
    min_width: f32,
    max_width: f32,
    text: impl Into<WidgetText>,
) -> Response {
    let chrome = super::chrome(ui);
    let palette = &chrome.palette;
    // The vertical padding is small on purpose: the height floor below is
    // what sets the height, so a readout comes out exactly as tall as a
    // `toolbar_button` and the row of controls shares one baseline instead of
    // stepping up and down across the bar.
    let padding = chrome.density.metrics().readout_padding;
    let galley = text.into().into_galley(
        ui,
        Some(TextWrapMode::Truncate),
        (max_width - 2.0 * padding.x).max(1.0),
        TextStyle::Body,
    );
    let size = vec2(
        (galley.size().x + 2.0 * padding.x).max(min_width),
        (galley.size().y + 2.0 * padding.y).max(MIN_TOUCH_POINTS),
    );
    let (rect, response) = ui.allocate_exact_size(size, Sense::hover());
    response.widget_info(|| WidgetInfo::labeled(WidgetType::Label, true, galley.text()));

    if ui.is_rect_visible(rect) {
        let ppp = ui.painter().pixels_per_point();
        let rect = rect.round_to_pixels(ppp);
        let painter = ui.painter();
        painter.rect_filled(rect, 0.0, palette.well);
        paint_bevel(painter, rect, Bevel::Sunken, palette, chrome.edges);
        let text_pos = pos2(
            rect.min.x + padding.x,
            rect.center().y - 0.5 * galley.size().y,
        )
        .round_to_pixels(ppp);
        painter.galley(text_pos, galley, palette.text);
    }
    response
}

fn toolbar_control(ui: &mut Ui, text: WidgetText, selected: bool) -> Response {
    let chrome = super::chrome(ui);
    let palette = &chrome.palette;
    let galley = text.into_galley(
        ui,
        Some(TextWrapMode::Extend),
        f32::INFINITY,
        TextStyle::Button,
    );
    let padding = chrome.density.metrics().control_padding;
    // The touch floor is applied AFTER the density padding, not scaled with
    // it: a denser bar has tighter padding around its labels, and a control
    // whose label is short still ends up 24 points on a side.
    let size = (galley.size() + 2.0 * padding).max(vec2(MIN_TOUCH_POINTS, MIN_TOUCH_POINTS));
    let (rect, response) = ui.allocate_exact_size(size, Sense::click());
    response.widget_info(|| {
        WidgetInfo::selected(WidgetType::Button, ui.is_enabled(), selected, galley.text())
    });

    if ui.is_rect_visible(rect) {
        let ppp = ui.painter().pixels_per_point();
        let px = 1.0 / ppp;
        let rect = rect.round_to_pixels(ppp);
        let pressed = response.is_pointer_button_down_on();
        let hovered = response.hovered();
        // Fill first, then bevel, exactly as the panel painters do.
        let (fill, bevel) = if pressed {
            (Some(palette.face_pressed), Some(Bevel::SunkenThin))
        } else if selected {
            (Some(palette.selection_tint), Some(Bevel::SunkenThin))
        } else if hovered {
            (Some(palette.hover), Some(Bevel::RaisedThin))
        } else {
            (None, None)
        };
        let painter = ui.painter();
        if let Some(fill) = fill {
            painter.rect_filled(rect, 0.0, fill);
        }
        if let Some(bevel) = bevel {
            paint_bevel(painter, rect, bevel, palette, chrome.edges);
        }
        // The classic tactile cue: pressed content shifts one pixel
        // down-right, as if the cap travelled with the finger.
        let nudge = if pressed {
            vec2(px, px)
        } else {
            vec2(0.0, 0.0)
        };
        let text_color = if ui.is_enabled() {
            palette.text
        } else {
            palette.text_disabled
        };
        let text_pos = (rect.center() - 0.5 * galley.size() + nudge).round_to_pixels(ppp);
        painter.galley(text_pos, galley, text_color);
        // Keyboard focus: a one-pixel accent ring just inside the edge. The
        // modern stand-in for the Win95 dotted marquee.
        if response.has_focus() {
            ring(
                painter,
                rect.shrink(3.0 * px),
                px,
                palette.link,
                palette.link,
            );
        }
    }
    response
}

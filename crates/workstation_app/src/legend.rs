//! The colour bar that says what a pane's colours mean.
//!
//! Three pieces on purpose. [`legend_layout`] is arithmetic - which span the
//! bar covers, where each label sits on it, what each label reads - and every
//! rule in it is pinned by a test that needs no window. [`legend_geometry`] is
//! the arithmetic that needs a font: how wide the column must be to hold what
//! is in it, and so where every rect sits. [`draw_legend`] turns those two into
//! shapes and decides nothing.
//!
//! The third rule, after the two below, is that the legend is exactly as wide
//! as its own contents. It used to be a fixed 54 points whatever it held, wrong
//! in both directions at once: a correlation coefficient bar labelled "0.4" to
//! "1.0" got about twice the width it needed, so an opaque box sat over the
//! radar explaining nothing, while a badge like "ASSUMED ENV" ran off the left
//! of the panel onto the storm with no backing. Both come of never measuring,
//! so both are fixed by measuring - with `egui`'s own layout, at the same
//! [`egui::FontId`] the glyphs are painted with. A character count times a
//! guessed advance is not a measurement: "0.4" and "-100" are not the same
//! width, and neither are "l" and "W" in the title's proportional font. See
//! [`measure_legend`] for the rule that keeps a BADGE from undoing all that.
//!
//! The failure this file exists to prevent is a bar that lies about its own
//! range. Every built-in reflectivity palette is fully transparent below
//! 10 dBZ, and the reflectivity domain is declared from -32 dBZ, so a bar drawn
//! straight across the declared domain hangs 42 dBZ of labels on a stretch of
//! scope that stays empty no matter what the radar returns. An analyst reading
//! weak-echo structure off the bottom third of that bar is reading nothing.
//! [`legend_span`] clips the declared domain against the part of the palette
//! that actually has ink, and refuses to produce a legend at all when the two
//! do not meet.
//!
//! The second rule is the unit split. The tick ladder is chosen in DISPLAY
//! units, because "50 kt" is the number an analyst reads off a warning, and
//! every tick is then converted BACK to engine units before it touches the bar
//! or the palette. Sampling a colour table with a knots number is a factor of
//! 1.94 error that still paints a picture that looks plausible, which is
//! exactly the kind of error that ships.
//!
//! The ladder itself is Heckbert's, via `product_engine::ticks`: Paul S.
//! Heckbert, "Nice Numbers for Graph Labels", in Andrew S. Glassner (ed.),
//! Graphics Gems, Academic Press, 1990, pp. 61-63.

use std::sync::Arc;

use color_tables::{ColorTable, Rgba8};
use eframe::egui;
use product_engine::ticks::nice_ticks;
use product_engine::{DisplayDomain, ValueRange};

/// The most ticks a bar may carry before the ladder is thinned.
///
/// The layout does not know how tall the bar will be, so this is the worst
/// case: the shortest bar [`draw_legend`] will paint is [`MIN_BAR_HEIGHT`]
/// points, and a 10 point label needs about 18 points of pitch before the
/// ascenders of one label touch the descenders of the one above. 140 / 18 is
/// 7.8, so eight labels is the most that can be read on the shortest bar this
/// draws.
const MAX_LEGEND_TICKS: usize = 8;

/// A ladder of one label is not a scale, so thinning stops here.
const MIN_LEGEND_TICKS: usize = 2;

/// The most decimal places a tick label may carry.
///
/// Correlation coefficient is the only product that needs three, and nothing
/// needs four: a ladder that fine has a step below the resolution of the `f32`
/// the value is stored in.
const MAX_LABEL_DECIMALS: usize = 3;

/// How far outside the display span a tick may sit and still count as being on
/// the bar, as a fraction of the span's magnitude.
///
/// The bar's ends are `f32` and the ladder is `f64`, and the widening between
/// them invents a bound that is a hair off a round number. Correlation
/// coefficient is the case that shows it: the domain's `0.2_f32` widens to
/// 0.20000000298023224, which puts the round number 0.2 outside the range, so
/// `nice_ticks` correctly drops the tick at the bottom of the bar; it decides
/// membership by exact comparison and documents that it does. 1e-7 is about
/// the relative width of an `f32`, so this recovers that tick and nothing
/// coarser. The engine-side containment check in [`legend_layout`] is what
/// keeps the slack honest: a tick that does not land inside the `f32` span is
/// still dropped.
const DISPLAY_SLACK_FRACTION: f64 = 1e-7;

/// An UPPER BOUND on how wide a legend column may become. NOT the width it is
/// drawn at.
///
/// This used to be the width every legend was drawn at, 54 points, whatever it
/// held. What a legend claims now comes from [`legend_geometry`], which
/// measures the contents in the font they are painted in; a caller needing the
/// number calls that and reads [`LegendGeometry::column`]. No reader of this
/// constant exists outside this file, so nothing was silently given a different
/// number - but the meaning changed, so: RESERVING against it is still safe,
/// because over-reserving is the safe direction; POSITIONING against it is not.
///
/// It is kept because it still has a job, but a smaller one than when it was
/// first widened to 120. Letting a BADGE widen the column to this bound was
/// itself the storm-covering box this file exists to remove: `app.rs` builds
/// `ASSUMED ENV H0 3.0 km / H-20 6.0 km ARL` for every hail product computed
/// from a guessed environment, 211 points of 9-point monospace, so MESH, POH
/// and POSH each got a 120-point column behind a 128-point panel - twice the 62
/// of the fixed 54-point version this replaced, on bars that need 37. Badges
/// therefore WRAP ([`measure_legend`]) and lift the column only as far as their
/// longest WORD, so the cap is now reached only by a title, a unit, or a single
/// word longer than any this application has.
pub const LEGEND_WIDTH: f32 = 120.0;

/// Width of the coloured bar itself, inside the legend column.
const BAR_WIDTH: f32 = 12.0;

/// Padding between the legend column and the edge of its backing panel.
///
/// The panel therefore bleeds this far into [`EDGE_MARGIN`], which is why the
/// margin is wider than the padding.
const PANEL_PAD: f32 = 4.0;

/// Gap between the legend column and the pane's right edge.
const EDGE_MARGIN: f32 = 8.0;

/// Inset from the top of the pane rect. The pane header is 26 points tall, so
/// this clears it when the caller passes the whole pane rect, and merely insets
/// when the caller passes the content rect below it.
const TOP_MARGIN: f32 = 34.0;

/// Inset from the bottom of the pane rect. The cursor readout owns the bottom
/// left; this keeps the legend's own unit label off the same line.
const BOTTOM_MARGIN: f32 = 26.0;

/// Horizontal gap between a tick label and the tick mark it belongs to.
const LABEL_GAP: f32 = 3.0;

/// Length of the tick mark drawn against the bar's left edge.
const TICK_MARK: f32 = 4.0;

/// Vertical gap between the badge stack and the top of the bar.
const BAR_GAP: f32 = 4.0;

const TITLE_FONT_SIZE: f32 = 11.0;
const BADGE_FONT_SIZE: f32 = 9.0;
const LABEL_FONT_SIZE: f32 = 10.0;

/// Vertical gap between the bottom of the bar and the unit label under it.
///
/// The only line pitch left in this file. The title, badges and unit used to be
/// given a FIXED pitch - 14, 11 and 13 points - while `draw_legend` stacked
/// them by their galleys' real heights, which on the embedded fonts are 13, 10
/// and 12. Reserve and paint disagreed by a point per line, and a wrapped badge
/// would have turned that slack into an overlap: a two-row badge is two rows of
/// galley whatever a per-BADGE constant says. Both now read the same galleys.
const UNIT_GAP: f32 = 2.0;

/// The most badges that may be stacked under the title. A pane that somehow
/// collects a dozen qualifiers must not push the bar off the bottom of itself.
const MAX_BADGES: usize = 4;

/// The most ROWS the badge stack may occupy, however many badges there are and
/// however long they run.
///
/// A ceiling, not a reservation: [`legend_geometry`] spends fewer rows when the
/// pane is too short to afford them, down to one row per badge, which is
/// exactly the header the fixed line pitch used to reserve. Without that a
/// four-row hail badge would take the bar's place on a 250-point pane, where
/// the version this replaced drew a bar.
const MAX_BADGE_ROWS: usize = 4;

/// The row budget is shared out one row per badge at a minimum, so a stack that
/// could hold more badges than rows would overrun it.
const _: () = assert!(MAX_BADGES <= MAX_BADGE_ROWS);

/// Below this the bar is too short to carry [`MIN_LEGEND_TICKS`] readable
/// labels, so nothing is drawn at all. An unreadable legend is worse than no
/// legend: it still covers the radar data underneath it.
const MIN_BAR_HEIGHT: f32 = 140.0;

/// Below this pane width the legend would cover the storm rather than explain
/// it, so nothing is drawn.
const MIN_DATA_WIDTH: f32 = 90.0;

/// The most gradient rows painted, however tall the bar is. A 4K pane is under
/// 2200 rows; this only stops a pathological scale factor from queueing tens of
/// thousands of quads.
const MAX_GRADIENT_ROWS: usize = 4096;

const TITLE_COLOR: egui::Color32 = egui::Color32::from_rgb(239, 243, 246);
const LABEL_COLOR: egui::Color32 = egui::Color32::from_rgb(220, 228, 234);
const MUTED_COLOR: egui::Color32 = egui::Color32::from_rgb(166, 184, 196);
const BADGE_COLOR: egui::Color32 = egui::Color32::from_rgb(255, 214, 120);
const BAR_OUTLINE_COLOR: egui::Color32 = egui::Color32::from_rgb(90, 104, 116);

/// Backing panel behind the bar: dark enough to read labels against a
/// reflectivity core, translucent enough not to hide the storm underneath.
///
/// A function rather than a const because `Color32::from_rgba_unmultiplied`
/// premultiplies, and so is not a `const fn`.
fn panel_color() -> egui::Color32 {
    egui::Color32::from_rgba_unmultiplied(4, 7, 10, 196)
}

/// One labelled position on the bar.
#[derive(Clone, Debug, PartialEq)]
pub struct LegendTick {
    /// The value a colour lookup would use. Engine units, always.
    pub engine_value: f32,
    /// The rounded number an analyst reads, in display units, without its unit.
    pub label: String,
    /// Where the tick sits on the bar: 0 at the bottom, 1 at the top.
    pub fraction: f32,
}

/// Everything about a colour bar that can be worked out without a window.
#[derive(Clone, Debug, PartialEq)]
pub struct LegendLayout {
    /// The engine-value span the bar covers, bottom to top.
    pub span: ValueRange,
    /// Ascending by value, and therefore by [`LegendTick::fraction`].
    pub ticks: Vec<LegendTick>,
    /// The unit the labels are in. Empty for a dimensionless product.
    pub unit_label: &'static str,
}

/// The engine-value span a legend should show: the product's declared domain
/// intersected with the part of the palette that actually has ink.
///
/// Returns `None` in the three cases where no honest bar exists, and the caller
/// is expected to say "palette/domain mismatch" rather than paint one anyway:
///
/// 1. The palette has no opaque stop at all, so it can never ink anything.
/// 2. The palette's ink misses the domain entirely - a reflectivity table left
///    selected on a correlation coefficient pane, say. Drawing the declared
///    domain here would produce a bar of one flat colour under a full ladder of
///    labels, which reads as a working legend.
/// 3. The overlap is a single value. `ColorTable::inked_value_span` documents
///    that a palette with one opaque stop reports a zero-width span; a bar with
///    no extent divides by zero when a tick is placed on it, and every tick
///    comes back NaN.
pub fn legend_span(domain: &DisplayDomain, table: &ColorTable) -> Option<ValueRange> {
    let (inked_low, inked_high) = table.inked_value_span()?;
    let inked = ValueRange::new(inked_low, inked_high);
    let span = domain.declared_engine_range.intersect(inked)?;
    if !span.min.is_finite() || !span.max.is_finite() || span.min >= span.max {
        return None;
    }
    Some(span)
}

/// The span, the tick ladder, and the unit for one product drawn with one
/// palette, or `None` when [`legend_span`] finds nothing honest to draw.
///
/// The ladder can come back empty. A domain whose display transform has a zero
/// or non-finite scale collapses to a single display value, and `nice_ticks`
/// answers an empty ladder for a range with no span rather than a column of
/// identical labels. That is why no explicit invertibility guard appears here:
/// the tick loop never runs, so `to_engine` is never asked to divide by zero.
/// Every built-in product is pinned by test to keep at least two ticks.
pub fn legend_layout(domain: &DisplayDomain, table: &ColorTable) -> Option<LegendLayout> {
    let span = legend_span(domain, table)?;

    // Both ends converted, then ordered. Ordering rather than assuming that
    // `span.min` maps to the low display value costs one comparison, and means
    // a future transform with a negative scale - a depth below a surface, an
    // inverted index - produces a correct ladder instead of an empty one.
    let first = domain.to_display(span.min);
    let second = domain.to_display(span.max);
    let display_low = first.min(second);
    let display_high = first.max(second);

    let magnitude = display_low
        .abs()
        .max(display_high.abs())
        .max(display_high - display_low);
    let slack = magnitude * DISPLAY_SLACK_FRACTION;

    let display_ticks = nice_ticks(
        display_low - slack,
        display_high + slack,
        domain.tick_hint.target_intervals,
    );
    let decimals = label_decimals(&display_ticks);

    let mut ticks: Vec<LegendTick> = Vec::with_capacity(display_ticks.len());
    for display_value in display_ticks {
        let engine_value = domain.to_engine(display_value);
        // The engine span is the authority on what is on the bar. A tick that
        // only survived the slack above and does not land inside the `f32` span
        // is dropped here.
        if !engine_value.is_finite() || !span.contains(engine_value) {
            continue;
        }
        ticks.push(LegendTick {
            engine_value,
            // Formatted from the DISPLAY number, not from the engine value
            // converted back: 0.2 kept as an `f64` prints "0.2", while the same
            // tick round-tripped through `f32` is 0.20000000298 and prints
            // "0.200" the moment the label gains a decimal place.
            label: format!("{display_value:.decimals$}"),
            fraction: bar_fraction(span, engine_value),
        });
    }

    // Ascending by engine value. `nice_ticks` already ascends in display units,
    // which is the same order for every transform this application has; the
    // sort is what makes that true for a negative scale as well.
    ticks.sort_by(|left, right| left.engine_value.total_cmp(&right.engine_value));
    thin_to_fit(&mut ticks);

    Some(LegendLayout {
        span,
        ticks,
        unit_label: domain.display_unit.label(),
    })
}

/// Where a value sits on the bar: 0 at the bottom, 1 at the top.
///
/// Clamped because a tick at an endpoint is converted to display units and back
/// through an `f32`, and a round trip that lands half an ulp past the end would
/// otherwise report a fraction of 1.0000001 and paint the tick above the bar.
fn bar_fraction(span: ValueRange, engine_value: f32) -> f32 {
    let width = f64::from(span.max) - f64::from(span.min);
    if width <= 0.0 {
        return 0.0;
    }
    let fraction = (f64::from(engine_value) - f64::from(span.min)) / width;
    (fraction as f32).clamp(0.0, 1.0)
}

/// The fewest decimal places that write every tick exactly.
///
/// Derived from the ticks rather than taken from `DisplayDomain::decimals`,
/// which is a readout's precision and not a label's: correlation coefficient
/// asks for three decimals so a probe can show 0.847, and a bar labelled
/// "0.200 0.400 0.600" spends three characters per label saying nothing.
fn label_decimals(display_ticks: &[f64]) -> usize {
    for decimals in 0..=MAX_LABEL_DECIMALS {
        let scale = 10f64.powi(decimals as i32);
        let exact = display_ticks.iter().all(|tick| {
            let scaled = tick * scale;
            // Relative, with a floor of one: the absolute error in a tick grows
            // with its magnitude, and a tick of 0 has no magnitude to scale by.
            (scaled - scaled.round()).abs() <= scaled.abs().max(1.0) * 1e-9
        });
        if exact {
            return decimals;
        }
    }
    MAX_LABEL_DECIMALS
}

/// Drop every other tick until the ladder fits, keeping the ends and the even
/// spacing.
///
/// Halving is the only thinning that preserves both. Dropping from one end
/// leaves a bar labelled over three quarters of its height and blank over the
/// last quarter, which reads as missing data rather than as a thinned ladder.
fn thin_to_fit(ticks: &mut Vec<LegendTick>) {
    while ticks.len() > MAX_LEGEND_TICKS && ticks.len().div_ceil(2) >= MIN_LEGEND_TICKS {
        let thinned: Vec<LegendTick> = std::mem::take(ticks)
            .into_iter()
            .enumerate()
            .filter(|(index, _)| index % 2 == 0)
            .map(|(_, tick)| tick)
            .collect();
        *ticks = thinned;
    }
}

/// One legend laid out with the real font: every line as a `Galley`, and the
/// rects those galleys will be painted into.
///
/// The galleys are carried through to paint time rather than measured here and
/// re-created there. A width taken from one layout and glyphs painted from
/// another can disagree - a different `FontId`, a different rounding of
/// `pixels_per_point` - and the disagreement is invisible until a label hangs
/// over the bar it is labelling.
pub struct LegendGeometry {
    /// The translucent backing panel. Every mark the legend makes is inside it.
    pub panel: egui::Rect,
    /// The column the contents are right-aligned in. [`Self::panel`] is this
    /// expanded by [`PANEL_PAD`], and its width is what the legend costs the
    /// pane.
    pub column: egui::Rect,
    /// The coloured bar.
    pub bar: egui::Rect,
    /// The x that every tick label's RIGHT edge sits on.
    pub label_right: f32,
    /// One galley per entry in [`LegendLayout::ticks`], in the same order.
    labels: Vec<Arc<egui::Galley>>,
    /// `None` when the caller named the product with an empty string.
    title: Option<Arc<egui::Galley>>,
    /// Blank entries dropped, then truncated to [`MAX_BADGES`].
    badges: Vec<Arc<egui::Galley>>,
    /// `None` for a dimensionless product, which has no unit to name.
    unit: Option<Arc<egui::Galley>>,
}

impl LegendGeometry {
    /// The horizontal span the tick label at `index` is painted across, or
    /// `None` when there is no such tick.
    ///
    /// [`draw_legend`] places every label through this, so a test that checks a
    /// label against the bar or against the panel is checking what is on the
    /// screen rather than re-deriving it.
    pub fn label_x_span(&self, index: usize) -> Option<(f32, f32)> {
        let galley = self.labels.get(index)?;
        Some((self.label_right - galley.size().x, self.label_right))
    }
}

/// A legend's contents laid out, before they are given a position.
struct MeasuredLegend {
    column_width: f32,
    labels: Vec<Arc<egui::Galley>>,
    title: Option<Arc<egui::Galley>>,
    badges: Vec<Arc<egui::Galley>>,
    unit: Option<Arc<egui::Galley>>,
}

impl MeasuredLegend {
    /// The height the title and badge stack occupy, from the galleys that will
    /// be painted rather than from a per-line constant. This places the top of
    /// the bar, and [`draw_legend`] stacks the same galleys by the same
    /// heights, so the two cannot drift apart; a wrapped badge is one galley of
    /// several rows and is measured as such.
    fn header_height(&self) -> f32 {
        self.title
            .iter()
            .chain(self.badges.iter())
            .map(|galley| galley.size().y)
            .sum()
    }

    /// The room the unit line under the bar needs, its gap included, or nothing
    /// at all for a dimensionless product that has no unit to name.
    fn unit_height(&self) -> f32 {
        self.unit
            .as_ref()
            .map_or(0.0, |galley| UNIT_GAP + galley.size().y)
    }
}

/// Lay every line of a legend out and work out how wide the column must be.
///
/// The width rule is a floor, a lift, and a cap, and each part is deliberate.
///
/// The FLOOR is the BAR BLOCK - the bar, its tick marks, the gap, and the
/// widest tick label as the font actually lays it out. It is what the legend
/// cannot do without, so it is never traded away. This is the half a fixed
/// width gets wrong in both directions: too wide for a bar labelled "0.4",
/// about right for one labelled "-32", and there is no one number that is right
/// for both.
///
/// The LIFT is the title, the unit under the bar, and - only as far as its
/// longest WORD - the badge stack. Title and unit are short and cannot be
/// abbreviated without lying: "deg/km" is what makes KDP's column 36 points
/// rather than the 31 its "-2" label needs, and a unit cut to "deg/k" would be
/// a different quantity.
///
/// Badges are the part that has to be handled differently, and that is the
/// second defect this rule exists to fix. A badge says what LIMITS the picture
/// ("PARTIAL" means this frame is half a volume, "ASSUMED ENV" means the hail
/// sizes came from a guessed freezing level), so it has to be readable - but
/// `app.rs` does not send words, it sends sentences: every hail product carries
/// `ASSUMED ENV H0 3.0 km / H-20 6.0 km ARL`, 211 points of 9-point monospace.
/// Letting that widen the column pinned MESH, POH and POSH at the 120-point cap
/// behind a 128-point panel when their bars need 37 - twice the panel of the
/// fixed-width version this file replaced. So a badge WRAPS inside the column
/// instead and may lift it only far enough that no WORD is broken mid-letter; a
/// stack that still does not fit is elided with a visible ellipsis at
/// [`MAX_BADGE_ROWS`]. Wrapping keeps both properties at once: the whole of
/// "ASSUMED ENV" is on screen AND the column stays at 38.
///
/// The CAP is [`LEGEND_WIDTH`], which after the wrapping rule only a title, a
/// unit, or a single word longer than any in this application can reach. Past
/// it a line is elided rather than accommodated, because an ellipsis is a
/// truncation the analyst can see and a quarter-pane legend is not.
fn measure_legend(
    painter: &egui::Painter,
    layout: &LegendLayout,
    title: &str,
    badges: &[String],
    max_badge_rows: usize,
) -> MeasuredLegend {
    let label_font = egui::FontId::monospace(LABEL_FONT_SIZE);
    let labels: Vec<Arc<egui::Galley>> = layout
        .ticks
        .iter()
        .map(|tick| painter.layout_no_wrap(tick.label.clone(), label_font.clone(), LABEL_COLOR))
        .collect();
    let widest_label = labels
        .iter()
        .map(|galley| galley.size().x)
        .fold(0.0_f32, f32::max);
    // A bar with no ladder needs no room for one, not even the tick mark and
    // the gap. `legend_layout` documents when that happens: a display transform
    // with a zero scale has no position on the bar a label could mean, so it
    // answers an empty ladder rather than a stack of identical numbers.
    let bar_block = if labels.is_empty() {
        BAR_WIDTH
    } else {
        BAR_WIDTH + TICK_MARK + LABEL_GAP + widest_label
    };

    let title_font = egui::FontId::proportional(TITLE_FONT_SIZE);
    let badge_font = egui::FontId::monospace(BADGE_FONT_SIZE);
    // A blank line is not laid out at all, and that is a correctness rule
    // rather than a saving. `epaint` gives a galley with no glyphs the empty
    // bounding rect `Rect::NOTHING`, whose corners are infinite; the text shape
    // built from it rotates that box by its (zero) angle, which multiplies an
    // infinity by a zero and hands the tessellator a NaN-bounded shape. The
    // registry never names a product with an empty string, so this is a guard
    // against a caller rather than a live case - but it also means an unnamed
    // pane spends no line on its name.
    let title_galley = (!title.trim().is_empty())
        .then(|| painter.layout_no_wrap(title.to_owned(), title_font.clone(), TITLE_COLOR));
    let unit_galley = (!layout.unit_label.is_empty()).then(|| {
        painter.layout_no_wrap(
            layout.unit_label.to_owned(),
            label_font.clone(),
            MUTED_COLOR,
        )
    });

    let drawn_badges: Vec<&String> = badges
        .iter()
        .filter(|badge| !badge.trim().is_empty())
        .take(MAX_BADGES)
        .collect();

    let text_block = title_galley
        .iter()
        .chain(unit_galley.iter())
        .map(|galley| galley.size().x)
        .chain(std::iter::once(widest_badge_word(
            painter,
            &drawn_badges,
            &badge_font,
        )))
        .fold(0.0_f32, f32::max);

    // The bar block is the floor and the text block only lifts it, so a tick
    // label wider than the cap keeps its room rather than being drawn over the
    // bar. Nothing in the registry comes close - the widest built-in ladder is
    // velocity's "-100" - and a test pins that every built-in column fits under
    // the cap, so this is a guard against a future product, not a live case.
    let column_width = bar_block.max(text_block.min(LEGEND_WIDTH));

    // Re-laid out only when a line does not fit, so the common case - a three
    // letter title over a wider bar - lays each line out exactly once.
    let title_galley = title_galley
        .map(|title| elide_to_width(painter, title, &title_font, TITLE_COLOR, column_width));
    let unit_galley = unit_galley
        .map(|unit| elide_to_width(painter, unit, &label_font, MUTED_COLOR, column_width));

    let wanted = drawn_badges.len();
    let mut rows_left = max_badge_rows;
    let mut badge_galleys: Vec<Arc<egui::Galley>> = Vec::with_capacity(wanted);
    for (index, badge) in drawn_badges.into_iter().enumerate() {
        // Every badge is guaranteed one row, so a sentence in the first badge
        // can never silence the ones under it. "PARTIAL" - this frame is half a
        // volume - has to survive a hail environment sentence stacked above it.
        let still_to_come = wanted - index - 1;
        let allowance = rows_left.saturating_sub(still_to_come).max(1);
        let galley = laid_out(
            painter,
            badge.clone(),
            &badge_font,
            BADGE_COLOR,
            column_width,
            allowance,
        );
        rows_left = rows_left.saturating_sub(galley.rows.len());
        badge_galleys.push(galley);
    }

    MeasuredLegend {
        column_width,
        labels,
        title: title_galley,
        badges: badge_galleys,
        unit: unit_galley,
    }
}

/// The widest single WORD across the badges that will be drawn, in `font`.
///
/// The floor a wrapped badge needs, and the only width a badge may claim.
/// Wrapping is what stops a sentence-length badge from widening the column, but
/// wrapping into a column narrower than one word breaks that word mid-letter -
/// "PARTIA" over "L" - and a mangled qualifier reads as a different qualifier.
/// Splitting on whitespace rather than measuring the whole string is the entire
/// difference between a 38-point column and a 120-point one for the hail
/// products. Answers 0 for no badges, which lifts nothing.
fn widest_badge_word(painter: &egui::Painter, badges: &[&String], font: &egui::FontId) -> f32 {
    badges
        .iter()
        .flat_map(|badge| badge.split_whitespace())
        .map(|word| {
            painter
                .layout_no_wrap(word.to_owned(), font.clone(), BADGE_COLOR)
                .size()
                .x
        })
        .fold(0.0_f32, f32::max)
}

/// Hand `galley` back, or lay its text out again to fit `max_width`.
///
/// Only re-laid out when it does not fit, so the common case - a three letter
/// title over a wider bar - is laid out exactly once.
fn elide_to_width(
    painter: &egui::Painter,
    galley: Arc<egui::Galley>,
    font: &egui::FontId,
    color: egui::Color32,
    max_width: f32,
) -> Arc<egui::Galley> {
    if galley.size().x <= max_width {
        return galley;
    }
    laid_out(painter, galley.text().to_owned(), font, color, max_width, 1)
}

/// Lay `text` out into `max_width` over at most `max_rows` rows, with a visible
/// ellipsis when it still does not fit.
///
/// One row means elide, with `break_anywhere` as `epaint` recommends there.
/// More than one wraps between words where it can, so "ASSUMED ENV" breaks
/// after "ASSUMED" rather than after "ASSU"; a word longer than the column is
/// broken anyway, because `epaint` must put it somewhere, and
/// [`widest_badge_word`] is what keeps that off every badge `app.rs` sends. The
/// rows of a wrapped badge are left-aligned in a box whose right edge is on the
/// column, which is how wrapped text reads - right-aligning each row needs
/// `LayoutJob::halign`, which moves the galley origin [`paint_right_aligned`]
/// places every other line by.
fn laid_out(
    painter: &egui::Painter,
    text: String,
    font: &egui::FontId,
    color: egui::Color32,
    max_width: f32,
    max_rows: usize,
) -> Arc<egui::Galley> {
    let mut job = egui::text::LayoutJob::simple(text, font.clone(), color, max_width);
    job.wrap.max_rows = max_rows;
    job.wrap.break_anywhere = max_rows == 1;
    job.wrap.overflow_character = Some('\u{2026}');
    painter.layout_job(job)
}

/// Where every part of one legend goes inside `pane_rect`, or `None` when there
/// is no room for it.
///
/// `None` for the two reasons the fixed-width version had, both now checked
/// against the MEASURED width:
///
/// 1. The pane is too narrow. The legend must never be the reason an analyst
///    cannot see the storm, so at least [`MIN_DATA_WIDTH`] of pane has to be
///    left over. Measuring is what lets a narrow bar keep its legend on a pane
///    that a fixed 54 points would have refused outright: a correlation
///    coefficient legend measures 37.06, so it fits a pane 17 points narrower
///    than the old rule allowed.
/// 2. The bar would be shorter than [`MIN_BAR_HEIGHT`], which cannot carry two
///    readable labels. An unreadable legend is worse than no legend: it still
///    covers the radar underneath it.
pub fn legend_geometry(
    painter: &egui::Painter,
    pane_rect: egui::Rect,
    layout: &LegendLayout,
    title: &str,
    badges: &[String],
) -> Option<LegendGeometry> {
    // Rejected first: every guard below is a `<` against a width and every `<`
    // against a NaN is false, so a non-finite pane rect passed BOTH and gave a
    // geometry of NaN rects. `egui` does not reject those either - it queues
    // NaN quads and NaN galley positions, which cost a frame and paint nothing.
    // `Rect::NOTHING` is egui's empty rect, infinite rather than zero-sized, so
    // it lands here too.
    if !pane_rect.is_finite() {
        return None;
    }

    let mut measured = measure_legend(painter, layout, title, badges, MAX_BADGE_ROWS);

    // The column's width does not depend on how many rows the badges take -
    // `measure_legend` fixes it before it wraps anything - so this guard is
    // asked once.
    if pane_rect.width() < measured.column_width + EDGE_MARGIN * 2.0 + MIN_DATA_WIDTH {
        return None;
    }

    let column = egui::Rect::from_min_max(
        egui::pos2(
            pane_rect.right() - EDGE_MARGIN - measured.column_width,
            pane_rect.top() + TOP_MARGIN,
        ),
        egui::pos2(
            pane_rect.right() - EDGE_MARGIN,
            pane_rect.bottom() - BOTTOM_MARGIN,
        ),
    );

    let bar_in = |measured: &MeasuredLegend| {
        egui::Rect::from_min_max(
            egui::pos2(
                column.right() - BAR_WIDTH,
                column.top() + measured.header_height() + BAR_GAP,
            ),
            egui::pos2(column.right(), column.bottom() - measured.unit_height()),
        )
    };

    // A wrapped badge is worth several rows of header, and on a short pane
    // those rows come out of the bar. Spending fewer of them is better than
    // refusing the legend: the ladder is the part an analyst cannot
    // reconstruct, and a badge cut short still carries the ellipsis that says
    // so. One row per badge is the floor, which is the header the fixed line
    // pitch reserved, so this can only draw where the old rule drew.
    let mut bar = bar_in(&measured);
    let mut budget = MAX_BADGE_ROWS;
    while bar.height() < MIN_BAR_HEIGHT && budget > 1 {
        budget -= 1;
        measured = measure_legend(painter, layout, title, badges, budget);
        bar = bar_in(&measured);
    }
    if bar.height() < MIN_BAR_HEIGHT {
        return None;
    }

    Some(LegendGeometry {
        panel: column.expand(PANEL_PAD),
        column,
        bar,
        label_right: bar.left() - TICK_MARK - LABEL_GAP,
        labels: measured.labels,
        title: measured.title,
        badges: measured.badges,
        unit: measured.unit,
    })
}

/// Paint the bar inside the pane's right edge.
///
/// Draws into the painter it is given and claims no window, area, or layer of
/// its own, so it composes over the radar raster the pane already painted. The
/// bottom left of the pane is left alone: the cursor readout lives there.
///
/// Every position comes from [`legend_geometry`]; nothing is decided here.
///
/// The range-folded colour is deliberately not shown. It is a category, not a
/// value - `render2d` selects it from the folded code in the moment data before
/// any value conversion happens - and this signature carries nothing to say
/// whether the product on screen can fold at all, so a swatch drawn here would
/// claim that reflectivity folds to purple.
pub fn draw_legend(
    painter: &egui::Painter,
    pane_rect: egui::Rect,
    layout: &LegendLayout,
    table: &ColorTable,
    title: &str,
    badges: &[String],
) {
    let Some(geometry) = legend_geometry(painter, pane_rect, layout, title, badges) else {
        return;
    };

    // Clipped to the pane so a long tick label - "-1000" on a shear product -
    // cannot reach into the pane next door.
    let painter = painter.with_clip_rect(pane_rect.intersect(painter.clip_rect()));

    painter.rect_filled(geometry.panel, 3.0, panel_color());

    let right = geometry.column.right();
    let mut line_top = geometry.column.top();
    if let Some(title) = &geometry.title {
        line_top += paint_right_aligned(&painter, right, line_top, title, TITLE_COLOR);
    }
    for badge in &geometry.badges {
        line_top += paint_right_aligned(&painter, right, line_top, badge, BADGE_COLOR);
    }

    draw_gradient(&painter, geometry.bar, layout.span, table);
    painter.rect_stroke(
        geometry.bar,
        0.0,
        egui::Stroke::new(1.0, BAR_OUTLINE_COLOR),
        egui::StrokeKind::Outside,
    );

    for (index, tick) in layout.ticks.iter().enumerate() {
        // `legend_layout` cannot produce this - `bar_fraction` clamps to 0..1
        // and a non-finite engine value is dropped before a tick is built - but
        // this signature takes a `LegendLayout` from anywhere. A NaN fraction
        // is not a panic, which is why it would ship: it is a tick mark and a
        // label at a NaN y, silently missing from a bar that still looks whole.
        if !tick.fraction.is_finite() {
            continue;
        }
        let y = geometry.bar.bottom() - tick.fraction * geometry.bar.height();
        painter.line_segment(
            [
                egui::pos2(geometry.bar.left() - TICK_MARK, y),
                egui::pos2(geometry.bar.left(), y),
            ],
            egui::Stroke::new(1.0, MUTED_COLOR),
        );
        // `layout.ticks` and `geometry.labels` are built from the same slice in
        // the same order, so this is a total lookup; the `let else` is what
        // makes that a missing label rather than a panic if it ever stops being
        // true.
        let (Some(galley), Some((left, _))) =
            (geometry.labels.get(index), geometry.label_x_span(index))
        else {
            continue;
        };
        // Vertically centred on the tick, which is what `Align2::RIGHT_CENTER`
        // did before the galleys were measured up front.
        painter.galley(
            egui::pos2(left, y - galley.size().y * 0.5),
            galley.clone(),
            LABEL_COLOR,
        );
    }

    if let Some(unit) = &geometry.unit {
        // The same gap `legend_geometry` reserved under the bar, so the unit
        // ends exactly on the column's bottom edge rather than one point past
        // it and three points inside the panel's rounded corner.
        paint_right_aligned(
            &painter,
            right,
            geometry.bar.bottom() + UNIT_GAP,
            unit,
            MUTED_COLOR,
        );
    }
}

/// Paint one already-laid-out line with its right edge on `right` and its top
/// on `top`, and answer its height so the caller can stack the next line under
/// it.
///
/// That is what the `Align2::RIGHT_TOP` calls this replaced did, position for
/// position: `painter.text` lays the text out and then offsets it by the
/// galley's own size, which is exactly the offset applied here.
fn paint_right_aligned(
    painter: &egui::Painter,
    right: f32,
    top: f32,
    galley: &Arc<egui::Galley>,
    color: egui::Color32,
) -> f32 {
    painter.galley(
        egui::pos2(right - galley.size().x, top),
        galley.clone(),
        color,
    );
    galley.size().y
}

/// The bar itself: one palette sample per physical pixel row.
///
/// Sampled at the centre of the value band a row covers, not at its edge. A
/// stepped palette changes colour at a stop, and sampling at the row's top edge
/// puts every band half a row too high - invisible on screen, and wrong in a
/// screenshot measured against the labels.
fn draw_gradient(painter: &egui::Painter, bar: egui::Rect, span: ValueRange, table: &ColorTable) {
    let pixels_per_point = painter.pixels_per_point().max(1.0);
    let rows = ((bar.height() * pixels_per_point).round() as usize).clamp(1, MAX_GRADIENT_ROWS);
    let rows_f32 = rows as f32;

    let mut mesh = egui::Mesh::default();
    for row in 0..rows {
        let row_f32 = row as f32;
        // Rows are placed by interpolating the bar's own edges rather than by
        // accumulating a row height, so rounding cannot leave a hairline gap
        // between two rows or overshoot the bottom of the bar.
        let top = bar.top() + bar.height() * (row_f32 / rows_f32);
        let bottom = bar.top() + bar.height() * ((row_f32 + 1.0) / rows_f32);
        let fraction = 1.0 - (row_f32 + 0.5) / rows_f32;
        let engine_value = span.min + fraction * (span.max - span.min);
        mesh.add_colored_rect(
            egui::Rect::from_min_max(egui::pos2(bar.left(), top), egui::pos2(bar.right(), bottom)),
            to_color32(table.sample(engine_value)),
        );
    }
    painter.add(egui::Shape::mesh(mesh));
}

fn to_color32(color: Rgba8) -> egui::Color32 {
    egui::Color32::from_rgba_unmultiplied(color.r, color.g, color.b, color.a)
}

#[cfg(test)]
mod tests {
    use super::*;

    use color_tables::{ColorStop, ColorTableSet};
    use product_engine::domain::{PlausibleRange, TickHint};
    use product_engine::registry::ProductRegistry;
    use product_engine::units::{
        AffineTransform, DisplayUnit, METERS_PER_SECOND_TO_KNOTS, METERS_TO_KILOFEET, PhysicalUnit,
    };
    use render2d::color_family_for_moment;

    const BUILTIN_IDS: [&str; 10] = [
        "REF", "VEL", "DVEL", "SRV", "DSRV", "SW", "ZDR", "RHO", "PHI", "KDP",
    ];

    fn opaque_stop(value: f32) -> ColorStop {
        ColorStop {
            value,
            color: Rgba8::opaque(200, 200, 200),
            end_color: None,
        }
    }

    fn clear_stop(value: f32) -> ColorStop {
        ColorStop {
            value,
            color: Rgba8::TRANSPARENT,
            end_color: None,
        }
    }

    fn test_table(stops: Vec<ColorStop>) -> ColorTable {
        ColorTable::new("test", stops).expect("test palette is valid")
    }

    fn labels(layout: &LegendLayout) -> Vec<&str> {
        layout
            .ticks
            .iter()
            .map(|tick| tick.label.as_str())
            .collect()
    }

    fn builtin_domain(id: &str) -> DisplayDomain {
        ProductRegistry::builtin()
            .get(id)
            .unwrap_or_else(|| panic!("{id} is a built-in product"))
            .domain
    }

    /// The palette the application would actually pair with this product.
    fn builtin_table_for(id: &str) -> ColorTable {
        let descriptor = ProductRegistry::builtin()
            .get(id)
            .unwrap_or_else(|| panic!("{id} is a built-in product"));
        ColorTableSet::default()
            .for_family(color_family_for_moment(
                &descriptor.computation.source_moment(),
            ))
            .clone()
    }

    /// An echo top domain: stored in metres, read in kilofeet. There is no echo
    /// top product in the registry yet; this is the domain one will carry, and
    /// it is here because metres-to-kilofeet is the conversion that makes the
    /// engine/display split visible.
    fn echo_top_domain() -> DisplayDomain {
        DisplayDomain {
            engine_unit: PhysicalUnit::Meters,
            display_unit: DisplayUnit::Kilofeet,
            display_from_engine: AffineTransform::scaled(METERS_TO_KILOFEET),
            declared_engine_range: ValueRange::new(0.0, 12_000.0),
            plausible: PlausibleRange::new(0.0, 22_000.0, 0.0, 30_000.0),
            tick_hint: TickHint::DEFAULT,
            decimals: 1,
        }
    }

    #[test]
    fn the_reflectivity_bar_starts_at_the_palettes_first_opaque_stop_not_at_the_declared_domain() {
        let domain = builtin_domain("REF");
        assert_eq!(
            domain.declared_engine_range,
            ValueRange::new(-32.0, 94.5),
            "the domain still declares the whole 8-bit encoding"
        );

        let layout = legend_layout(&domain, &builtin_table_for("REF"))
            .expect("reflectivity has a drawable legend");

        // AWIPS Wilson fades in from -30 dBZ and turns fully opaque at -20;
        // the bar starts where the paint is solid, not at -32.
        //
        // -20.01 until the parser learned to read GR two-colour ramp rows.
        // The palette's -30 row declares its fade in one row, so the shipped
        // text no longer carries a hand-expanded second stop parked 0.01 dBZ
        // below the -20 row, and the first solid value is the declared one.
        assert_eq!(
            layout.span,
            ValueRange::new(-20.0, 94.5),
            "the bar must start at the palette's first opaque stop"
        );
        assert_eq!(layout.unit_label, "dBZ");
        assert_eq!(labels(&layout), vec!["-20", "0", "20", "40", "60", "80"]);
    }

    #[test]
    fn an_echo_top_bar_is_labelled_in_kilofeet_while_its_span_stays_in_metres() {
        let domain = echo_top_domain();
        let layout = legend_layout(
            &domain,
            &test_table(vec![opaque_stop(0.0), opaque_stop(12_000.0)]),
        )
        .expect("an echo top palette in metres has a drawable legend");

        assert_eq!(
            layout.span,
            ValueRange::new(0.0, 12_000.0),
            "the span is what a colour lookup is fed, so it stays in metres"
        );
        assert_eq!(layout.unit_label, "kft");
        assert_eq!(
            domain.format_display(layout.span.max),
            "39.4 kft",
            "the top of the bar is 12 000 m, which an analyst reads as 39.4 kft"
        );
        assert_eq!(
            labels(&layout),
            vec!["0", "5", "10", "15", "20", "25", "30", "35"]
        );

        let top = layout.ticks.last().expect("the ladder is not empty");
        assert_eq!(
            top.engine_value, 10_668.0,
            "35 kft is exactly 35 * 304.8 m, and that metre value is what the palette is \
             sampled with"
        );
        assert!(
            (top.fraction - 0.889).abs() < 1e-3,
            "35 kft sits at 10 668 / 12 000 of the bar, got {}",
            top.fraction
        );
    }

    #[test]
    fn a_velocity_bar_is_labelled_in_knots_while_its_span_stays_in_metres_per_second() {
        let domain = builtin_domain("VEL");
        let layout =
            legend_layout(&domain, &builtin_table_for("VEL")).expect("velocity has a legend");

        assert_eq!(
            layout.span,
            ValueRange::new(-64.0, 64.0),
            "the encoded velocity domain is narrower than the palette, and it is metres per \
             second"
        );
        assert_eq!(layout.unit_label, "kt");
        assert_eq!(labels(&layout), vec!["-100", "-50", "0", "50", "100"]);

        let top = layout.ticks.last().expect("the ladder is not empty");
        assert!(
            (f64::from(top.engine_value) - 100.0 / METERS_PER_SECOND_TO_KNOTS).abs() < 1e-3,
            "100 kt is 51.444 m/s, and 51.444 is what the colour table must be sampled with; \
             sampling it with 100 would read 1.94 times too fast, got {}",
            top.engine_value
        );

        let zero = &layout.ticks[2];
        assert_eq!(zero.engine_value, 0.0);
        assert!(
            (zero.fraction - 0.5).abs() < 1e-6,
            "a symmetric velocity domain puts zero at the middle of the bar, got {}",
            zero.fraction
        );
    }

    #[test]
    fn a_correlation_coefficient_bar_ticks_every_two_tenths_across_its_domain() {
        let domain = builtin_domain("RHO");
        let layout = legend_layout(&domain, &builtin_table_for("RHO"))
            .expect("correlation coefficient has a legend");

        // The bounds are the DECODED endpoints of the field, not round numbers:
        // rho-HV arrives as `(raw + 60.5) / 300`, so the lowest and highest
        // codes are 62.5/300 and 315.5/300. Written as those quotients
        // deliberately - a bar that stopped at a tidy 1.05 left the saturation
        // code above its last stop, and on real volumes 4 to 10 percent of all
        // gates clamped there and were painted the brightest colour on the
        // scope, which is noise outshining weather.
        assert_eq!(layout.span, ValueRange::new(62.5 / 300.0, 315.5 / 300.0));
        assert_eq!(layout.unit_label, "", "a ratio carries no unit");
        assert_eq!(
            labels(&layout),
            vec!["0.4", "0.6", "0.8", "1.0"],
            "the domain asks for three decimals so a probe can read 0.847; a bar labelled \
             0.400 0.600 spends three characters per label saying nothing"
        );

        // There is deliberately no 0.2 tick. The bar now starts at the field's
        // own decoded floor, 62.5/300 = 0.2083, so a tick at 0.2 would sit
        // BELOW the bar it is labelling.
        let bottom = &layout.ticks[0];
        assert!(
            bottom.fraction > 0.0,
            "0.4 is inside the bar, not its floor, got {}",
            bottom.fraction
        );
        let top = layout.ticks.last().expect("the ladder is not empty");
        assert!(
            // (1.0 - 62.5/300) / (315.5/300 - 62.5/300) = 0.791667 / 0.843333.
            (top.fraction - 0.938_735).abs() < 1e-4,
            "1.0 sits at (1.0 - 0.2083) / (1.0517 - 0.2083) of the bar, got {}",
            top.fraction
        );
    }

    #[test]
    fn an_all_transparent_palette_has_no_legend() {
        let domain = builtin_domain("REF");
        let invisible = test_table(vec![clear_stop(-32.0), clear_stop(94.5)]);
        assert_eq!(legend_span(&domain, &invisible), None);
        assert_eq!(
            legend_layout(&domain, &invisible),
            None,
            "a palette that can never ink anything must produce no bar at all"
        );
    }

    #[test]
    fn a_palette_whose_ink_misses_the_domain_has_no_legend() {
        // A reflectivity palette left selected on a correlation coefficient
        // pane. Every value in 0.2..1.05 samples the palette's last stop, so
        // drawing the declared domain would give a bar of one flat colour under
        // a full ladder of labels - which reads as a working legend.
        let domain = builtin_domain("RHO");
        let wrong_family = test_table(vec![opaque_stop(-32.0), opaque_stop(-10.0)]);
        assert_eq!(legend_span(&domain, &wrong_family), None);
        assert_eq!(legend_layout(&domain, &wrong_family), None);
    }

    /// The other half of the fix in `ColorTable::inked_value_span`: a `.pal`
    /// whose only ink is a declared fade gets a bar, because it paints.
    ///
    /// The palette is two clear rows, the lower one declaring an opaque second
    /// colour, and a `step: 5` row on top. It paints a thousand of the 2,501
    /// values on a 0.01 grid from -5 to 20 - `sample(2.5)` is `[80,0,0,102]`
    /// and `sample(7.5)` is `[160,0,0,204]` - and it used to report its ink as
    /// the single value 12.5, the clear stop the fade arrives on and the one
    /// value in the interval it does not paint. That went straight through
    /// `legend_span`'s zero-width guard, so the pane drew a palette an analyst
    /// could see with no bar to read it by.
    #[test]
    fn a_palette_whose_only_ink_is_a_declared_fade_gets_a_legend() {
        let domain = builtin_domain("REF");
        let fading = ColorTable::parse(
            "fade only",
            "product: BR\nstep: 5\ncolor4: 0 0 0 0 0 200 0 0 255\ncolor4: 12.5 0 0 0 0\n",
        )
        .expect("palette parses");
        assert_eq!(fading.sample(2.5).to_array(), [80, 0, 0, 102]);
        assert_eq!(fading.inked_value_span(), Some((0.0, 12.5)));

        let span = legend_span(&domain, &fading).expect("the palette paints, so it gets a bar");
        assert_eq!((span.min, span.max), (0.0, 12.5));
        let layout = legend_layout(&domain, &fading).expect("and a ladder to read it by");
        assert!(
            layout.ticks.len() >= MIN_LEGEND_TICKS,
            "a bar over 12.5 dBZ should carry a readable ladder, got {:?}",
            labels(&layout)
        );
    }

    #[test]
    fn a_palette_that_inks_a_single_value_has_no_legend() {
        // `ColorTable::inked_value_span` documents this shape: one transparent
        // stop and one opaque stop report a zero-width span. Placing a tick on
        // a bar with no extent divides by zero and every fraction comes back
        // NaN, so the bar is refused rather than drawn.
        let domain = builtin_domain("SW");
        let single = test_table(vec![clear_stop(0.0), opaque_stop(10.0)]);
        assert_eq!(single.inked_value_span(), Some((10.0, 10.0)));
        assert_eq!(legend_span(&domain, &single), None);
        assert_eq!(legend_layout(&domain, &single), None);
    }

    #[test]
    fn every_tick_fraction_lies_on_the_bar_and_rises_with_its_value() {
        for id in BUILTIN_IDS {
            let layout = legend_layout(&builtin_domain(id), &builtin_table_for(id))
                .unwrap_or_else(|| panic!("{id} has a drawable legend"));
            let mut previous: Option<&LegendTick> = None;
            for tick in &layout.ticks {
                assert!(
                    (0.0..=1.0).contains(&tick.fraction),
                    "{id} placed {} at {}, which is off the bar",
                    tick.label,
                    tick.fraction
                );
                if let Some(previous) = previous {
                    assert!(
                        tick.engine_value > previous.engine_value,
                        "{id} ladder does not ascend: {} then {}",
                        previous.label,
                        tick.label
                    );
                    assert!(
                        tick.fraction > previous.fraction,
                        "{id} put {} at or below {} on the bar",
                        tick.label,
                        previous.label
                    );
                }
                previous = Some(tick);
            }
        }
    }

    #[test]
    fn the_ends_of_a_bar_are_fraction_zero_and_fraction_one() {
        let span = ValueRange::new(-64.0, 64.0);
        assert_eq!(bar_fraction(span, -64.0), 0.0);
        assert_eq!(bar_fraction(span, 64.0), 1.0);
        assert_eq!(bar_fraction(span, 0.0), 0.5);
        assert_eq!(
            bar_fraction(span, 70.0),
            1.0,
            "a value past the top of the bar is clamped onto it, never drawn above it"
        );
    }

    #[test]
    fn every_builtin_product_keeps_at_least_two_ticks() {
        for descriptor in ProductRegistry::builtin().all() {
            // Through the palette module, which is what the pane uses. Going
            // via the colour FAMILY instead would hand a volume product the
            // reflectivity ramp, and a domain in metres or kilograms cannot
            // intersect a span in dBZ - the legend would be undrawable for a
            // reason that has nothing to do with the legend.
            let table = crate::palettes::table_for(descriptor, &ColorTableSet::default());
            let layout = legend_layout(&descriptor.domain, &table)
                .unwrap_or_else(|| panic!("{} has no drawable legend", descriptor.id.0));
            assert!(
                layout.ticks.len() >= MIN_LEGEND_TICKS,
                "{} kept only {} tick(s); a bar with one label is not a scale",
                descriptor.id.0,
                layout.ticks.len()
            );
            assert!(
                layout.ticks.len() <= MAX_LEGEND_TICKS,
                "{} kept {} ticks, which will not fit the shortest bar drawn",
                descriptor.id.0,
                layout.ticks.len()
            );
        }
    }

    #[test]
    fn a_crowded_ladder_is_thinned_by_dropping_every_other_tick() {
        let domain = DisplayDomain {
            engine_unit: PhysicalUnit::Percent,
            display_unit: DisplayUnit::Percent,
            display_from_engine: AffineTransform::IDENTITY,
            declared_engine_range: ValueRange::new(0.0, 100.0),
            plausible: PlausibleRange::new(0.0, 100.0, 0.0, 100.0),
            // Twenty intervals asks for twenty-one labels on a bar that can
            // read eight.
            tick_hint: TickHint::intervals(20),
            decimals: 0,
        };
        let layout = legend_layout(
            &domain,
            &test_table(vec![opaque_stop(0.0), opaque_stop(100.0)]),
        )
        .expect("a percent domain has a legend");

        assert_eq!(
            labels(&layout),
            vec!["0", "20", "40", "60", "80", "100"],
            "21 ticks halve to 11 and then to 6, keeping both ends and the even spacing"
        );
    }

    #[test]
    fn a_domain_whose_display_transform_cannot_be_inverted_yields_a_bar_with_no_ladder() {
        // A zero scale collapses every value onto one display number. The bar
        // is still honest - the colours are real - but there is no position on
        // it a label could mean, and the ladder must come back empty rather
        // than as a column of identical labels stacked on one pixel.
        let domain = DisplayDomain {
            engine_unit: PhysicalUnit::Dbz,
            display_unit: DisplayUnit::Dbz,
            display_from_engine: AffineTransform::scaled(0.0),
            declared_engine_range: ValueRange::new(0.0, 50.0),
            plausible: PlausibleRange::new(0.0, 50.0, 0.0, 50.0),
            tick_hint: TickHint::DEFAULT,
            decimals: 1,
        };
        assert!(!domain.display_from_engine.is_invertible());
        let layout = legend_layout(
            &domain,
            &test_table(vec![opaque_stop(0.0), opaque_stop(50.0)]),
        )
        .expect("the span itself is still drawable");
        assert_eq!(layout.span, ValueRange::new(0.0, 50.0));
        assert!(layout.ticks.is_empty());
    }

    // ---------------------------------------------------------------------
    // Width. Everything below drives real `egui` text layout, because a width
    // measured any other way is the defect these tests exist to catch.
    // ---------------------------------------------------------------------

    /// The width every legend was drawn at before it was measured. Kept here,
    /// and nowhere else, so the tests that say "narrower than it used to be"
    /// can say what "used to be" was.
    const OLD_FIXED_WIDTH: f32 = 54.0;

    /// A pane `width` points across and tall enough for any bar this file will
    /// draw: 600 points leaves a bar of 543, well past [`MIN_BAR_HEIGHT`].
    fn pane(width: f32) -> egui::Rect {
        egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(width, 600.0))
    }

    /// Lay a legend out through a real `egui` context, so every width in the
    /// answer came from the font the glyphs are painted with.
    fn geometry_in(
        pane_rect: egui::Rect,
        layout: &LegendLayout,
        title: &str,
        badges: &[String],
    ) -> Option<LegendGeometry> {
        let ctx = egui::Context::default();
        let raw = egui::RawInput {
            screen_rect: Some(pane_rect),
            ..Default::default()
        };
        let mut geometry = None;
        // `run_ui` may run more than one pass; the last one is the one that
        // would have been painted, and it is the one kept.
        let _ = ctx.run_ui(raw, |ui| {
            geometry = legend_geometry(ui.painter(), pane_rect, layout, title, badges);
        });
        geometry
    }

    /// The width `text` lays out to in `font`, measured the same way the legend
    /// measures it.
    fn text_width(text: &str, font: egui::FontId) -> f32 {
        let ctx = egui::Context::default();
        let raw = egui::RawInput {
            screen_rect: Some(pane(600.0)),
            ..Default::default()
        };
        let mut width = 0.0;
        let _ = ctx.run_ui(raw, |ui| {
            width = ui
                .painter()
                .layout_no_wrap(text.to_owned(), font.clone(), egui::Color32::WHITE)
                .size()
                .x;
        });
        width
    }

    fn label_width(text: &str) -> f32 {
        text_width(text, egui::FontId::monospace(LABEL_FONT_SIZE))
    }

    /// A bar carrying exactly these tick labels and no unit.
    ///
    /// The tick values are irrelevant to width; they are only kept ordered and
    /// on 0..1 so the layout is one [`legend_layout`] could have produced.
    fn bar_labelled(labels: &[&str]) -> LegendLayout {
        bar_labelled_in(labels, "")
    }

    fn bar_labelled_in(labels: &[&str], unit: &'static str) -> LegendLayout {
        let last = labels.len().saturating_sub(1).max(1) as f32;
        LegendLayout {
            span: ValueRange::new(0.0, 1.0),
            ticks: labels
                .iter()
                .enumerate()
                .map(|(index, label)| LegendTick {
                    engine_value: index as f32 / last,
                    label: (*label).to_owned(),
                    fraction: index as f32 / last,
                })
                .collect(),
            unit_label: unit,
        }
    }

    /// The badge `app.rs` builds for every hail product computed from a guessed
    /// freezing level: `HailEnvironment::summary()`, verbatim.
    ///
    /// A sentence rather than a badge, and the input that this file gets wrong
    /// most easily. It lays out at 211 points of 9-point monospace, so any rule
    /// that lets a badge set the column width hands MESH, POH and POSH a legend
    /// wider than the fixed one this file replaced.
    const HAIL_SUMMARY: &str = "ASSUMED ENV H0 3.0 km / H-20 6.0 km ARL";

    /// The gate filter's badge, verbatim from `gate_filter_ui::pane_badge_text`.
    ///
    /// One word on purpose, and this file is the reason: the version that
    /// carried the whole criteria summary wrapped to four rows in a 40-point
    /// column, took the badge stack's entire row budget and pushed the colour
    /// bar sixty points down the pane. The full statement is on the FILTERED
    /// band `pane_canvas` draws under the header, which has the pane's width
    /// to say it in.
    const FILTERED: &str = crate::gate_filter_ui::FILTERED_WORD;

    /// Every badge stack `app.rs` can hand a pane, in the order `pane_badges`
    /// pushes them: the stall notice, then the gate filter, then the frame
    /// stage when the frame is not `Complete`, then the hail environment
    /// summary for MESH, POH and POSH.
    fn app_badge_stacks() -> Vec<Vec<String>> {
        vec![
            Vec::new(),
            vec!["PARTIAL".to_owned()],
            vec!["PREVIEW".to_owned()],
            vec![HAIL_SUMMARY.to_owned()],
            vec!["PARTIAL".to_owned(), HAIL_SUMMARY.to_owned()],
            vec![FILTERED.to_owned()],
            vec![FILTERED.to_owned(), "PARTIAL".to_owned()],
            vec![FILTERED.to_owned(), HAIL_SUMMARY.to_owned()],
            // The deepest stack the application can build: a stalled feed, a
            // filter, a partial frame and a guessed hail environment.
            vec![
                "FEED STALLED · 12 h OLD".to_owned(),
                FILTERED.to_owned(),
                "PARTIAL".to_owned(),
                HAIL_SUMMARY.to_owned(),
            ],
        ]
    }

    /// The characters of a laid-out badge that are actually on the screen, wrap
    /// points and ellipsis taken back out. Read from the galley's rows, so a
    /// test asking whether "ASSUMED ENV" survived asks about glyphs rather than
    /// about the string it was built from.
    fn badge_text(galley: &Arc<egui::Galley>) -> String {
        galley
            .rows
            .iter()
            .map(|row| row.row.text())
            .collect::<Vec<String>>()
            .join("")
            .replace('\u{2026}', "")
            .split_whitespace()
            .collect::<Vec<&str>>()
            .join(" ")
    }

    /// Run `body` in a real `egui` frame with a painter clipped to `pane_rect`,
    /// so [`draw_legend`] itself can be driven, not only its geometry.
    fn with_painter<R>(pane_rect: egui::Rect, body: impl FnOnce(&egui::Painter) -> R) -> R {
        let ctx = egui::Context::default();
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::pos2(0.0, 0.0),
                egui::vec2(4000.0, 4000.0),
            )),
            ..Default::default()
        };
        let mut out = None;
        let mut body = Some(body);
        let _ = ctx.run_ui(raw, |ui| {
            if let Some(body) = body.take() {
                out = Some(body(&ui.painter_at(pane_rect)));
            }
        });
        out.expect("the ui closure ran")
    }

    /// The rectangle every shape [`draw_legend`] actually queued falls inside:
    /// the only measurement here taken from the paint list rather than from the
    /// geometry, and what makes "the legend is narrower" a claim about the
    /// screen. Each shape is intersected with its own clip rect first, as the
    /// tessellator will. `None` when the legend drew nothing at all.
    fn painted_shapes(
        pane_rect: egui::Rect,
        layout: &LegendLayout,
        table: &ColorTable,
        title: &str,
        badges: &[String],
    ) -> Vec<egui::epaint::ClippedShape> {
        let ctx = egui::Context::default();
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::pos2(0.0, 0.0),
                egui::vec2(4000.0, 4000.0),
            )),
            ..Default::default()
        };
        ctx.run_ui(raw, |ui| {
            draw_legend(
                &ui.painter_at(pane_rect),
                pane_rect,
                layout,
                table,
                title,
                badges,
            );
        })
        .shapes
    }

    fn painted_bounds(
        pane_rect: egui::Rect,
        layout: &LegendLayout,
        table: &ColorTable,
        title: &str,
        badges: &[String],
    ) -> Option<egui::Rect> {
        painted_shapes(pane_rect, layout, table, title, badges)
            .iter()
            .filter_map(|clipped| {
                let bounds = clipped.shape.visual_bounding_rect();
                bounds
                    .is_positive()
                    .then(|| bounds.intersect(clipped.clip_rect))
            })
            .filter(|bounds| bounds.is_positive())
            .reduce(|left, right| left.union(right))
    }

    /// Every built-in product paired with the palette the pane draws it with,
    /// skipping the ones whose palette leaves no honest bar.
    fn builtin_legends() -> Vec<(&'static str, LegendLayout)> {
        ProductRegistry::builtin()
            .all()
            .iter()
            .filter_map(|descriptor| {
                let table = crate::palettes::table_for(descriptor, &ColorTableSet::default());
                legend_layout(&descriptor.domain, &table)
                    .map(|layout| (descriptor.short_name, layout))
            })
            .collect()
    }

    /// Every line the legend will paint, header and ladder alike.
    fn painted_lines(geometry: &LegendGeometry) -> Vec<Arc<egui::Galley>> {
        geometry
            .title
            .iter()
            .chain(geometry.badges.iter())
            .chain(geometry.unit.iter())
            .chain(geometry.labels.iter())
            .cloned()
            .collect()
    }

    /// A badge lifts the column by its longest WORD and wraps the rest.
    ///
    /// Two cases, and both were wrong before. "ASSUMED ENV" is 59.6 points and
    /// used to widen the column to all of it; `HailEnvironment::summary()` is
    /// 211 and used to pin the column at the 120 point cap, which put MESH, POH
    /// and POSH behind a 128 point panel against the 62 the fixed-width legend
    /// used - on the three products whose bars need 46. Both now cost the
    /// column the width of "ASSUMED" and nothing more.
    #[test]
    fn a_badge_lifts_the_column_by_its_longest_word_and_wraps_the_rest() {
        let badge_font = egui::FontId::monospace(BADGE_FONT_SIZE);
        let longest_word = text_width("ASSUMED", badge_font.clone());
        let layout = bar_labelled_in(&["10", "30", "50"], "mm");
        let bare = geometry_in(pane(600.0), &layout, "MESH", &[]).expect("a bare legend fits");

        for (badge, elided, kept) in [
            ("ASSUMED ENV", false, "ASSUMED ENV"),
            (HAIL_SUMMARY, true, "ASSUMED ENV"),
        ] {
            let whole = text_width(badge, badge_font.clone());
            assert!(
                whole > longest_word && longest_word > bare.column.width(),
                "this case only means something if the badge does not already fit: whole {whole}, \
                 longest word {longest_word}, bar block {}",
                bare.column.width()
            );

            let geometry = geometry_in(pane(600.0), &layout, "MESH", &[badge.to_owned()])
                .expect("a 600 point pane holds a legend");
            // Hand-computed from the width rule: the LIFT a badge is allowed is
            // its longest word, so the column is the width of "ASSUMED" - never
            // the whole badge, and never the cap.
            assert!(
                (geometry.column.width() - longest_word).abs() < 1e-3,
                "{badge:?} should cost the column its longest word ({longest_word}), got {}",
                geometry.column.width()
            );
            assert!(
                geometry.panel.width() <= OLD_FIXED_WIDTH + PANEL_PAD * 2.0,
                "{badge:?} covers {} points of radar, against the {} the fixed-width legend did",
                geometry.panel.width(),
                OLD_FIXED_WIDTH + PANEL_PAD * 2.0
            );

            // Narrower AND readable. That is the pair wrapping buys, which
            // neither widening nor one-row eliding could give alone: the part
            // that changes how the picture is READ has to survive, because a
            // hail size from a guessed freezing level is a different claim from
            // one computed from a sounding.
            let galley = &geometry.badges[0];
            assert!(
                galley.rows.len() <= MAX_BADGE_ROWS,
                "{badge:?} took {} rows of the {MAX_BADGE_ROWS} the stack has",
                galley.rows.len()
            );
            assert!(
                badge_text(galley).starts_with(kept),
                "{badge:?} lost its provenance: {:?}",
                badge_text(galley)
            );
            assert_eq!(
                galley.elided, elided,
                "{badge:?}: a qualifier cut short with no ellipsis is a claim about the data the \
                 analyst cannot see is incomplete"
            );
        }
    }

    #[test]
    fn every_badge_keeps_a_row_however_many_are_stacked() {
        // The row budget is shared out, so a sentence in the first badge must
        // not silence the ones under it. "PARTIAL" - this frame is half a
        // volume - is the one that must never vanish.
        let badges: Vec<String> = vec![
            HAIL_SUMMARY.to_owned(),
            "PARTIAL".to_owned(),
            "PREVIEW".to_owned(),
            "USER ENV".to_owned(),
        ];
        let geometry = geometry_in(pane(600.0), &bar_labelled(&["0.4", "1.0"]), "MESH", &badges)
            .expect("a 600 point pane holds a legend");

        assert_eq!(geometry.badges.len(), badges.len());
        let rows: usize = geometry.badges.iter().map(|badge| badge.rows.len()).sum();
        assert!(
            rows <= MAX_BADGE_ROWS,
            "the stack took {rows} rows, past the {MAX_BADGE_ROWS} the header reserves"
        );
        for (badge, source) in geometry.badges.iter().zip(&badges) {
            assert!(
                !badge.rows.is_empty() && !badge_text(badge).is_empty(),
                "{source:?} was laid out to nothing"
            );
        }

        // And a pane that somehow collects a dozen qualifiers drops the extras
        // rather than pushing the bar off the bottom of itself.
        let dozen: Vec<String> = (0..12).map(|index| format!("BADGE{index}")).collect();
        let crowded = geometry_in(pane(600.0), &bar_labelled(&["0.4", "1.0"]), "T", &dozen)
            .expect("a 600 point pane holds a legend");
        assert_eq!(crowded.badges.len(), MAX_BADGES);
    }

    // ---------------------------------------------------------------------
    // Everything below drives the real registry, the real palettes and real
    // `egui` font metrics rather than stand-ins, because the failures this
    // section exists to catch are ones only real metrics produce.
    // ---------------------------------------------------------------------

    /// A column is worth exactly what is in it, for all seventeen products,
    /// each drawn with the palette `palettes::table_for` pairs with it. What
    /// this run measures, in points, against the 54 they all used to get:
    ///
    /// ```text
    /// VILD                       25.03
    /// REF CREF ET18 VIL SW       31.03
    /// KDP                        36.12   (its unit, not its labels)
    /// ZDR RHO PHI MESH POH POSH  37.06
    /// VEL DVEL SRV DSRV          43.09
    /// ```
    ///
    /// RHO is the bar that motivated the change: 37.06 rather than 54, so the panel
    /// behind it is 45.06 instead of 62.0 and 17 points of radar come back. KDP
    /// proves the unit line has to be measured too - its widest tick label is
    /// "-2", a bar block of 31.03, but "deg/km" under the bar is 36.12 and that
    /// is what sets its width. Figures are from `egui` 0.34.3's embedded Hack
    /// and Ubuntu-Light, so what is asserted is the identity behind them,
    /// hand-derived from the width rule in [`measure_legend`]:
    ///
    /// ```text
    /// column = max(BAR_WIDTH + TICK_MARK + LABEL_GAP + widest tick label,
    ///              title, unit)
    /// ```
    ///
    /// Every term measured through the same layout the glyphs are painted with.
    /// The identity survives a font change and is strictly stronger than the
    /// numbers: a product a few points wide of its own contents fails here even
    /// if it stays under every bound.
    #[test]
    fn every_builtin_column_is_its_bar_block_its_title_or_its_unit_and_nothing_else() {
        for (name, layout) in builtin_legends() {
            let geometry =
                geometry_in(pane(600.0), &layout, name, &[]).expect("a 600 point pane holds it");

            let widest = layout
                .ticks
                .iter()
                .map(|tick| label_width(&tick.label))
                .fold(0.0_f32, f32::max);
            let bar_block = BAR_WIDTH + TICK_MARK + LABEL_GAP + widest;
            let title = text_width(name, egui::FontId::proportional(TITLE_FONT_SIZE));
            let unit = if layout.unit_label.is_empty() {
                0.0
            } else {
                label_width(layout.unit_label)
            };
            let expected = bar_block.max(title).max(unit);

            assert!(
                (geometry.column.width() - expected).abs() < 1e-3,
                "{name}: column {} against bar block {bar_block} / title {title} / unit {unit}",
                geometry.column.width()
            );
            assert!(
                geometry.column.width() < OLD_FIXED_WIDTH,
                "{name} measured {}, no narrower than the {OLD_FIXED_WIDTH} every legend used to \
                 be given",
                geometry.column.width()
            );
        }
    }

    /// The two ends of the registry, hand-computed rather than derived: a
    /// dimensionless ratio labelled "0.4" is the narrowest bar block the
    /// application has and a velocity labelled "-100" is the widest, and one
    /// fixed number cannot be right for both. The whole defect, in two lines of
    /// arithmetic - 12 + 4 + 3 + the label, for each.
    #[test]
    fn the_narrowest_and_widest_bars_in_the_registry_are_both_their_own_width() {
        for (title, labels, unit, widest) in [
            ("RHO", ["0.4", "0.6", "0.8", "1.0"].as_slice(), "", "0.4"),
            (
                "VEL",
                ["-100", "-50", "0", "50", "100"].as_slice(),
                "kt",
                "-100",
            ),
        ] {
            let geometry = geometry_in(pane(600.0), &bar_labelled_in(labels, unit), title, &[])
                .expect("a 600 point pane fits either");
            let expected = BAR_WIDTH + TICK_MARK + LABEL_GAP + label_width(widest);
            assert!(
                (geometry.column.width() - expected).abs() < 1e-3,
                "{title}: expected {expected} points, got {}",
                geometry.column.width()
            );
            assert!(geometry.column.width() < OLD_FIXED_WIDTH);
        }
    }

    /// A badge stack spends fewer rows rather than costing the legend.
    ///
    /// Wrapping buys width with height, and on a short pane those rows come out
    /// of the bar. Measured on MESH with the hail sentence: a four-row budget
    /// needs a 271-point pane, one row per badge needs 241, and the fixed-pitch
    /// version this replaced needed 242. Shrinking is what stops the trade
    /// costing a legend that used to be drawn.
    #[test]
    fn a_short_pane_spends_fewer_badge_rows_rather_than_losing_the_legend() {
        let layout = bar_labelled_in(&["10", "30", "50"], "mm");
        let badges = [HAIL_SUMMARY.to_owned()];
        let roomy = geometry_in(pane(600.0), &layout, "MESH", &badges).expect("a tall pane fits");
        assert_eq!(roomy.badges[0].rows.len(), MAX_BADGE_ROWS);

        let short = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(600.0, 245.0));
        let cramped = geometry_in(short, &layout, "MESH", &badges)
            .expect("245 points of pane still earns a bar rather than nothing");
        assert_eq!(
            cramped.badges[0].rows.len(),
            1,
            "the budget shrank to its floor"
        );
        assert!(cramped.bar.height() >= MIN_BAR_HEIGHT);
        assert!(
            cramped.badges[0].elided,
            "a cut badge still says it was cut"
        );
    }

    /// The legend must not cost the pane more than the fixed-width version did,
    /// for any product, with any badge stack `app.rs` can send.
    ///
    /// THIS IS THE TEST THAT WAS MISSING. Every width test here used to pass
    /// `&[]` for badges, so none measured what the application draws - and for
    /// MESH, POH and POSH that is a legend carrying `HailEnvironment::summary()`.
    /// Letting that sentence set the column width pinned those three at the 120
    /// point cap behind a 128 point panel, against the 62 the fixed 54-point
    /// column used: the defect the measuring was meant to remove, back on the
    /// products that need a small pane most.
    ///
    /// The second assertion is the one property [`LEGEND_WIDTH`]'s doc still
    /// promises. Nothing outside this file reads it - a grep finds only
    /// `legend_layout` at `app.rs:1119` and `draw_legend` at
    /// `pane_canvas.rs:230` - so this pins what a future caller could rely on:
    /// RESERVING that much always over-reserves. POSITIONING against it is what
    /// the doc forbids, and [`legend_geometry`] answers that instead.
    #[test]
    fn no_product_with_any_badge_app_rs_sends_costs_more_pane_than_the_fixed_width_did() {
        const OLD_PANEL: f32 = OLD_FIXED_WIDTH + PANEL_PAD * 2.0;
        for (name, layout) in builtin_legends() {
            for badges in app_badge_stacks() {
                let geometry = geometry_in(pane(600.0), &layout, name, &badges)
                    .expect("a 600 point pane holds it");
                assert!(
                    geometry.panel.width() <= OLD_PANEL,
                    "{name} with {badges:?} covers {} points of radar, against the {OLD_PANEL} \
                     the fixed-width legend covered",
                    geometry.panel.width()
                );
                assert!(
                    geometry.column.width() <= LEGEND_WIDTH,
                    "{name} with {badges:?} measured {}, past the {LEGEND_WIDTH} point bound",
                    geometry.column.width()
                );
            }
        }
    }

    /// Sweep every pane width and prove the legend is drawn wholly inside the
    /// pane or not at all. Half-point steps, because a pane rect comes from
    /// `egui`'s layout rather than an integer grid and the guard is a single
    /// `<`. One context for the whole sweep: one per sample rebuilds the font
    /// atlas each time and makes this a sixty second test.
    #[test]
    fn across_every_pane_width_the_legend_is_wholly_inside_the_pane_or_absent() {
        let legends = builtin_legends();
        with_painter(pane(4000.0), |painter| {
            for (name, layout) in &legends {
                for badges in app_badge_stacks() {
                    let mut narrowest: Option<f32> = None;
                    let mut width = 0.0_f32;
                    while width <= 400.0 {
                        let pane_rect = pane(width);
                        let at = format!("{name} {badges:?} on a {width} point pane");
                        let Some(found) =
                            legend_geometry(painter, pane_rect, layout, name, &badges)
                        else {
                            // Monotone in pane width, or dragging a splitter
                            // makes the legend flicker in and out.
                            assert!(narrowest.is_none(), "{at}: refused after {narrowest:?}");
                            width += 0.5;
                            continue;
                        };
                        narrowest.get_or_insert(width);
                        assert!(
                            pane_rect.contains_rect(found.panel),
                            "{at}: panel {:?} escapes the pane",
                            found.panel
                        );
                        assert!(
                            found.panel.contains_rect(found.bar),
                            "{at}: the bar escapes its own panel"
                        );
                        // Everything is right-aligned on the column, so a left
                        // edge is the only one that can escape the panel.
                        let mut leftmost = found.panel.left();
                        for galley in painted_lines(&found) {
                            leftmost = leftmost.min(found.column.right() - galley.size().x);
                        }
                        for index in 0..layout.ticks.len() {
                            let (left, right) =
                                found.label_x_span(index).expect("every tick has a label");
                            leftmost = leftmost.min(left);
                            assert!(
                                right <= found.bar.left() - TICK_MARK + 1e-3,
                                "{at}: label {index} ends at {right}, over its own tick mark"
                            );
                        }
                        assert!(
                            leftmost >= found.panel.left() - 1e-3,
                            "{at}: ink at {leftmost} is left of the panel at {}, so it is painted \
                             onto the radar with no backing",
                            found.panel.left()
                        );
                        assert!(
                            pane_rect.width() - found.column.width() - EDGE_MARGIN * 2.0
                                >= MIN_DATA_WIDTH - 1e-3,
                            "{at}: the legend ate into the {MIN_DATA_WIDTH} points held for the \
                             storm"
                        );
                        width += 0.5;
                    }
                    assert!(
                        narrowest.is_some_and(|first| first < 160.0),
                        "{name} {badges:?} was first drawn at {narrowest:?}, no better than the \
                         160 points the fixed-width rule demanded"
                    );
                }
            }
        });
    }

    /// The bar's top and bottom come from the galleys that are painted. The old
    /// fixed pitch reserved 14 points for a 13-point title, 11 for a 10-point
    /// badge and 13 for a 12-point unit; that error is harmless, which is why
    /// it survived, but with a WRAPPED badge under it the same code overlaps -
    /// a two-row badge is two rows whatever a constant says.
    #[test]
    fn the_header_reserved_above_the_bar_is_the_header_that_is_painted() {
        for badges in app_badge_stacks() {
            let layout = bar_labelled_in(&["10", "30", "50"], "mm");
            let geometry =
                geometry_in(pane(600.0), &layout, "MESH", &badges).expect("a 600 pane holds it");

            let painted: f32 = geometry
                .title
                .iter()
                .chain(geometry.badges.iter())
                .map(|galley| galley.size().y)
                .sum();
            assert!(
                (geometry.bar.top() - geometry.column.top() - painted - BAR_GAP).abs() < 1e-3,
                "{badges:?}: the bar starts {} points below the column top but the header paints \
                 {painted} plus a {BAR_GAP} point gap",
                geometry.bar.top() - geometry.column.top()
            );

            let unit = geometry.unit.as_ref().expect("mm is a unit");
            assert!(
                (geometry.bar.bottom() + UNIT_GAP + unit.size().y - geometry.column.bottom()).abs()
                    < 1e-3,
                "{badges:?}: the unit line ends {} points past the column's bottom",
                geometry.bar.bottom() + UNIT_GAP + unit.size().y - geometry.column.bottom()
            );
        }
    }

    /// Nothing a caller can pass makes the legend panic, and every case here
    /// goes through `draw_legend` rather than stopping at the geometry.
    ///
    /// `legend_layout` cannot produce a NaN fraction - `bar_fraction` clamps to
    /// 0..1 and a non-finite engine value is dropped before a tick is built -
    /// but `draw_legend` takes a `LegendLayout` from anywhere, and a NaN
    /// fraction is not a panic: it is a tick at a NaN y, silently missing from
    /// a bar that still looks whole.
    #[test]
    fn degenerate_layouts_and_panes_are_drawn_without_panicking() {
        let table = test_table(vec![opaque_stop(0.0), opaque_stop(1.0)]);
        let tick = |label: &str, fraction: f32| LegendTick {
            engine_value: fraction,
            label: label.to_owned(),
            fraction,
        };
        let ladder = |ticks: Vec<LegendTick>| LegendLayout {
            span: ValueRange::new(0.0, 1.0),
            ticks,
            unit_label: "kt",
        };
        let broken_fractions = ladder(vec![
            tick("NaN", f32::NAN),
            tick("inf", f32::INFINITY),
            tick("0.5", 0.5),
        ]);
        let no_ticks = ladder(Vec::new());
        let one_tick = bar_labelled(&["7"]);
        let too_many: Vec<String> = (0..12).map(|index| format!("BADGE{index}")).collect();
        let blanks = vec![String::new(), " ".to_owned()];
        let wide = pane(600.0);

        let cases: Vec<(&str, &LegendLayout, &str, &[String], egui::Rect)> = vec![
            ("nan fractions", &broken_fractions, "X", &[], wide),
            ("no ticks", &no_ticks, "", &[], wide),
            ("one tick", &one_tick, "", &[], wide),
            ("twelve badges", &one_tick, "T", &too_many, wide),
            ("blank badges", &one_tick, "", &blanks, wide),
            ("zero size", &one_tick, "T", &[], egui::Rect::ZERO),
            (
                "inverted",
                &one_tick,
                "T",
                &[],
                egui::Rect::from_min_max(egui::pos2(10.0, 10.0), egui::pos2(-10.0, -10.0)),
            ),
            ("nan pane", &one_tick, "T", &[], egui::Rect::NAN),
            ("infinite pane", &one_tick, "T", &[], egui::Rect::EVERYTHING),
            ("empty pane", &one_tick, "T", &[], egui::Rect::NOTHING),
            (
                "too short for a bar",
                &one_tick,
                "T",
                &[],
                pane(600.0).with_max_y(80.0),
            ),
        ];

        for (name, layout, title, badges, pane_rect) in cases {
            with_painter(pane_rect, |painter| {
                draw_legend(painter, pane_rect, layout, &table, title, badges);
            });
            // Both guards in `legend_geometry` are `<` comparisons and every
            // `<` against a NaN is false, so a non-finite pane used to pass
            // both and produce a geometry of NaN rects that `egui` queues
            // without complaint. `Rect::NOTHING` is infinite, not zero-sized.
            assert!(
                pane_rect.is_finite() || geometry_in(pane_rect, layout, title, badges).is_none(),
                "{name}: {pane_rect:?} is not finite and still produced a legend"
            );
            for clipped in painted_shapes(pane_rect, layout, &table, title, badges) {
                let bounds = clipped.shape.visual_bounding_rect();
                assert!(
                    bounds.is_finite() || bounds == egui::Rect::NOTHING,
                    "{name}: queued a shape bounded by {bounds:?}"
                );
            }
        }

        // A tick the bar cannot place is not painted at all, rather than
        // painted at a NaN y. Counted against the same ladder with the two
        // unplaceable ticks removed: the difference would be one tick mark and
        // one glyph run each.
        let placeable = LegendLayout {
            span: broken_fractions.span,
            ticks: vec![broken_fractions.ticks[2].clone()],
            unit_label: broken_fractions.unit_label,
        };
        assert_eq!(
            painted_shapes(pane(600.0), &broken_fractions, &table, "X", &[]).len(),
            painted_shapes(pane(600.0), &placeable, &table, "X", &[]).len(),
            "a NaN or infinite tick fraction still put marks on the bar"
        );
    }

    /// What is actually PAINTED is the panel, and the panel is inside the pane.
    ///
    /// Every other width test here measures [`legend_geometry`]. This one takes
    /// the union of the shapes `draw_legend` queued into `egui`'s paint list,
    /// so a rect placed correctly and a glyph painted somewhere else fails here
    /// and nowhere else.
    #[test]
    fn every_shape_the_legend_paints_lands_inside_the_panel_it_measured() {
        let pane_rect = pane(600.0);
        for descriptor in ProductRegistry::builtin().all() {
            let name = descriptor.short_name;
            let table = crate::palettes::table_for(descriptor, &ColorTableSet::default());
            let Some(layout) = legend_layout(&descriptor.domain, &table) else {
                continue;
            };
            for badges in app_badge_stacks() {
                let geometry = geometry_in(pane_rect, &layout, name, &badges).expect("fits");
                let painted = painted_bounds(pane_rect, &layout, &table, name, &badges)
                    .unwrap_or_else(|| panic!("{name} painted nothing"));

                assert!(
                    pane_rect.contains_rect(painted),
                    "{name} {badges:?}: painted {painted:?}, outside the pane {pane_rect:?}"
                );
                assert!(
                    painted.width() <= OLD_FIXED_WIDTH + PANEL_PAD * 2.0,
                    "{name} {badges:?}: {} points of ink over the radar, against the {} the \
                     fixed-width version put there",
                    painted.width(),
                    OLD_FIXED_WIDTH + PANEL_PAD * 2.0
                );
                assert!(
                    painted.left() >= geometry.panel.left() - 1.0,
                    "{name} {badges:?}: ink at {} reaches past the panel's left edge at {}, so it \
                     is painted onto the radar with no backing",
                    painted.left(),
                    geometry.panel.left()
                );
            }
        }
    }
}

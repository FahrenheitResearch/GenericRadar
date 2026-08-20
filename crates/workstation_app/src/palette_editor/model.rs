//! The editor's own model of a colour table, free of egui.
//!
//! Two things force this to exist rather than editing a [`ColorTable`] in
//! place. A `ColorTable` holds **engine** values (metres per second, dBZ) and
//! has already applied and forgotten the header that produced them - the GR
//! `Scale:` row - so an editor built on it could not show an analyst the
//! numbers they typed: it would show 15.43 where they wrote 30 knots. And a
//! `ColorTable` is immutable by construction: it sorts, dedups and precomputes
//! Oklab at birth, which is exactly what you do not want under a value that is
//! being dragged.
//!
//! So the editor keeps display-unit stops with stable ids, and crosses to
//! `ColorTable` through exactly two functions - [`EditorTable::to_color_table`]
//! and [`EditorTable::from_color_table`]. Everything the preview, the gradient
//! strip and the pane ever see comes out of the first one, so what is on
//! screen is what the file says.
//!
//! # The dialect
//!
//! GR2Analyst `.pal`: `Product:`, `Units:`, `Scale:`, `Step:`, `RF:` and
//! `Color:`/`Color4:` rows, one stop per row, values ascending. A row may
//! carry **two** colours - `Color: 20 255 0 0 255 255 0` - which means "this
//! stop starts red and ramps to yellow just before the next stop". That is the
//! ramp pair, and `ColorStop::end_color` carries it, so the text this module
//! writes and the table the renderer samples say the same thing row for row.
//! An editor stop is therefore one file row and one `ColorStop`, with no
//! expansion step in between that could drift from either.

use std::fmt;

use color_tables::{ColorTable, ColorTableError, ColorTableFamily, Rgba8};

/// Knots to metres per second.
///
/// Declared here rather than imported because `color_tables` keeps its copies
/// private to its parser. `unit_conversion_uses_the_same_factor_the_parser_uses`
/// re-derives both through `ColorTable::parse` and fails if they ever drift,
/// so the duplication is pinned rather than trusted.
pub const KNOT_TO_MPS: f32 = 0.514_444;
/// Miles per hour to metres per second. Same provenance as [`KNOT_TO_MPS`].
pub const MPH_TO_MPS: f32 = 0.447_04;

/// A stop's identity, stable across sorting.
///
/// Sorting by value is not optional - a colour table is defined by ascending
/// stops - but it moves indices out from under a drag and out from under the
/// selection. Rows, the strip's handles and the selection all address stops by
/// this instead of by index.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct StopId(u32);

/// One row of the editor: a value in the table's own units, a colour, and
/// optionally the colour the segment ramps to before the next row.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct EditorStop {
    pub id: StopId,
    /// In [`EditorTable::units`], not engine units. 55 dBZ is 55.0; 30 knots
    /// is 30.0 and becomes 15.43 m/s only on the way to a [`ColorTable`].
    pub value: f32,
    pub color: Rgba8,
    /// The second colour of a GR two-colour row. `None` is a flat row.
    pub ramp_end: Option<Rgba8>,
}

/// The unit a table's numbers are written in.
///
/// Only [`Self::Knots`] and [`Self::MilesPerHour`] carry an actual conversion,
/// because those are the only two the palette parser rescales; `dBZ` and `m/s`
/// are labels on numbers that are already engine values. [`Self::Unstated`]
/// writes no `Units:` row at all, which is the honest choice for correlation
/// coefficient or differential phase - neither is measured in any of the four.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub enum EditorUnits {
    #[default]
    Unstated,
    Dbz,
    MetresPerSecond,
    Knots,
    MilesPerHour,
}

impl EditorUnits {
    /// Every choice, in the order a combo should offer them.
    pub const ALL: [Self; 5] = [
        Self::Unstated,
        Self::Dbz,
        Self::MetresPerSecond,
        Self::Knots,
        Self::MilesPerHour,
    ];

    /// What the combo shows.
    pub fn label(self) -> &'static str {
        match self {
            Self::Unstated => "(none)",
            Self::Dbz => "dBZ",
            Self::MetresPerSecond => "m/s",
            Self::Knots => "kt",
            Self::MilesPerHour => "mph",
        }
    }

    /// What goes in the file's `Units:` row, or `None` to omit the row.
    pub fn pal_token(self) -> Option<&'static str> {
        match self {
            Self::Unstated => None,
            Self::Dbz => Some("dBZ"),
            Self::MetresPerSecond => Some("m/s"),
            Self::Knots => Some("kt"),
            Self::MilesPerHour => Some("mph"),
        }
    }

    /// Multiply a display value by this to get the engine value.
    ///
    /// The same table `color_tables`' `unit_value_to_mps_scale` keeps: every
    /// spelling it does not recognise scales by one, which is why `dBZ` and
    /// `m/s` are 1.0 here rather than absent.
    pub fn to_engine(self) -> f32 {
        match self {
            Self::Unstated | Self::Dbz | Self::MetresPerSecond => 1.0,
            Self::Knots => KNOT_TO_MPS,
            Self::MilesPerHour => MPH_TO_MPS,
        }
    }

    /// Read a `Units:` row. Accepts the spellings the parser accepts, so a
    /// file written by hand (`KNOTS`, `mi/h`) survives a trip through the
    /// editor with its conversion intact.
    ///
    /// A unit this build has no entry for reads as [`Self::Unstated`]: the
    /// parser scales it by one and so does `Unstated`, so the numbers keep
    /// their meaning and only the label is lost. Inventing a variant per file
    /// instead would put text the combo cannot show into a control that has to
    /// round-trip.
    pub fn from_pal(token: &str) -> Self {
        match token.trim().to_ascii_lowercase().as_str() {
            "kt" | "kts" | "knot" | "knots" => Self::Knots,
            "mph" | "mi/h" => Self::MilesPerHour,
            "dbz" => Self::Dbz,
            "m/s" | "mps" | "ms" => Self::MetresPerSecond,
            _ => Self::Unstated,
        }
    }

    /// The unit a family's numbers are usually written in, for a new table.
    pub fn default_for(family: ColorTableFamily) -> Self {
        match family {
            ColorTableFamily::Reflectivity => Self::Dbz,
            ColorTableFamily::Velocity | ColorTableFamily::SpectrumWidth => Self::MetresPerSecond,
            _ => Self::Unstated,
        }
    }
}

/// What the table does between two stops, as the editor offers it.
///
/// Three rows for the analyst's two-valued question, because "smooth" is two
/// different mixers and a table that arrived from a `.pal` was authored
/// against one of them. `Mode: smooth` has meant "lerp the sRGB bytes" in
/// GR-format palettes for as long as they have existed; the perceptual mixer
/// (Ottosson 2020, "A perceptual color space for image processing") is newer
/// and is what every preset in this build flips into. Collapsing the two would
/// mean that opening someone's palette and pressing Save quietly repainted it.
///
/// The fourth `SampleMode` the parser knows - a quantised ramp - is not a row
/// here: it is [`EditorTable::step`] set while [`Self::Stepped`] is chosen,
/// because that is what the GR `Step:` row is.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum Sampling {
    /// `Mode: continuous` - mix in Oklab, and paint nothing below the first
    /// opaque stop.
    SmoothPerceptual,
    /// `Mode: smooth` - the legacy straight-line mix of the sRGB bytes, which
    /// is what a palette written for GR2Analyst or RadarScope was drawn
    /// through.
    SmoothLegacy,
    /// `Mode: stepped`, or `Step: n` when a step is set: one flat band per
    /// interval.
    Stepped,
}

impl Sampling {
    /// Every choice, smooth first because that is where a new table starts.
    pub const ALL: [Self; 3] = [Self::SmoothPerceptual, Self::SmoothLegacy, Self::Stepped];

    pub fn label(self) -> &'static str {
        match self {
            Self::SmoothPerceptual => "Smooth (perceptual)",
            Self::SmoothLegacy => "Smooth (sRGB ramp)",
            Self::Stepped => "Stepped",
        }
    }

    /// The one-line reason to pick this one, for the control's help text.
    pub fn help(self) -> &'static str {
        match self {
            Self::SmoothPerceptual => {
                "Colours are mixed in Oklab, so the ramp reads as even brightness. \
                 The rendering every shipped palette uses."
            }
            Self::SmoothLegacy => {
                "Colours are mixed straight in sRGB bytes - what GR2Analyst and \
                 RadarScope draw a .pal through. Keep this for a palette that was \
                 hand-tuned against it."
            }
            Self::Stepped => {
                "One flat band per interval, so band edges are contours to count. \
                 Set a step below to put the edges on a round grid instead of on \
                 the stops."
            }
        }
    }

    /// Whether the `Step:` row means anything here.
    pub fn uses_step(self) -> bool {
        matches!(self, Self::Stepped)
    }
}

/// A colour table as the editor holds it: display-unit stops plus the header
/// rows that decide what those numbers mean.
#[derive(Clone, Debug, PartialEq)]
pub struct EditorTable {
    pub name: String,
    /// Which measurement this table draws. Drives the preview's moment, the
    /// default value span, and the `Product:` row.
    pub family: ColorTableFamily,
    /// The raw `Product:` token. Kept verbatim rather than regenerated from
    /// `family` so a file that says `BR` does not come back saying `REF`.
    pub product: Option<String>,
    pub units: EditorUnits,
    /// The GR `Scale:` row. `Some` **overrides** the unit conversion
    /// entirely: the parser reads `1 / scale` and never looks at `Units:`.
    /// That is why it is the power-user control, and why changing it
    /// reinterprets the numbers instead of converting them.
    pub scale: Option<f32>,
    /// The GR `Step:` row: band edges land on this grid rather than on the
    /// stops. Only meaningful while [`Self::sampling`] is
    /// [`Sampling::Stepped`]; a smooth table writes no `Step:` row, because
    /// the parser lets a later `Mode:` row override it and the number would
    /// round-trip as a lie.
    pub step: Option<f32>,
    pub sampling: Sampling,
    pub range_folded: Rgba8,
    /// Always sorted ascending by value. Every mutator here restores that.
    stops: Vec<EditorStop>,
    next_id: u32,
}

impl EditorTable {
    /// A minimal two-stop table over a family's nominal domain, for "new".
    pub fn new(family: ColorTableFamily, name: impl Into<String>) -> Self {
        let (low, high) = family.nominal_domain();
        let mut table = Self {
            name: name.into(),
            family,
            product: product_token(family).map(str::to_owned),
            units: EditorUnits::default_for(family),
            scale: None,
            step: None,
            sampling: Sampling::SmoothPerceptual,
            range_folded: default_range_folded(),
            stops: Vec::new(),
            next_id: 0,
        };
        table.push_stop(low, Rgba8::opaque(16, 24, 48), None);
        table.push_stop(high, Rgba8::opaque(240, 240, 240), None);
        table
    }

    pub fn stops(&self) -> &[EditorStop] {
        &self.stops
    }

    pub fn index_of(&self, id: StopId) -> Option<usize> {
        self.stops.iter().position(|stop| stop.id == id)
    }

    pub fn stop(&self, id: StopId) -> Option<&EditorStop> {
        self.stops.iter().find(|stop| stop.id == id)
    }

    pub fn stop_mut(&mut self, id: StopId) -> Option<&mut EditorStop> {
        self.stops.iter_mut().find(|stop| stop.id == id)
    }

    /// Multiply a display value by this to get the engine value the renderer
    /// samples with. `Scale:` wins over `Units:` because that is the order
    /// `ColorTable::parse` resolves them in.
    pub fn engine_factor(&self) -> f32 {
        match self.scale {
            Some(scale) if scale.is_finite() && scale > 0.0 => 1.0 / scale,
            _ => self.units.to_engine(),
        }
    }

    pub fn to_engine(&self, display_value: f32) -> f32 {
        display_value * self.engine_factor()
    }

    pub fn to_display(&self, engine_value: f32) -> f32 {
        let factor = self.engine_factor();
        if factor == 0.0 {
            engine_value
        } else {
            engine_value / factor
        }
    }

    /// The display-unit interval the stops cover. Both bounds are equal when a
    /// table has collapsed onto one value, which a strip must test for before
    /// dividing by the span.
    pub fn display_span(&self) -> (f32, f32) {
        match (self.stops.first(), self.stops.last()) {
            (Some(first), Some(last)) => (first.value, last.value),
            _ => (0.0, 1.0),
        }
    }

    /// The interval the strip, its handles and the value drags run over.
    ///
    /// [`Self::display_span`] with one guarantee added: it is never
    /// zero-width. A table whose stops have all been typed onto the same
    /// value - one keystroke away in the stop list - has a display span of
    /// `(v, v)`, and an axis of zero width maps every handle to the same
    /// pixel and every pixel back to the same value, so the handles stack and
    /// dragging one moves nothing. Widening by [`Self::fallback_gap`] leaves
    /// a real axis to pull the stops apart on, which is the only pointer-led
    /// way out of that state.
    pub fn strip_span(&self) -> (f32, f32) {
        let (low, high) = self.display_span();
        if high > low {
            return (low, high);
        }
        let gap = self.fallback_gap();
        (low - gap, high + gap)
    }

    /// Display units per pixel for a stop's value drag.
    ///
    /// Scaled so a full sweep of the strip covers the table's own range rather
    /// than a fixed number of units - a correlation coefficient table and a
    /// reflectivity table are three orders of magnitude apart. Read off
    /// [`Self::strip_span`] and not [`Self::display_span`], because a
    /// collapsed table's span is zero and the floor that used to catch it -
    /// a thousandth of a unit per pixel - meant ninety thousand pixels of
    /// travel to drag a stop off 95 dBZ.
    pub fn drag_speed(&self) -> f32 {
        let (low, high) = self.strip_span();
        ((high - low).abs() / 400.0).max(f32::MIN_POSITIVE)
    }

    /// The value gap to open when there is no interval to work in.
    ///
    /// A twentieth of the family's own nominal domain, expressed in the
    /// table's display units, so it is 6.35 dBZ on reflectivity and 0.05 on
    /// correlation coefficient rather than one unit in both. Used wherever a
    /// stop has to be placed somewhere no existing stop already is.
    pub fn fallback_gap(&self) -> f32 {
        let (low, high) = self.family.nominal_domain();
        let width = self.to_display(high - low).abs();
        if width.is_finite() && width > 0.0 {
            width / 20.0
        } else {
            1.0
        }
    }

    /// Change the unit and **re-express every number in it**, so each colour
    /// stays on the physical value it was on.
    ///
    /// Computed from [`Self::engine_factor`] before and after rather than from
    /// the two units directly, which is what makes it a no-op while a `Scale:`
    /// row is in force: with a scale set the unit is inert, so switching it
    /// must not move a single stop.
    pub fn set_units(&mut self, units: EditorUnits) {
        let before = self.engine_factor();
        self.units = units;
        let after = self.engine_factor();
        if before == after || after == 0.0 || !before.is_finite() || !after.is_finite() {
            return;
        }
        let ratio = before / after;
        for stop in &mut self.stops {
            stop.value *= ratio;
        }
        if let Some(step) = self.step {
            self.step = Some(step * ratio);
        }
        self.sort_stops();
    }

    /// Set (or clear) the `Scale:` row **without touching a number**.
    ///
    /// The deliberate opposite of [`Self::set_units`]: the stops keep the
    /// values written against them and every one of them comes to mean
    /// something else. That is what "reinterpret" means, and it is the reason
    /// both controls exist instead of one.
    pub fn set_scale(&mut self, scale: Option<f32>) {
        self.scale = scale.filter(|value| value.is_finite() && *value > 0.0);
    }

    /// Move one stop and restore the ascending order.
    pub fn set_value(&mut self, id: StopId, value: f32) {
        if !value.is_finite() {
            return;
        }
        if let Some(stop) = self.stop_mut(id) {
            stop.value = value;
        }
        self.sort_stops();
    }

    /// Insert a stop midway between `id` and the stop above it, coloured so
    /// the insertion changes no pixel a smooth table paints.
    ///
    /// The colour is what the table **already** paints at that value, read
    /// back out of the real sampler rather than guessed by mixing the two
    /// neighbours. The two are not the same: a segment may declare a ramp
    /// target the next stop's colour knows nothing about, a clear row holds
    /// clear across its whole segment instead of fading, and the perceptual
    /// mixer's midpoint is not the sRGB one. An analyst adds a stop to gain a
    /// handle, not to gain a stripe. The mix is kept only for the case where
    /// there is no table to sample - fewer than two stops, which the editor
    /// cannot reach but a caller could.
    ///
    /// Past the last stop there is nothing to be midway between, so the new
    /// stop lands one interval further on, keeping the last interval's width,
    /// and takes the last colour the table reaches.
    ///
    /// Where there is no room at all - two stops on the same value, or two a
    /// single ULP apart, both of which the stop list's value field can reach
    /// in one keystroke - the new stop lands a whole [`Self::fallback_gap`]
    /// above instead. That is the difference between a button that recovers a
    /// collapsed table and one that grows the list without changing anything:
    /// the midpoint of two equal values is that value again, so every stop
    /// added lands on the pile and `to_color_table` keeps failing.
    pub fn insert_after(&mut self, id: StopId) -> Option<StopId> {
        let index = self.index_of(id)?;
        let here = self.stops[index];
        let gap = self.fallback_gap();
        let (value, fallback) = match self.stops.get(index + 1) {
            Some(next) => {
                let middle = midpoint(here.value, next.value);
                let value = if middle > here.value && middle < next.value {
                    middle
                } else {
                    here.value + gap
                };
                (
                    value,
                    mix(here.ramp_end.unwrap_or(here.color), next.color, 0.5),
                )
            }
            None => {
                let width = match index
                    .checked_sub(1)
                    .and_then(|before| self.stops.get(before))
                {
                    Some(previous) => (here.value - previous.value).abs(),
                    None => gap,
                };
                let width = if width > 0.0 { width } else { gap };
                (here.value + width, here.ramp_end.unwrap_or(here.color))
            }
        };
        let color = self
            .to_color_table()
            .map(|table| table.sample(self.to_engine(value)))
            .unwrap_or(fallback);
        Some(self.push_stop(value, color, None))
    }

    /// Remove a stop. Refuses to go below two, because a table of one stop is
    /// not a colour table - `ColorTable::new` rejects it - and an editor that
    /// let you reach that state would have nothing to preview.
    pub fn remove(&mut self, id: StopId) -> bool {
        if self.stops.len() <= 2 {
            return false;
        }
        let Some(index) = self.index_of(id) else {
            return false;
        };
        self.stops.remove(index);
        true
    }

    /// Append a stop and return its id. Sorts, so the caller may push in any
    /// order.
    pub fn push_stop(&mut self, value: f32, color: Rgba8, ramp_end: Option<Rgba8>) -> StopId {
        let id = StopId(self.next_id);
        self.next_id = self.next_id.wrapping_add(1);
        self.stops.push(EditorStop {
            id,
            value,
            color,
            ramp_end,
        });
        self.sort_stops();
        id
    }

    /// Drop every stop, for a reader that is about to install its own.
    ///
    /// Leaves the table in the one state the rest of this type forbids -
    /// fewer than two stops - so it is only reachable from inside this module
    /// tree, and only ever with a full replacement following it.
    pub(super) fn clear_stops(&mut self) {
        self.stops.clear();
    }

    fn sort_stops(&mut self) {
        // Stable, so two stops written at the same value keep the order they
        // were typed in rather than swapping under the pointer every frame.
        self.stops
            .sort_by(|left, right| left.value.total_cmp(&right.value));
    }

    /// The name as the file carries it and as everything downstream of the
    /// file sees it: [`one_line`] applied to whatever is in the Name field.
    ///
    /// The field itself is left exactly as typed - trimming under a cursor
    /// makes a space impossible to type in the middle of a name - so the
    /// canonical form has to be taken here, at every crossing out of the
    /// editor. There is one crossing that is easy to miss and was:
    /// [`Self::to_color_table`] names the parsed table, and naming it from the
    /// raw field while [`Self::pal_text`] wrote the canonical one put a
    /// different name on the table built from the text than on the table built
    /// from the file, which `super::store::save`'s equality check then read as
    /// the colours having changed.
    pub fn pal_name(&self) -> String {
        one_line(&self.name)
    }

    /// The table as a `.pal` file.
    ///
    /// One text, written once: it is what [`super::store`] saves AND what
    /// [`Self::to_color_table`] hands the parser, so the file and the picture
    /// on screen cannot disagree by construction rather than by care.
    pub fn pal_text(&self) -> String {
        let mut text = String::new();
        text.push_str("; GenericRadar colour table\n");
        text.push_str(
            "; GR2Analyst .pal dialect: header rows, then one Color4 row per\n\
             ; stop (value, RGBA). A second RGBA on a row is the colour that\n\
             ; row ramps to just before the next one.\n",
        );
        text.push_str(&format!("Name: {}\n", self.pal_name()));
        if let Some(product) = self.product.as_deref().map(one_line)
            && !product.is_empty()
        {
            text.push_str(&format!("Product: {product}\n"));
        }
        if let Some(units) = self.units.pal_token() {
            text.push_str(&format!("Units: {units}\n"));
        }
        if let Some(scale) = self.scale {
            text.push_str(&format!("Scale: {}\n", number(scale)));
        }
        // Exactly one of `Step:` and `Mode:` is ever written. Both would be
        // ambiguous: the parser keeps one sampling variable that either row
        // overwrites, so which of the two survived would depend on the order
        // they happened to land in.
        match (self.sampling, self.step) {
            (Sampling::Stepped, Some(step)) => {
                text.push_str(&format!("Step: {}\n", number(step)));
            }
            (Sampling::Stepped, None) => text.push_str("Mode: stepped\n"),
            (Sampling::SmoothLegacy, _) => text.push_str("Mode: smooth\n"),
            (Sampling::SmoothPerceptual, _) => text.push_str("Mode: continuous\n"),
        }
        text.push_str(&format!(
            "RF: {} {} {} {}\n",
            self.range_folded.r, self.range_folded.g, self.range_folded.b, self.range_folded.a
        ));
        for stop in &self.stops {
            text.push_str(&format!(
                "Color4: {} {}",
                number(stop.value),
                rgba_text(stop.color)
            ));
            if let Some(end) = stop.ramp_end {
                text.push(' ');
                text.push_str(&rgba_text(end));
            }
            text.push('\n');
        }
        text
    }

    /// The table the renderer, the strip and the pane all sample.
    ///
    /// Built by writing the `.pal` and handing it to the real parser, rather
    /// than by assembling `ColorStop`s directly, so that the preview is
    /// literally the saved file read back: a header the writer emits wrongly
    /// shows up as a wrong picture instead of as a file nobody notices until
    /// it is reloaded.
    pub fn to_color_table(&self) -> Result<ColorTable, ColorTableError> {
        // `pal_name`, not `name`: `ColorTable::parse` has no `Name:` arm and
        // takes the name from this argument, so anything other than the name
        // the text declares makes the built table disagree with the file.
        ColorTable::parse(self.pal_name(), &self.pal_text())
    }

    /// Read an installed table back into editable state.
    ///
    /// `family` is passed in because a `ColorTable` does not carry one - the
    /// application decides which family a table is installed into - and is
    /// only used when the `Product:` row does not name one.
    ///
    /// What cannot come back is the `Scale:` row: a `ColorTable` has already
    /// applied it and forgotten it, so it is stated as `None` rather than
    /// guessed and the numbers on screen are the engine values the table
    /// actually holds. Ramp pairs do come back - `ColorStop::end_color`
    /// carries them.
    pub fn from_color_table(family: ColorTableFamily, table: &ColorTable) -> Self {
        let product = table.product().map(str::to_owned);
        let family = product
            .as_deref()
            .and_then(family_from_product_token)
            .unwrap_or(family);
        // A recovered unit is trusted only when it does not rescale, and that
        // is not timidity: a `ColorTable` applies EITHER its `Scale:` row or
        // its `Units:` row - scale wins - and then forgets which. So a table
        // whose row says `kt` may have had its knots converted by a scale of
        // 1.94384, or by 2.5, or not at all. Dividing the engine values by the
        // knot factor to "recover" the authored numbers would be a guess, and
        // a wrong guess moves every colour. Engine values under a unit that
        // does not rescale are the honest reading. A table that came from a
        // file does not lose its unit, because the editor reloads that from
        // the file itself - see `super::pal`.
        let recovered = table.units().map(EditorUnits::from_pal);
        let units = match recovered {
            Some(units) if units.to_engine() == 1.0 => units,
            _ => EditorUnits::default_for(family),
        };
        // `sample_mode_label` rather than `rendering`, because the two smooth
        // modes have to be told apart: a palette drawn through the legacy sRGB
        // lerp must keep being drawn through it.
        let sampling = match table.sample_mode_label() {
            "interpolated" => Sampling::SmoothLegacy,
            "continuous" => Sampling::SmoothPerceptual,
            _ => Sampling::Stepped,
        };
        let mut editable = Self {
            // `base_name`, not `name`: the installed table's name carries its
            // sampling mode as a suffix so two drawings of one palette can sit
            // in the same list, and that suffix is not part of what the
            // palette is called. Editing "AWIPS Wilson REF (continuous)" would
            // otherwise save a file named after the switch position.
            name: table.base_name().to_owned(),
            family,
            product,
            units,
            scale: None,
            step: None,
            sampling,
            range_folded: table.range_folded_rgba(),
            stops: Vec::new(),
            next_id: 0,
        };
        // Step is a display-unit number, and `ColorTable` holds it scaled into
        // engine units along with the stops.
        editable.step = table.step_size().map(|step| editable.to_display(step));
        // A transparent row that fades UP into an inked one has to say where
        // it fades to, or the file this editor writes would paint a band the
        // table on screen does not.
        //
        // The reason is the dialect, not this editor. A `.pal` row that paints
        // nothing and declares no second colour is read as a mask that holds
        // transparent right up to the next row - see
        // `color_tables::hold_clear_gr_rows` - while a table built in Rust
        // from `clear_stop` ramps out of it like any other undeclared segment.
        // Both readings are deliberate and both are right for what they read.
        // But an editable table is on its way to becoming a `.pal`, so the
        // moment a Rust-built preset is opened here its clear rows are written
        // out in the form the dialect reads the same way the table already
        // paints. Without this, duplicating Smooth Classic REF lost the fade
        // between its 9.5 dBZ clear row and its first inked stop at 10.
        //
        // Only where the two readings differ: a clear row whose next row is
        // also clear holds and ramps to the same nothing, and spelling that
        // out would put a redundant second colour on the row for no gain.
        let stops = table.stops();
        for (index, stop) in stops.iter().enumerate() {
            let ramp_end = stop.end_color.or_else(|| {
                (stop.color.a == 0)
                    .then(|| stops.get(index + 1).map(|next| next.color))
                    .flatten()
                    .filter(|next| next.a != 0)
            });
            editable.push_stop(editable.to_display(stop.value), stop.color, ramp_end);
        }
        editable
    }
}

/// The GR `Product:` token for a family, or `None` for the catch-all, which
/// names no measurement and so has no token to write.
pub fn product_token(family: ColorTableFamily) -> Option<&'static str> {
    match family {
        ColorTableFamily::Reflectivity => Some("BR"),
        ColorTableFamily::Velocity => Some("BV"),
        ColorTableFamily::SpectrumWidth => Some("SW"),
        ColorTableFamily::DifferentialReflectivity => Some("ZDR"),
        ColorTableFamily::CorrelationCoefficient => Some("CC"),
        ColorTableFamily::DifferentialPhase => Some("PHI"),
        ColorTableFamily::SpecificDifferentialPhase => Some("KDP"),
        ColorTableFamily::Generic => None,
    }
}

/// Which family a `Product:` token names, accepting both the GR spellings
/// (`BR`, `BV`) and the NEXRAD moment names an analyst is as likely to type.
pub fn family_from_product_token(token: &str) -> Option<ColorTableFamily> {
    match token.trim().to_ascii_uppercase().as_str() {
        "BR" | "REF" | "N0Q" | "DBZ" => Some(ColorTableFamily::Reflectivity),
        "BV" | "VEL" | "SRV" | "N0U" => Some(ColorTableFamily::Velocity),
        "SW" | "SPW" => Some(ColorTableFamily::SpectrumWidth),
        "ZDR" => Some(ColorTableFamily::DifferentialReflectivity),
        "CC" | "RHO" | "RHOHV" => Some(ColorTableFamily::CorrelationCoefficient),
        "PHI" | "PHIDP" => Some(ColorTableFamily::DifferentialPhase),
        "KDP" => Some(ColorTableFamily::SpecificDifferentialPhase),
        _ => None,
    }
}

/// The parser's own default range-folded colour, so a new table starts where
/// every built-in starts rather than at black.
fn default_range_folded() -> Rgba8 {
    Rgba8::new(126, 80, 196, 245)
}

fn midpoint(low: f32, high: f32) -> f32 {
    low + (high - low) * 0.5
}

fn mix(left: Rgba8, right: Rgba8, amount: f32) -> Rgba8 {
    let lerp = |left: u8, right: u8| {
        (f32::from(left) + (f32::from(right) - f32::from(left)) * amount).round() as u8
    };
    Rgba8::new(
        lerp(left.r, right.r),
        lerp(left.g, right.g),
        lerp(left.b, right.b),
        lerp(left.a, right.a),
    )
}

fn rgba_text(color: Rgba8) -> String {
    format!("{} {} {} {}", color.r, color.g, color.b, color.a)
}

/// A number that reads back as the same `f32`.
///
/// `Display` for `f32` prints the shortest decimal that round-trips, which is
/// exactly the contract the save-and-reload test needs: write, parse, write
/// again, identical bytes.
fn number(value: f32) -> String {
    format!("{value}")
}

/// Collapse a header value onto one line, exactly as a reader will see it back.
///
/// A newline inside a name would end the row and turn the rest of the name
/// into an unknown key, so control characters become spaces. The no-break
/// space goes the same way for a different reason: both `.pal` readers - the
/// shipped `ColorTable::parse` and [`super::pal`] - replace U+00A0 with an
/// ordinary space before they look at a line, so a name pasted out of a word
/// processor comes back with plain spaces in it whatever was written. Writing
/// the no-break space and reading a space is the file meaning one thing and
/// the editor another, which is the one thing a save is not allowed to do.
///
/// Then trimmed, because the reader trims: `Name: Bench ` reads back as
/// `Bench`, and a name that keeps its trailing space on screen is a name the
/// file cannot carry.
pub(super) fn one_line(text: &str) -> String {
    text.chars()
        .map(|character| {
            if character.is_control() || character == '\u{a0}' {
                ' '
            } else {
                character
            }
        })
        .collect::<String>()
        .trim()
        .to_owned()
}

/// Why a save was refused: the file it was about to write does not read back
/// as the table on screen.
#[derive(Clone, Debug, PartialEq)]
pub enum RoundTripError {
    /// The text parsed into a different set of stops, headers or colours.
    Mismatch(&'static str),
    /// The table itself is not a colour table - fewer than two distinct stops.
    NotATable(ColorTableError),
}

impl fmt::Display for RoundTripError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Mismatch(what) => write!(formatter, "the saved file would not read back: {what}"),
            Self::NotATable(error) => write!(formatter, "{error}"),
        }
    }
}

impl std::error::Error for RoundTripError {}

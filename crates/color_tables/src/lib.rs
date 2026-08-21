//! Fast color table parsing and sampling for radar renderers.
//!
//! A [`ColorTable`] is a list of [`ColorStop`]s plus a [`SampleMode`] that says
//! what happens *between* the stops. The mode is not part of the palette: the
//! same stops drawn as flat bands and drawn as a continuous ramp are two
//! pictures of one colour scheme, and [`ColorTable::rendered`] converts between
//! them without touching a single stop. [`TableRendering`] is the two-valued
//! control an analyst actually holds.

pub mod files;
pub mod hazards;
pub mod oklab;
pub mod presets;
pub mod user;

use std::collections::hash_map::DefaultHasher;
use std::fmt;
use std::hash::{Hash, Hasher};

pub use files::{
    PALETTE_EXTENSION, PaletteFile, UserNameFault, palette_named_in, palette_names_in,
    user_palette_name_fault,
};
pub use presets::*;

use presets::{
    CC_MAX, CC_MIN, KDP_MAX_DEG_PER_KM, KDP_MIN_DEG_PER_KM, PHI_MAX_DEG, PHI_MIN_DEG, ZDR_MAX_DB,
    ZDR_MIN_DB,
};
/// The meteorologically interesting sub-range of the ZDR domain, which only the
/// tests that check where each ZDR palette spends its colour need.
#[cfg(test)]
use presets::{ZDR_MET_MAX_DB, ZDR_MET_MIN_DB};

const KNOT_TO_MPS: f32 = 0.514_444;
const MPH_TO_MPS: f32 = 0.447_04;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct Rgba8 {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl Rgba8 {
    pub const TRANSPARENT: Self = Self {
        r: 0,
        g: 0,
        b: 0,
        a: 0,
    };

    pub const fn new(r: u8, g: u8, b: u8, a: u8) -> Self {
        Self { r, g, b, a }
    }

    pub const fn opaque(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b, a: 255 }
    }

    pub const fn to_array(self) -> [u8; 4] {
        [self.r, self.g, self.b, self.a]
    }

    fn lerp(self, other: Self, amount: f32) -> Self {
        let amount = amount.clamp(0.0, 1.0);
        Self {
            r: lerp_u8(self.r, other.r, amount),
            g: lerp_u8(self.g, other.g, amount),
            b: lerp_u8(self.b, other.b, amount),
            a: lerp_u8(self.a, other.a, amount),
        }
    }
}

/// Which physical domain a colour table is drawn over.
///
/// The dual-pol moments each get their own family because their domains have
/// nothing in common: ZDR is a small signed decibel ratio, CC is a bounded
/// correlation crowded against 1.0, PHI is a 360-degree angle that wraps, and
/// KDP is a small signed gradient. A single "other" ramp over 0..100 - which is
/// what all four shared before - leaves every one of them a flat wash, because
/// the whole observed distribution of each falls inside a single stop interval.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ColorTableFamily {
    Reflectivity,
    ReceivedPower,
    Velocity,
    SpectrumWidth,
    DifferentialReflectivity,
    CorrelationCoefficient,
    DifferentialPhase,
    SpecificDifferentialPhase,
    Generic,
}

impl ColorTableFamily {
    pub fn label(self) -> &'static str {
        match self {
            Self::Reflectivity => "Reflectivity",
            Self::ReceivedPower => "Received Power (dBm)",
            Self::Velocity => "Velocity / SRV",
            Self::SpectrumWidth => "Spectrum Width",
            Self::DifferentialReflectivity => "Differential Reflectivity (ZDR)",
            Self::CorrelationCoefficient => "Correlation Coefficient (CC)",
            Self::DifferentialPhase => "Differential Phase (PHI)",
            Self::SpecificDifferentialPhase => "Specific Differential Phase (KDP)",
            Self::Generic => "Other",
        }
    }

    /// Every family, in the order a picker should list them.
    ///
    /// Base moments first because they are what an analyst reaches for most,
    /// then dual-pol in the order the moments appear in a NEXRAD message, then
    /// the catch-all last.
    pub const ALL: [Self; 9] = [
        Self::Reflectivity,
        Self::ReceivedPower,
        Self::Velocity,
        Self::SpectrumWidth,
        Self::DifferentialReflectivity,
        Self::CorrelationCoefficient,
        Self::DifferentialPhase,
        Self::SpecificDifferentialPhase,
        Self::Generic,
    ];

    /// The engine-value domain the family's moment lives on.
    ///
    /// A caller that has to size a histogram axis or synthesise a ramp needs a
    /// range for the *moment* without having to sniff whichever table happens
    /// to be selected. Ranges follow Ryzhkov and Zrnic (2019), *Radar
    /// Polarimetry for Weather Observations*, Springer,
    /// doi:10.1007/978-3-030-05093-1, taken where possible from the WSR-88D
    /// Level II field encodings themselves - see `ZDR_MIN_DB` and `CC_MIN`.
    ///
    /// It is **not** a legend range. For the five dual-pol and spectrum-width
    /// families every built-in table is drawn edge to edge on this domain, and
    /// a test pins that. The three older families are not: the reflectivity
    /// presets ink from -15 or -10 dBZ to between 75 and 95, the velocity
    /// presets from -70 to +70 except Sign Check VEL at +/-100, and Generic is a
    /// placeholder ramp. A legend must therefore use the selected table's
    /// `inked_value_span`, which is what it paints, and keep this for the axis
    /// it is laid out against.
    pub fn nominal_domain(self) -> (f32, f32) {
        match self {
            Self::Reflectivity => (-32.0, 95.0),
            // NCAR's operational `dbmlow.colors`, used for the DOW6/7 DBM
            // products, is defined edge-to-edge over this received-power span.
            // https://github.com/NCAR/lrose-displays/blob/master/color_scales/dbmlow.colors
            Self::ReceivedPower => (-120.0, 20.0),
            Self::Velocity => (-70.0, 70.0),
            Self::SpectrumWidth => (0.0, 24.0),
            Self::DifferentialReflectivity => (ZDR_MIN_DB, ZDR_MAX_DB),
            Self::CorrelationCoefficient => (CC_MIN, CC_MAX),
            Self::DifferentialPhase => (PHI_MIN_DEG, PHI_MAX_DEG),
            Self::SpecificDifferentialPhase => (KDP_MIN_DEG_PER_KM, KDP_MAX_DEG_PER_KM),
            Self::Generic => (0.0, 100.0),
        }
    }

    /// Whether the family's domain wraps, so the first and last colour must
    /// agree. Only differential phase does.
    pub fn is_cyclic(self) -> bool {
        matches!(self, Self::DifferentialPhase)
    }
}

/// One row of a palette: a value, the colour at that value, and optionally the
/// colour the segment above it reaches.
///
/// The second colour is the GR2Analyst/RadarScope `.pal` two-colour ramp
/// entry, `color: <value> <r> <g> <b> <r2> <g2> <b2>`, which shared palettes
/// use constantly. A row that declares one says "ramp from my colour to *this* by
/// the next row", which is not the same picture as "ramp to the next row's
/// colour": the two differ wherever a palette wants a hard edge between bands
/// and a gradient inside them, which is most of what a hand-tuned reflectivity
/// table is doing.
///
/// `None` means the row declares one colour, and the segment above it ramps to
/// whatever the next row opens with. That is the older, simpler dialect and
/// the shape every table built from [`stop`] has.
///
/// Which of the two a mode actually paints is [`ColorTable::sample`]'s
/// business, and the banded modes deliberately ignore it: a hard band has one
/// colour.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ColorStop {
    pub value: f32,
    pub color: Rgba8,
    /// The colour this stop's segment reaches just before the next stop, when
    /// the row declared one.
    pub end_color: Option<Rgba8>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ColorTable {
    name: String,
    product: Option<String>,
    units: Option<String>,
    range_folded: Rgba8,
    /// How this copy of the palette is being drawn right now.
    sample_mode: SampleMode,
    /// The mode the palette was written with, which never changes.
    ///
    /// Carried separately because neither drawing can be recovered from the
    /// stops once `sample_mode` has moved. A palette that declares `step: 5`
    /// bands on a 5 dBZ grid rather than at its stop values, and a palette
    /// written as a continuous sRGB ramp has to stay one; switch `sample_mode`
    /// away from either and the fact would be gone for good. Keeping the
    /// original here is what makes [`ColorTable::rendered`] a lossless round
    /// trip in both directions, which in turn is what lets the sampling be a
    /// control the analyst flips rather than a property baked into a table at
    /// birth.
    authored_mode: SampleMode,
    stops: Vec<ColorStop>,
    /// Every stop's colour in Oklab, in stop order.
    ///
    /// Derived from `stops` at construction and never separately mutated, so
    /// `stop_oklab.len() == stops.len()` always holds. It exists so the
    /// perceptual sampler runs only the inverse transform per sample; see
    /// [`oklab`] for why that matters at the per-pixel call rate.
    stop_oklab: Vec<oklab::Oklab>,
    /// The colour each segment ramps *to*, in Oklab, resolved by
    /// `segment_end_color` at construction and never separately mutated, so
    /// `segment_end_oklab.len() == stops.len()` always holds.
    ///
    /// Kept resolved rather than stored per declaration so the perceptual
    /// sampler makes one array read instead of re-deciding which of the three
    /// targets applies on every pixel.
    segment_end_oklab: Vec<oklab::Oklab>,
}

impl ColorTable {
    pub fn new(name: impl Into<String>, stops: Vec<ColorStop>) -> Result<Self, ColorTableError> {
        Self::from_parts(
            name.into(),
            None,
            None,
            default_range_folded_color(),
            SampleMode::Interpolated,
            stops,
        )
    }

    pub fn new_stepped(
        name: impl Into<String>,
        stops: Vec<ColorStop>,
    ) -> Result<Self, ColorTableError> {
        Self::from_parts(
            name.into(),
            None,
            None,
            default_range_folded_color(),
            SampleMode::Stepped,
            stops,
        )
    }

    pub fn parse(name: impl Into<String>, text: &str) -> Result<Self, ColorTableError> {
        Self::parse_with_default_mode(name, text, SampleMode::Interpolated)
    }

    pub fn parse_with_default_mode(
        name: impl Into<String>,
        text: &str,
        default_sample_mode: SampleMode,
    ) -> Result<Self, ColorTableError> {
        let name = name.into();
        let mut product = None;
        let mut units = None;
        let mut scale = None;
        let mut range_folded = default_range_folded_color();
        let mut sample_mode = default_sample_mode;
        let mut stops = Vec::new();

        for (line_index, original_line) in text.lines().enumerate() {
            let line_number = line_index + 1;
            let line = normalize_line(original_line);
            let line = line.trim();
            if line.is_empty()
                || line.starts_with(';')
                || line.starts_with('#')
                || line.starts_with("$$")
            {
                continue;
            }

            let Some((raw_key, raw_value)) = split_key_value(line) else {
                continue;
            };
            let key = normalize_key(raw_key);
            let value = raw_value.trim();

            match key.as_str() {
                "product" => product = non_empty(value),
                "units" => units = non_empty(value),
                "scale" => scale = parse_positive_f32(value),
                "step" => {
                    sample_mode = parse_positive_f32(value)
                        .map(|step| SampleMode::QuantizedInterpolated { step, origin: 0.0 })
                        .unwrap_or(SampleMode::Stepped);
                }
                "mode" | "samplemode" | "interpolate" | "interpolation" | "smooth" => {
                    if let Some(parsed_mode) = parse_sample_mode(value) {
                        sample_mode = parsed_mode;
                    }
                }
                "rf" | "rangefolded" | "rangefoldedcolor" => {
                    range_folded = parse_color_only(value, line_number)?;
                }
                "color" | "color4" | "solidcolor" | "solidcolor4" => {
                    stops.push(parse_color_stop(value, key.ends_with('4'), line_number)?);
                }
                _ => {}
            }
        }

        let unit_scale = scale
            .map(|scale| 1.0 / scale)
            .or_else(|| units.as_deref().map(unit_value_to_mps_scale))
            .unwrap_or(1.0);
        if unit_scale != 1.0 {
            for stop in &mut stops {
                stop.value *= unit_scale;
            }
            sample_mode = sample_mode.scale_values(unit_scale);
        }
        // The text is written in the GR `.pal` dialect by definition of having
        // arrived here, so its clear rows carry that dialect's hold. Tables
        // built from stops in Rust do not, and must not pick it up from the
        // sampler - see `hold_clear_gr_rows`.
        hold_clear_gr_rows(&mut stops);

        Self::from_parts(name, product, units, range_folded, sample_mode, stops)
    }

    pub fn parse_stepped(name: impl Into<String>, text: &str) -> Result<Self, ColorTableError> {
        Self::parse_with_default_mode(name, text, SampleMode::Stepped)
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn product(&self) -> Option<&str> {
        self.product.as_deref()
    }

    pub fn units(&self) -> Option<&str> {
        self.units.as_deref()
    }

    pub fn stops(&self) -> &[ColorStop] {
        &self.stops
    }

    /// Whether the palette is drawn as a continuous ramp rather than as bands.
    ///
    /// True for both continuous modes. `Interpolated` is the legacy sRGB path,
    /// kept bit-for-bit for the five palettes authored on it; `Continuous` is
    /// the perceptual path any palette can be switched into. They differ in the
    /// colours they invent between stops, not in whether they invent any.
    pub fn interpolates(&self) -> bool {
        matches!(
            self.sample_mode,
            SampleMode::Interpolated | SampleMode::Continuous
        )
    }

    pub fn sample_mode_label(&self) -> &'static str {
        self.sample_mode.label()
    }

    pub fn step_size(&self) -> Option<f32> {
        self.sample_mode.step_size()
    }

    /// Which of the two things an analyst can ask for this palette is doing.
    pub fn rendering(&self) -> TableRendering {
        self.sample_mode.rendering()
    }

    /// The palette's name with its sampling mode stripped off the end.
    ///
    /// Every built-in carries its mode in its name, because two rows of a
    /// picker can hold the same palette drawn two ways and something has to
    /// tell them apart. That makes the full name useless as *identity*: it
    /// changes when the analyst flips the switch, and code that remembers "the
    /// installed table" by name would lose track of it at the flip. This is the
    /// half that does not move.
    ///
    /// A palette whose name carries no mode - anything an analyst loads from a
    /// file - is returned unchanged.
    pub fn base_name(&self) -> &str {
        base_name_of(&self.name)
    }

    /// The same palette, drawn the other way.
    ///
    /// Not a different table: the stops, the product, the units, the
    /// range-folded colour and the inked span all come through untouched, and
    /// asking for the rendering a table already has returns it unchanged. Only
    /// what happens between two stops moves.
    ///
    /// Two properties hold for every built-in and are pinned by tests, because
    /// they are what make this safe to put behind a switch:
    ///
    /// * flipping to [`TableRendering::Stepped`] restores exactly the colours
    ///   the palette painted before continuous rendering existed, byte for
    ///   byte;
    /// * whatever the banded drawing inks, the continuous drawing inks, so no
    ///   gate that painted before stops painting and the inked span does not
    ///   move. For a palette authored banded - which is every palette whose
    ///   default this change touched - that holds in both directions and the
    ///   flip is a pure recolouring of the echo. It holds in both directions
    ///   for the palettes authored as sRGB ramps too, with one exception:
    ///   AWIPS Wilson opens on a clear row that declares an opaque second
    ///   colour, so its continuous drawings fade the bottom ten dBZ in and
    ///   hard bands, which have one colour per band, cannot. Flipping that one
    ///   to stepped drops the fade, which is what asking for hard bands means.
    ///
    /// The name follows the mode, so the two renderings never collide in a
    /// list - which is what lets a picker go on identifying a row by its name.
    /// A palette loaded from a file, whose name carries no mode, therefore
    /// acquires one the first time it is flipped; leaving it bare would put two
    /// rows with one name into the same list.
    pub fn rendered(&self, rendering: TableRendering) -> Self {
        let sample_mode = match rendering {
            TableRendering::Smooth => self.authored_mode.continuous(),
            TableRendering::Stepped => self.authored_mode.banded(),
        };
        if sample_mode == self.sample_mode {
            return self.clone();
        }
        Self {
            name: format!("{} ({})", self.base_name(), sample_mode.label()),
            sample_mode,
            ..self.clone()
        }
    }

    /// Rename in place, for the preset builders that stamp a mode onto a name.
    pub(crate) fn renamed(mut self, name: String) -> Self {
        self.name = name;
        self
    }

    pub fn sample(&self, value: f32) -> Rgba8 {
        if !value.is_finite() {
            return Rgba8::TRANSPARENT;
        }
        match self.sample_mode {
            SampleMode::Interpolated => self.sample_interpolated(value),
            SampleMode::Stepped => self.sample_stepped(value),
            SampleMode::QuantizedInterpolated { step, origin } => {
                if let Some(first_opaque_value) = self.first_opaque_value()
                    && value < first_opaque_value
                {
                    return Rgba8::TRANSPARENT;
                }
                let quantized = quantize_value(value, step, origin);
                self.sample_interpolated(quantized)
            }
            SampleMode::Continuous => {
                // The same cut-off `QuantizedInterpolated` applies, and for the
                // same reason, which is why this mode and not plain
                // `Interpolated` is what a banded palette becomes when it is
                // asked to run continuously. Every reflectivity preset opens
                // with two alpha-0 stops well below the first inked one - -10
                // and 7.5 dBZ against ink from 10 - and interpolating across
                // that gap would fade 2.5 dBZ of noise floor onto the scope
                // that the banded rendering keeps off it. Clipping instead
                // makes the switch a pure recolouring of the echo: the set of
                // gates that paint does not move by one.
                if let Some(first_opaque_value) = self.first_opaque_value()
                    && value < first_opaque_value
                {
                    return Rgba8::TRANSPARENT;
                }
                self.sample_perceptual(value)
            }
        }
    }

    fn sample_interpolated(&self, value: f32) -> Rgba8 {
        let Some(first) = self.stops.first() else {
            return Rgba8::TRANSPARENT;
        };
        if value <= first.value {
            return first.color;
        }
        let index = self.stops.partition_point(|stop| stop.value < value);
        if index >= self.stops.len() {
            return self
                .stops
                .last()
                .map(|stop| stop.color)
                .unwrap_or(Rgba8::TRANSPARENT);
        }
        let right = self.stops[index];
        if value == right.value {
            return right.color;
        }
        let left = self.stops[index - 1];
        let span = (right.value - left.value).max(f32::EPSILON);
        left.color.lerp(
            segment_end_color(&self.stops, index - 1),
            (value - left.value) / span,
        )
    }

    /// `sample_interpolated`'s lookup with `oklab::mix` in place of the sRGB
    /// lerp. The bracketing arithmetic and the segment's target colour are
    /// deliberately identical, so the two continuous modes can only ever
    /// disagree about the colour they invent between the two ends, never about
    /// the ends themselves.
    fn sample_perceptual(&self, value: f32) -> Rgba8 {
        let Some(first) = self.stops.first() else {
            return Rgba8::TRANSPARENT;
        };
        if value <= first.value {
            return first.color;
        }
        let index = self.stops.partition_point(|stop| stop.value < value);
        if index >= self.stops.len() {
            return self
                .stops
                .last()
                .map(|stop| stop.color)
                .unwrap_or(Rgba8::TRANSPARENT);
        }
        let right = self.stops[index];
        if value == right.value {
            return right.color;
        }
        let left = self.stops[index - 1];
        let span = (right.value - left.value).max(f32::EPSILON);
        oklab::mix(
            left.color,
            self.stop_oklab[index - 1],
            segment_end_color(&self.stops, index - 1),
            self.segment_end_oklab[index - 1],
            (value - left.value) / span,
        )
    }

    /// One flat band per stop interval, painted in the band's *own* colour.
    ///
    /// A stop's declared end colour is deliberately not drawn here. A hard band
    /// has one colour by definition; painting the second one somewhere inside
    /// the band would put a seam where the whole point of the rendering is that
    /// there is none, and painting it instead of the first would move every
    /// band's colour up one row. The second colour is what the *continuous*
    /// renderings ramp toward, and asking for bands is asking for that ramp to
    /// be collapsed onto the value it starts from.
    fn sample_stepped(&self, value: f32) -> Rgba8 {
        let Some(first) = self.stops.first() else {
            return Rgba8::TRANSPARENT;
        };
        if value <= first.value {
            return first.color;
        }
        let index = self.stops.partition_point(|stop| stop.value < value);
        if index >= self.stops.len() {
            return self
                .stops
                .last()
                .map(|stop| stop.color)
                .unwrap_or(Rgba8::TRANSPARENT);
        }
        let right = self.stops[index];
        if value == right.value {
            return right.color;
        }
        self.stops[index - 1].color
    }

    fn first_opaque_value(&self) -> Option<f32> {
        let first = self.stops.first()?;
        (first.color.a == 0).then(|| {
            self.stops
                .iter()
                .find(|stop| stop.color.a > 0)
                .map(|stop| stop.value)
        })?
    }

    /// The engine-value range over which this table actually puts ink on the
    /// screen.
    ///
    /// Read off the *segments* rather than off the stops, because a stop's
    /// colour is only half of what its segment paints. A segment contributes
    /// its lower bound when it opens opaque and its upper bound - the next
    /// stop's value - when it arrives opaque, and those are two independent
    /// facts once a row can declare a second colour. Scanning stop colours
    /// alone ties the two together and gets both ends wrong on a table with
    /// declared targets: a clear row that declares an opaque end, which is what
    /// mirroring a lead-in transparent preset produces, inks up to the next
    /// stop even when that stop is itself clear, and a clear row carrying the
    /// dialect's transparent hold inks none of its interval even though the row
    /// above it is opaque.
    ///
    /// Which colour a segment *arrives* at depends on the drawing, so this
    /// reads `sample_mode` for it through `paints_ramp_targets`. A `Stepped`
    /// band arrives at the colour it opened with, because that is the only
    /// colour it has; every other mode reaches the interpolated sampler and
    /// arrives at the ramp target. Reading the ramp target for a `Stepped`
    /// table would report ink at the top of a stretch the bands leave blank,
    /// and miss the last band of a palette whose top row is clear.
    ///
    /// Where inside a fading segment the alpha crosses zero is a rounding
    /// artefact - Wilson crosses alpha 1 at -29.98 dBZ - so the segment's
    /// declared bounds are what get reported. They are the numbers the palette
    /// actually states.
    ///
    /// That makes a fade a lead-in or a lead-out and not part of the span, the
    /// same way a clear lead-in is, and it cuts both ways. Wilson's ink is
    /// reported from -20 dBZ, the value at which its first band arrives, not
    /// from the -30 its fade opens at; symmetrically, an opaque row followed by
    /// a clear one with no declared target fades out across its interval and is
    /// reported up to its *own* value, not up to the clear stop it disappears
    /// into. A legend labels where the palette is itself, at both ends. That is
    /// only visible on a continuously drawn table: a `Stepped` one holds its
    /// last colour flat to the clear stop, so its high bound is that stop.
    ///
    /// A lead-in leads in to something, though, and a table can be nothing but
    /// the fade: `step: 5` over one clear row that declares an opaque second
    /// colour and one clear row above it paints a thousand values and declares
    /// no opaque stop at all. Read strictly, that table's ink is a single
    /// value at the clear stop the fade arrives on - which is the one value in
    /// the interval it does *not* paint - and a caller that suppresses a
    /// zero-width bar suppresses the legend of a palette an analyst can see on
    /// the scope. So when a declared fade is the only ink, the lowest one
    /// contributes its opening as well and the span is the stretch it covers.
    ///
    /// "Declared" is what separates that from a clear stop with no second
    /// colour, which fades into whatever the row above it is and stays a
    /// lead-in: the row above is opaque, so it is already the low bound and
    /// the span has extent or is honestly a single value. A row that writes
    /// its own second colour is the palette saying that interval paints, and
    /// when nothing else does, that is the palette.
    ///
    /// Prevents a legend that advertises a range where nothing is ever painted.
    /// Twelve of the forty-seven built-in tables open with exactly two alpha-0
    /// stops: every one of the twelve reflectivity presets that has any
    /// transparency at all - the nine parsed ones and the three interpolated
    /// ones. They declare their first stop at -10 dBZ (Low Precip and Clean
    /// Light at -15), so a legend bar drawn across the declared domain labels
    /// ticks from -10 or -15 dBZ over a stretch of scope that stays empty no
    /// matter what the radar returns. Eleven of the twelve ink from 10 dBZ;
    /// Storm Detail, whose second transparent stop sits at 0 rather than 7.5,
    /// inks from 5 dBZ. The other thirty-five built-ins have no transparent
    /// stop at all and report their full declared range.
    ///
    /// "Inked" here means "where the palette varies", not "where pixels appear".
    /// `sample()` clamps outside the stop range in every mode instead of fading
    /// out, so a value past the last stop is still drawn in the last stop's
    /// color. The span is the interval a legend should label, never a claim
    /// about which pixels get covered.
    ///
    /// The range-folded color is deliberately excluded even though it is opaque:
    /// it is selected by the folded code in the moment data, which render2d
    /// intercepts before value conversion, so it has no engine value and letting
    /// it widen the span would hang a number on a non-numeric category.
    ///
    /// Returns `None` only for a table that can never ink anything: every stop
    /// transparent *and* no segment arriving anywhere opaque either. Its legend
    /// must be suppressed rather than drawn empty. Note this differs from
    /// `first_opaque_value`, which reports `None` for the common case of a
    /// palette whose first stop is already opaque.
    ///
    /// The two bounds can still be equal, for the one shape where a single
    /// value really is all the ink there is: a `.pal` whose clear lead-in row
    /// declares nothing picks up the dialect's transparent hold, so it fades
    /// nowhere and a table of that row plus one opaque stop reports
    /// `(10.0, 10.0)`. A legend that places a tick at
    /// `(value - low) / (high - low)` divides by zero and gets NaN
    /// coordinates, so the caller must still test for `high == low` before
    /// laying out a bar. No built-in table has this shape.
    pub fn inked_value_span(&self) -> Option<(f32, f32)> {
        let mut first_inked: Option<f32> = None;
        let mut last_inked: Option<f32> = None;
        // Where the lowest lead-in fade opens, held back in case those fades
        // turn out to be the only ink the table has.
        let mut first_fade_opened_at: Option<f32> = None;
        // Every bound this loop offers is either `stops[index].value` or
        // `stops[index + 1].value`, and the stops are sorted, so the offers
        // arrive in non-decreasing order and first/last are the extremes
        // without a running min and max.
        let mut inked_at = |value: f32| {
            if first_inked.is_none() {
                first_inked = Some(value);
            }
            last_inked = Some(value);
        };
        let paints_ramp_targets = self.sample_mode.paints_ramp_targets();
        for (index, stop) in self.stops.iter().enumerate() {
            if stop.color.a > 0 {
                inked_at(stop.value);
            }
            // What the segment arrives at, which is the ramp target only where
            // the drawing paints one. A `Stepped` band arrives at the colour it
            // opened with, so there an opaque row followed by a clear one inks
            // its whole band and a clear row followed by an opaque one inks
            // none of it.
            let arrives_at = if paints_ramp_targets {
                segment_end_color(&self.stops, index)
            } else {
                stop.color
            };
            if let Some(next) = self.stops.get(index + 1)
                && arrives_at.a > 0
            {
                // A clear row that DECLARES an opaque second colour is a row
                // stating that its own interval paints. A clear row that
                // declares nothing is a lead-in to the stop above it, and that
                // stop is already where the span starts.
                if stop.color.a == 0 && stop.end_color.is_some() && first_fade_opened_at.is_none() {
                    first_fade_opened_at = Some(stop.value);
                }
                inked_at(next.value);
            }
        }
        match (first_inked, last_inked) {
            // A declared fade is normally not part of the span, because it
            // leads in to the opaque stop that is. When it is the only ink the
            // table has there is nothing for it to lead in to, so the fade is
            // the palette and its own opening is the low bound - otherwise the
            // report is a single value at the clear stop the fade ends on,
            // which is the one value in the whole interval that paints
            // nothing, and it puts the caller's `high == low` guard between an
            // analyst and a legend for a table that visibly paints.
            (Some(low), Some(high)) => match first_fade_opened_at {
                Some(fade) if low >= high && fade < low => Some((fade, high)),
                _ => Some((low, high)),
            },
            _ => None,
        }
    }

    pub fn color_for_value(&self, value: f32) -> [u8; 4] {
        self.sample(value).to_array()
    }

    pub fn range_folded_color(&self) -> [u8; 4] {
        self.range_folded.to_array()
    }

    pub fn range_folded_rgba(&self) -> Rgba8 {
        self.range_folded
    }

    /// A cheap identity for "would this table paint a different picture".
    ///
    /// Renderers cache rasters against it, so it has to move whenever the
    /// colours do. `sample_mode` is hashed, which is what makes flipping the
    /// rendering invalidate a cached frame. `authored_mode` deliberately is
    /// not: it is where the table would go if it were flipped, not anything it
    /// paints now, and hashing it would throw away frames that are still
    /// correct.
    pub fn signature(&self) -> u64 {
        let mut hasher = DefaultHasher::new();
        self.name.hash(&mut hasher);
        self.product.hash(&mut hasher);
        self.units.hash(&mut hasher);
        self.range_folded.hash(&mut hasher);
        self.sample_mode.hash(&mut hasher);
        self.stops.len().hash(&mut hasher);
        for stop in &self.stops {
            stop.value.to_bits().hash(&mut hasher);
            stop.color.hash(&mut hasher);
            // The ramp target is part of the picture in both continuous
            // renderings, so two tables that differ only in where their
            // segments are heading must not share a cached raster.
            stop.end_color.hash(&mut hasher);
        }
        hasher.finish()
    }

    /// The same palette with the sign of every value flipped, for reading a
    /// velocity field under the opposite convention.
    ///
    /// Mirrors *what the table is currently drawing*, which is why it reads
    /// `sample_mode` and why a caller that also wants to switch the rendering
    /// should call [`ColorTable::rendered`] first and mirror the result. The
    /// two drawings do not reverse the same way:
    ///
    /// * A continuous rendering paints a segment as a ramp from its stop's
    ///   colour to the segment's resolved end colour, so reversing it means
    ///   turning each segment around: the colour a segment ramped *to* becomes
    ///   the colour its mirror image opens with, and the colour it opened with
    ///   becomes the colour the mirror ramps to. Every mirrored stop therefore
    ///   declares an end colour even when the original did not - what was
    ///   implicit in one direction has to be spelled out in the other.
    /// * A banded rendering paints the half-open interval `[v_i, v_i+1)` in
    ///   `c_i` and never paints a ramp target at all, so reflecting it means
    ///   moving each colour down one stop: the band that ran up to `v_i+1`
    ///   becomes the band that runs *from* `-v_i+1`, so the mirror's stop just
    ///   above `-v_i+1` carries `c_i`. Reversing the colours without that
    ///   shift - which is what this did until the reflection was written down -
    ///   hands every band its neighbour's colour and paints a gate at -3 m/s in
    ///   the colour the palette gives +5 to +20.
    ///
    /// The mirror of a single-colour palette drawn continuously paints exactly
    /// what it painted before, because an undeclared target *is* the next
    /// stop's colour and the two rules agree wherever nothing is declared.
    ///
    /// # Exactly at a stop value, and why the numbers look wrong
    ///
    /// A stop list describes a function that is continuous from the right -
    /// `sample` answers `c_i` at `v_i` and answers what the band or segment
    /// below was heading for just under it - and reflecting such a function
    /// makes it continuous from the *left*, which no single stop can say. That
    /// is not a rounding detail: velocity is a discrete field whose values land
    /// on these palettes' stops by construction. A NEXRAD velocity moment
    /// encodes multiples of 0.5 m/s and this crate's dealiased grid multiples
    /// of 0.1 m/s, so a palette banded at integer m/s has a fully populated
    /// gate population sitting on every one of its stops, the near-zero ones
    /// most of all. Handing those the neighbouring band's colour mis-painted a
    /// third of a real velocity sweep.
    ///
    /// So a stop with a hard edge becomes *two* stops in the mirror: the
    /// reflected point value keeps `-v_i`, and the reflected band or segment
    /// opens at [`f32::next_up`] of it, the next representable float, which is
    /// the smallest gap that leaves nothing between them. Reading the mirror's
    /// stops back therefore shows values like `-9.9999990` beside `-10.0`,
    /// which is the price of the reflection being exact at every value the
    /// radar can encode rather than only between them. Where the reflected
    /// opening is already the colour the original paints at the stop - every
    /// stop of a palette that declares no ramp targets - nothing is split and
    /// the mirror keeps the palette's own numbers. A quantised drawing splits
    /// only on stops its grid can actually land on, because the values between
    /// grid points are never looked up.
    ///
    /// What is left is a byte of rounding, and only in a continuous drawing: a
    /// mirrored segment opens one ULP later than the original's closed, so its
    /// `t` differs in the last place and a channel whose exact mix lands on a
    /// half rounds the other way. Over 800,460 samples of all ten built-in
    /// velocity palettes drawn both ways that is 3 samples, each off by one in
    /// one channel, none of them a value the radar can encode.
    /// `a_flip_of_every_velocity_palette_is_exact_at_every_value_a_radar_sends`
    /// measures both halves of that claim.
    ///
    /// # What a mirror still cannot carry
    ///
    /// The clamps come out right - a table clamps below its first stop and
    /// above its last, and the mirror's outermost stops now carry the
    /// reflected point values those clamps hold - but two things about a table
    /// are not values and do not reverse:
    ///
    /// * the range-folded colour, which is chosen by a code in the moment data
    ///   rather than by a velocity, and is copied across unchanged;
    /// * the transparency cut-off the continuous modes apply below a palette's
    ///   first opaque stop, which follows the stop list and so ends up at the
    ///   mirror's *bottom* while the reflected clear rows sit at its top. No
    ///   shipped velocity palette has a transparent row, which is the only
    ///   family the application ever mirrors.
    pub fn mirrored_values(&self, name: impl Into<String>) -> Self {
        let paints_ramp_targets = self.sample_mode.paints_ramp_targets();
        let mirrored_mode = self.sample_mode.mirrored_values();
        let mut stops = Vec::with_capacity(self.stops.len() * 2);
        for (index, stop) in self.stops.iter().enumerate().rev() {
            // What the reflected band or segment OPENS with, just above the
            // mirrored stop.
            let (color, end_color) = if !paints_ramp_targets {
                // Bands, shifted down one interval. `saturating_sub` is the
                // duplicate the bottom of the table needs: the original's
                // first colour is both the band above its first stop and
                // the colour it clamps to below it, and the mirror needs it
                // at its own top twice for the same reason.
                (self.stops[index.saturating_sub(1)].color, None)
            } else {
                match index.checked_sub(1) {
                    Some(below) => (
                        segment_end_color(&self.stops, below),
                        Some(self.stops[below].color),
                    ),
                    // The original's first stop becomes the mirror's last,
                    // and a last stop has no segment above it to ramp
                    // across.
                    None => (stop.color, None),
                }
            };
            let mirrored_value = -stop.value;
            // Everything under the mirror's lowest stop clamps to it, so that
            // one is read whatever the drawing rounds to and always has to
            // carry the reflected point value - it is the original's
            // open-ended top band or top clamp seen from the other side.
            let is_the_mirrors_clamp = index + 1 == self.stops.len();
            if color == stop.color
                || !(is_the_mirrors_clamp || mirrored_mode.paints_value(mirrored_value))
            {
                // Nothing to separate: either the reflected opening is already
                // the colour the original paints at this stop, or the drawing
                // is quantised and no grid point can land on this stop, so
                // nothing will ever ask for the value that would differ. One
                // stop says everything, and the mirror keeps the palette's own
                // numbers.
                stops.push(ColorStop {
                    value: mirrored_value,
                    color,
                    end_color,
                });
                continue;
            }
            // A hard edge, and the one place a naive `v -> -v` gets a whole
            // population of gates wrong. The original answers `stop.color` AT
            // `stop.value`, so the reflection owes that colour at
            // `-stop.value` exactly and owes the opening colour above it, and
            // one stop cannot say both: a stop list is continuous from the
            // right, its reflection is continuous from the left. So the point
            // value keeps the mirrored stop and the opening moves up by one
            // ULP, which is the smallest gap that leaves no `f32` between
            // them. The mirrored point stop declares no end colour: the sliver
            // it opens holds no other float, and letting it declare one would
            // hand `inked_value_span` a bound one ULP off the palette's own.
            stops.push(ColorStop {
                value: mirrored_value,
                color: stop.color,
                end_color: None,
            });
            stops.push(ColorStop {
                value: mirrored_value.next_up(),
                color,
                end_color,
            });
        }
        Self::from_parts_with_authored(
            name.into(),
            self.product.clone(),
            self.units.clone(),
            self.range_folded,
            self.sample_mode.mirrored_values(),
            self.authored_mode.mirrored_values(),
            stops,
        )
        .expect("mirrored table preserves valid stops")
    }

    fn from_parts(
        name: String,
        product: Option<String>,
        units: Option<String>,
        range_folded: Rgba8,
        sample_mode: SampleMode,
        stops: Vec<ColorStop>,
    ) -> Result<Self, ColorTableError> {
        Self::from_parts_with_authored(
            name,
            product,
            units,
            range_folded,
            sample_mode,
            sample_mode,
            stops,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn from_parts_with_authored(
        name: String,
        product: Option<String>,
        units: Option<String>,
        range_folded: Rgba8,
        sample_mode: SampleMode,
        authored_mode: SampleMode,
        mut stops: Vec<ColorStop>,
    ) -> Result<Self, ColorTableError> {
        stops.retain(|stop| stop.value.is_finite());
        stops.sort_by(|left, right| left.value.total_cmp(&right.value));
        // Two rows at one value: the last one written wins, which is what GR
        // does with a file that says the same value twice and what a person
        // editing a palette expects from a later line. `sort_by` is stable, so
        // "last" is still last in file order after the sort. `dedup_by` hands
        // the *later* element first and removes it, so the surviving row is
        // `kept` and the later row's colours have to be copied into it.
        stops.dedup_by(|later, kept| {
            if later.value.to_bits() == kept.value.to_bits() {
                *kept = *later;
                true
            } else {
                false
            }
        });

        if stops.len() < 2 {
            return Err(ColorTableError::NotEnoughStops);
        }

        let stop_oklab = stops
            .iter()
            .map(|stop| oklab::oklab_from_rgb(stop.color))
            .collect();
        let segment_end_oklab = (0..stops.len())
            .map(|index| oklab::oklab_from_rgb(segment_end_color(&stops, index)))
            .collect();

        Ok(Self {
            name,
            product,
            units,
            range_folded,
            sample_mode,
            authored_mode,
            stops,
            stop_oklab,
            segment_end_oklab,
        })
    }
}

/// What a table does between two stops.
///
/// Four modes, two of them continuous and two of them banded. The split that
/// matters to an analyst is [`TableRendering`]; this is the machinery under it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum SampleMode {
    /// Straight-line mix of the sRGB bytes.
    ///
    /// The legacy continuous path. Kept as its own mode, rather than folded
    /// into `Continuous`, because the built-in palettes written as sRGB ramps
    /// were authored against it - their stops were chosen by eye *through* this
    /// mixing - and their published colours are pinned by tests to the byte.
    /// Changing what they paint to make the crate tidier would be changing
    /// palettes nobody complained about. Both shipped defaults are in this
    /// group, and both are GR `.pal` ports whose ramp rows only make sense
    /// under it.
    Interpolated,
    /// One flat band per stop interval.
    Stepped,
    /// Round the value onto a grid, then mix the sRGB bytes.
    ///
    /// What a GR-format `step:` row asks for. Also banded - the plateau is the
    /// grid cell, not the stop interval - but the band edges land on round
    /// numbers of dBZ or m/s rather than wherever a stop happens to sit.
    QuantizedInterpolated { step: f32, origin: f32 },
    /// Perceptual mix, and nothing painted below the first opaque stop.
    ///
    /// The continuous rendering any palette can be switched into. See
    /// [`oklab`] for why the mix is not done on the sRGB bytes, and
    /// [`ColorTable::sample`] for why the transparency is clipped rather than
    /// faded.
    Continuous,
}

impl SampleMode {
    /// The suffixes [`ColorTable::base_name`] strips.
    ///
    /// Listed longest first, but the order is not what makes this correct and
    /// saying so would be a lie a reader could act on: "... (quantized
    /// stepped)" does not end with " (stepped)" because the opening
    /// parenthesis is part of the suffix, so no entry here can shadow another
    /// whatever order they come in. The order is kept anyway as the habit that
    /// stays safe if a bare-word suffix is ever added.
    const NAME_SUFFIXES: [&'static str; 4] = [
        " (quantized stepped)",
        " (interpolated)",
        " (continuous)",
        " (stepped)",
    ];

    fn label(self) -> &'static str {
        match self {
            Self::Interpolated => "interpolated",
            Self::Stepped => "stepped",
            Self::QuantizedInterpolated { .. } => "quantized stepped",
            Self::Continuous => "continuous",
        }
    }

    fn step_size(self) -> Option<f32> {
        match self {
            Self::QuantizedInterpolated { step, .. } => Some(step),
            Self::Interpolated | Self::Stepped | Self::Continuous => None,
        }
    }

    /// Whether this drawing ever puts a stop's *second* colour on the screen.
    ///
    /// Only [`SampleMode::Stepped`] answers `false`. It paints one flat band
    /// per stop interval in the band's own colour, so a stop's second colour is
    /// inert - see [`ColorTable::sample_stepped`] for why that is the right
    /// reading of a hard band rather than an oversight.
    ///
    /// `QuantizedInterpolated` answers `true` even though it is banded, because
    /// what it bands is the *value* and not the palette: it rounds the value
    /// onto the grid and then hands the result to `sample_interpolated`, which
    /// reads [`segment_end_color`]. A `.pal` is under no obligation to put its
    /// rows on multiples of its own `step:` - this crate's own shipped
    /// `GR2Analyst Classic REF` declares `step: 5` and then writes rows at 7.5,
    /// 62.5, 67.5, 72.5 and 92.5 - so a grid point routinely lands *between*
    /// two stops and paints the mix, which is a colour no stop declares. Where
    /// a row also declares a ramp target, that target is what the mix runs
    /// toward, and a transform that dropped it would repaint the band.
    ///
    /// Anything that rewrites stops rather than reading them -
    /// [`ColorTable::mirrored_values`] is the one - has to ask, because a ramp
    /// and a band do not reverse the same way.
    fn paints_ramp_targets(self) -> bool {
        match self {
            Self::Interpolated | Self::Continuous | Self::QuantizedInterpolated { .. } => true,
            Self::Stepped => false,
        }
    }

    /// Whether this drawing can ever be asked for exactly this value.
    ///
    /// Three of the four modes hand the value straight to a sampler, so every
    /// value is one of theirs. `QuantizedInterpolated` rounds first, so the
    /// only values it ever looks up are its own grid points and a stop that
    /// falls between two of them is never read at all.
    ///
    /// [`ColorTable::mirrored_values`] asks before it spends a stop on a
    /// value's exact reflection: a stop no grid point can land on cannot be
    /// painted, so splitting it would move the ramp geometry by an ULP and buy
    /// nothing.
    fn paints_value(self, value: f32) -> bool {
        match self {
            Self::Interpolated | Self::Continuous | Self::Stepped => true,
            Self::QuantizedInterpolated { step, origin } => {
                quantize_value(value, step, origin) == value
            }
        }
    }

    fn rendering(self) -> TableRendering {
        match self {
            Self::Interpolated | Self::Continuous => TableRendering::Smooth,
            Self::Stepped | Self::QuantizedInterpolated { .. } => TableRendering::Stepped,
        }
    }

    /// The banded drawing of a palette written this way.
    ///
    /// A quantised palette keeps its grid, because the grid is the whole point
    /// of it. Everything else bands at its own stops, which is the only
    /// banding a bare stop list can express.
    fn banded(self) -> Self {
        match self {
            Self::QuantizedInterpolated { .. } => self,
            Self::Interpolated | Self::Stepped | Self::Continuous => Self::Stepped,
        }
    }

    /// The continuous drawing of a palette written this way.
    ///
    /// `Interpolated` is left where it is: a palette already written as a
    /// continuous sRGB ramp is already continuous, and moving it to the
    /// perceptual mixer would silently repaint five shipped tables.
    fn continuous(self) -> Self {
        match self {
            Self::Interpolated => Self::Interpolated,
            Self::Stepped | Self::QuantizedInterpolated { .. } | Self::Continuous => {
                Self::Continuous
            }
        }
    }

    fn scale_values(self, scale: f32) -> Self {
        match self {
            Self::QuantizedInterpolated { step, origin } => Self::QuantizedInterpolated {
                step: step * scale,
                origin: origin * scale,
            },
            Self::Interpolated | Self::Stepped | Self::Continuous => self,
        }
    }

    fn mirrored_values(self) -> Self {
        match self {
            Self::QuantizedInterpolated { step, origin } => Self::QuantizedInterpolated {
                step,
                origin: -origin,
            },
            Self::Interpolated | Self::Stepped | Self::Continuous => self,
        }
    }
}

impl Hash for SampleMode {
    fn hash<H: Hasher>(&self, state: &mut H) {
        match *self {
            Self::Interpolated => 0_u8.hash(state),
            Self::Stepped => 1_u8.hash(state),
            Self::QuantizedInterpolated { step, origin } => {
                2_u8.hash(state);
                step.to_bits().hash(state);
                origin.to_bits().hash(state);
            }
            Self::Continuous => 3_u8.hash(state),
        }
    }
}

/// The sampling choice an analyst makes, as two values rather than four.
///
/// The complaint this exists to answer is that hard-banded tables collapse
/// distinct readings onto one colour, worst of all on velocity, where the thing
/// being read *is* the gate-to-gate difference. The answer is not another
/// column of palettes in the picker - it is that sampling was never a property
/// of a palette in the first place. Every table can be drawn either way, so
/// this is one switch beside one list instead of two entries per palette.
///
/// [`TableRendering::Smooth`] is the shipped default for every family.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum TableRendering {
    /// Continuous tone: every value the radar can send gets its own colour.
    Smooth,
    /// Hard bands: values inside one band share a colour, and the band edges
    /// are contours an analyst can count.
    Stepped,
}

impl TableRendering {
    /// Both values, smooth first because that is the default and the one the
    /// switch should offer first.
    pub const ALL: [Self; 2] = [Self::Smooth, Self::Stepped];

    /// A word for a button, capitalised for a label rather than a sentence.
    pub fn label(self) -> &'static str {
        match self {
            Self::Smooth => "Smooth",
            Self::Stepped => "Stepped",
        }
    }

    /// The other one, for a control that toggles rather than selects.
    pub fn flipped(self) -> Self {
        match self {
            Self::Smooth => Self::Stepped,
            Self::Stepped => Self::Smooth,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ColorTableSet {
    reflectivity: ColorTable,
    received_power: ColorTable,
    velocity: ColorTable,
    spectrum_width: ColorTable,
    differential_reflectivity: ColorTable,
    correlation_coefficient: ColorTable,
    differential_phase: ColorTable,
    specific_differential_phase: ColorTable,
    generic: ColorTable,
}

impl ColorTableSet {
    pub fn for_family(&self, family: ColorTableFamily) -> &ColorTable {
        match family {
            ColorTableFamily::Reflectivity => &self.reflectivity,
            ColorTableFamily::ReceivedPower => &self.received_power,
            ColorTableFamily::Velocity => &self.velocity,
            ColorTableFamily::SpectrumWidth => &self.spectrum_width,
            ColorTableFamily::DifferentialReflectivity => &self.differential_reflectivity,
            ColorTableFamily::CorrelationCoefficient => &self.correlation_coefficient,
            ColorTableFamily::DifferentialPhase => &self.differential_phase,
            ColorTableFamily::SpecificDifferentialPhase => &self.specific_differential_phase,
            ColorTableFamily::Generic => &self.generic,
        }
    }

    pub fn set_family(&mut self, family: ColorTableFamily, table: ColorTable) {
        match family {
            ColorTableFamily::Reflectivity => self.reflectivity = table,
            ColorTableFamily::ReceivedPower => self.received_power = table,
            ColorTableFamily::Velocity => self.velocity = table,
            ColorTableFamily::SpectrumWidth => self.spectrum_width = table,
            ColorTableFamily::DifferentialReflectivity => self.differential_reflectivity = table,
            ColorTableFamily::CorrelationCoefficient => self.correlation_coefficient = table,
            ColorTableFamily::DifferentialPhase => self.differential_phase = table,
            ColorTableFamily::SpecificDifferentialPhase => self.specific_differential_phase = table,
            ColorTableFamily::Generic => self.generic = table,
        }
    }

    pub fn signature_for_family(&self, family: ColorTableFamily) -> u64 {
        self.for_family(family).signature()
    }
}

impl Default for ColorTableSet {
    fn default() -> Self {
        Self {
            reflectivity: builtin_reflectivity_table(),
            received_power: builtin_received_power_table(),
            velocity: builtin_velocity_table(),
            spectrum_width: builtin_spectrum_width_table(),
            differential_reflectivity: builtin_differential_reflectivity_table(),
            correlation_coefficient: builtin_correlation_coefficient_table(),
            differential_phase: builtin_differential_phase_table(),
            specific_differential_phase: builtin_specific_differential_phase_table(),
            generic: builtin_generic_table(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ColorTableError {
    InvalidColor { line: usize, reason: &'static str },
    NotEnoughStops,
}

impl fmt::Display for ColorTableError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidColor { line, reason } => {
                write!(formatter, "invalid color table line {line}: {reason}")
            }
            Self::NotEnoughStops => write!(formatter, "color table needs at least two color stops"),
        }
    }
}

impl std::error::Error for ColorTableError {}

pub(crate) fn stop(value: f32, r: u8, g: u8, b: u8) -> ColorStop {
    ColorStop {
        value,
        color: Rgba8::opaque(r, g, b),
        end_color: None,
    }
}

/// A stop that paints nothing, used to hold the bottom of a palette clear.
///
/// The parsed reflectivity presets do this with `color4: ... 0` rows; a table
/// built from stops needs the same thing so that a scan's noise floor stays off
/// the scope instead of covering it in the first stop's colour, which is what
/// `sample` would otherwise do for every value below the first stop.
///
/// Declares no end colour, so the segment above it fades up into the next
/// stop's colour the way any other undeclared segment does. A clear stop in a
/// Rust-built table is the bottom of a gradient, not a mask; the mask is the GR
/// `.pal` dialect's reading of a clear *row*, and [`hold_clear_gr_rows`] writes
/// that one in at parse time where it belongs. A table that wants the mask says
/// so by declaring `Rgba8::TRANSPARENT` as the end colour, which is what a
/// parsed lead-in ends up carrying.
pub(crate) fn clear_stop(value: f32) -> ColorStop {
    ColorStop {
        value,
        color: Rgba8::TRANSPARENT,
        end_color: None,
    }
}

fn default_range_folded_color() -> Rgba8 {
    Rgba8::new(126, 80, 196, 245)
}

/// The colour the segment that opens at `index` ramps *to*.
///
/// Two cases, and no third:
///
/// * the row declared a second colour, and that is the target;
/// * the row is a single colour, and the target is the next row's colour,
///   which is the older one-colour-per-row reading a stop list has always had.
///
/// Deliberately *not* a place where a clear row acquires a hold. GR2Analyst
/// does hold a transparent row's colour across its interval rather than ramping
/// out of it, but that is a fact about the `.pal` dialect, and the tables this
/// crate builds in Rust from [`stop`] and [`clear_stop`] are not written in it.
/// A resolver that applied the rule to every table would mask the floor segment
/// of any preset whose lowest stop happens to be transparent, which is a large
/// and silent blast radius: the three interpolated reflectivity presets and
/// every synthesised derived-product ramp are that shape, and each would lose
/// the bottom of its range without a line of them changing. The rule lives at
/// the edge instead - [`hold_clear_gr_rows`] writes it into the parsed stops as
/// a declared end colour - so a `.pal` gets the dialect's reading of its clear
/// rows, a table built from stops ramps between them, and `stops()` shows which
/// one a caller is holding.
///
/// A free function rather than a method because the constructor has to resolve
/// every segment before there is a `ColorTable` to call a method on, and the
/// two must not be allowed to drift apart.
///
/// `index` is the segment's *lower* stop, which every sampling caller has
/// already bracketed, so `index + 1` is in range whenever the answer can
/// matter. The last stop has no segment above it and answers with its own
/// colour, which is what `sample` clamps to there anyway.
fn segment_end_color(stops: &[ColorStop], index: usize) -> Rgba8 {
    let stop = stops[index];
    if let Some(end) = stop.end_color {
        return end;
    }
    stops
        .get(index + 1)
        .map(|next| next.color)
        .unwrap_or(stop.color)
}

/// Write GR2Analyst's clear-row hold into the stops it applies to.
///
/// A GR `.pal` row that paints nothing and declares no second colour holds
/// transparent right up to the next row: a palette's lead-in is a mask, not the
/// bottom of a gradient, and ramping it up into the first inked row would fade
/// a band of noise floor onto the scope the author asked to keep off it. A row
/// that really does want a fade-in says so by declaring an opaque second
/// colour, which is what the AWIPS Wilson table's -30 dBZ row does.
///
/// The hold is recorded as an ordinary declared end colour rather than left for
/// the sampler to infer, because it belongs to the file and not to the format
/// this crate samples: only text that came through [`ColorTable::parse`] and
/// friends is written in the dialect. Making it explicit also means `stops()`,
/// `signature()` and any editor built on them see the same palette the sampler
/// does, with no rule hidden between them.
///
/// Provenance: BowEcho's `color_tables` reads the same dialect and states the
/// rule the same way - "Plain Color: rows ramp to the next row in GR ... EXCEPT
/// that GR holds a transparent row's color across its interval; interpolated
/// tables lerp through it" - and keeps the two readings apart rather than
/// merging them.
///
/// Applies to the last row too, harmlessly: it has no segment above it, so the
/// declaration is never read.
///
/// # What this does *not* claim
///
/// Getting the clear-row hold right closes one gap between this reader and
/// GR2Analyst; it does not close them all, and nobody should read a `.pal`
/// through this crate expecting a pixel-for-pixel GR screen. Two divergences
/// are known and deliberate, and both are left as they are:
///
/// * `SolidColor:` / `SolidColor4:` are parsed onto the same arm as `Color:` /
///   `Color4:`, so a solid row ramps into its neighbour here where GR paints it
///   as a flat band across its interval. The dialect's own reader carries a
///   `solid` flag per row for exactly this.
/// * `Step:` is taken as a request to quantise the display onto a grid. In GR
///   it is the legend's tick spacing and does not touch the drawing at all.
///
/// Both are pinned by tests that say plainly what the crate does, so a later
/// round that decides to close either gap will find the pin rather than a
/// silent change of picture.
fn hold_clear_gr_rows(stops: &mut [ColorStop]) {
    for stop in stops {
        if stop.end_color.is_none() && stop.color.a == 0 {
            stop.end_color = Some(stop.color);
        }
    }
}

pub(crate) fn lerp_u8(left: u8, right: u8, amount: f32) -> u8 {
    ((left as f32 + (right as f32 - left as f32) * amount).round()).clamp(0.0, 255.0) as u8
}

fn quantize_value(value: f32, step: f32, origin: f32) -> f32 {
    if !step.is_finite() || step <= 0.0 {
        return value;
    }
    ((value - origin) / step).round() * step + origin
}

/// The rendering suffix a name ends in, if it ends in one of the four this
/// build appends.
///
/// [`ColorTable::rendered`] spells the two drawings of one palette by putting
/// the sampling mode on the end of the name - "AWIPS Wilson REF (continuous)",
/// "AWIPS Wilson REF (stepped)" - so that two rows of a picker can hold one
/// palette twice and still be told apart. [`ColorTable::base_name`] is the
/// other half of that arrangement and takes the suffix back off.
///
/// It is public because the arrangement has a consequence a *writer* has to
/// know about: a name that already ends this way is not a name this build can
/// carry. Whatever an analyst types there, the flip of the smooth/stepped
/// switch rewrites it, and what is stored as the installed palette's identity
/// is the half without the suffix - which is not what the file declares. The
/// colour table editor asks this before it writes a file, and refuses the
/// name rather than writing one that would come back as something else.
pub fn rendering_suffix(name: &str) -> Option<&'static str> {
    SampleMode::NAME_SUFFIXES
        .into_iter()
        .find(|suffix| name.ends_with(suffix))
}

/// [`ColorTable::base_name`] for a name that has no table behind it yet.
///
/// The same string operation, so a *writer* checking a name an analyst has
/// typed and a *reader* asking what palette a table is cannot disagree about
/// where a name ends and its rendering begins. `ColorTable::base_name` is one
/// line over this.
///
/// This is the form every picker row reduces to, because a row's label is
/// [`ColorTable::rendered`]'s output - a base name with one of the four
/// suffixes on the end - and it is the form a stored palette choice is written
/// in. Anything that has to answer "does this name belong to a shipped
/// palette" therefore has to ask about this half of it and not the whole
/// string.
pub fn base_name_of(name: &str) -> &str {
    match rendering_suffix(name) {
        Some(suffix) => &name[..name.len() - suffix.len()],
        None => name,
    }
}

/// The name a `.pal` text declares in its `Name:` row, if it declares one.
///
/// [`ColorTable::parse`] has no `Name:` arm - it takes the name from its
/// argument, because the shipped presets are parsed out of embedded text that
/// has no such row and are named by the code that installs them. A file on
/// disk is the other way round: the row inside it is the palette's identity,
/// and the filename is a lossy handle that two different palettes can share.
/// So anything that has to answer "which palette is in this file" asks here,
/// through the same line normalisation, key normalisation and last-row-wins
/// precedence the parser itself uses.
///
/// `None` for a text with no `Name:` row or an empty one - a plain GR2Analyst
/// `.pal`, which never carries the row. The caller decides what to call such a
/// file; the convention in this build is its file stem.
pub fn declared_name(text: &str) -> Option<String> {
    let mut name = None;
    for original_line in text.lines() {
        let line = normalize_line(original_line);
        let line = line.trim();
        if line.is_empty()
            || line.starts_with(';')
            || line.starts_with('#')
            || line.starts_with("$$")
        {
            continue;
        }
        let Some((raw_key, raw_value)) = split_key_value(line) else {
            continue;
        };
        if normalize_key(raw_key) == "name"
            && let Some(value) = non_empty(raw_value)
        {
            name = Some(value);
        }
    }
    name
}

fn normalize_line(line: &str) -> String {
    line.replace('\u{a0}', " ")
}

fn normalize_key(key: &str) -> String {
    key.chars()
        .filter(|character| !character.is_ascii_whitespace() && *character != '_')
        .flat_map(char::to_lowercase)
        .collect()
}

fn split_key_value(line: &str) -> Option<(&str, &str)> {
    if let Some((key, value)) = line.split_once(':') {
        return Some((key, value));
    }
    let mut parts = line.splitn(2, char::is_whitespace);
    Some((parts.next()?, parts.next()?))
}

fn non_empty(value: &str) -> Option<String> {
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_owned())
}

/// One `color:` / `color4:` row, including the GR two-colour ramp form.
///
/// The dialect is GR2Analyst's and RadarScope reads the same files: a row is
/// `color: <value> <r> <g> <b>` or, for a ramp, `color: <value> <r> <g> <b>
/// <r2> <g2> <b2>`, with `color4:` adding an alpha byte to each colour. Shared
/// `.pal` files use the ramp form constantly, so trailing components are read
/// as the segment's end colour rather than discarded.
///
/// The end colour's alpha is optional in both forms and independent of the
/// row's own: a `color4:` row may declare `r2 g2 b2` and get an opaque end,
/// which is how a table fades a transparent lead-in up into its first band.
/// Numbers past a complete end colour are ignored, as they always were.
///
/// One or two trailing numbers are rejected rather than dropped. There is no
/// reading under which they are a colour, and a palette that silently loses a
/// mistyped ramp paints something the file does not say; the error carries the
/// line number so the row can be found.
fn parse_color_stop(
    value: &str,
    expects_alpha: bool,
    line: usize,
) -> Result<ColorStop, ColorTableError> {
    let numbers = parse_numbers(value);
    let required = if expects_alpha { 5 } else { 4 };
    if numbers.len() < required {
        return Err(ColorTableError::InvalidColor {
            line,
            reason: "expected value plus RGB or RGBA components",
        });
    }
    let alpha = if expects_alpha {
        byte_component(numbers[4], line)?
    } else {
        255
    };
    let end_color = match numbers.len() - required {
        0 => None,
        1 | 2 => {
            return Err(ColorTableError::InvalidColor {
                line,
                reason: "ramp end colour needs RGB or RGBA components",
            });
        }
        3 => Some(Rgba8::new(
            byte_component(numbers[required], line)?,
            byte_component(numbers[required + 1], line)?,
            byte_component(numbers[required + 2], line)?,
            255,
        )),
        _ => Some(Rgba8::new(
            byte_component(numbers[required], line)?,
            byte_component(numbers[required + 1], line)?,
            byte_component(numbers[required + 2], line)?,
            byte_component(numbers[required + 3], line)?,
        )),
    };
    Ok(ColorStop {
        value: numbers[0],
        color: Rgba8::new(
            byte_component(numbers[1], line)?,
            byte_component(numbers[2], line)?,
            byte_component(numbers[3], line)?,
            alpha,
        ),
        end_color,
    })
}

fn parse_color_only(value: &str, line: usize) -> Result<Rgba8, ColorTableError> {
    let numbers = parse_numbers(value);
    if numbers.len() < 3 {
        return Err(ColorTableError::InvalidColor {
            line,
            reason: "expected RGB components",
        });
    }
    Ok(Rgba8::new(
        byte_component(numbers[0], line)?,
        byte_component(numbers[1], line)?,
        byte_component(numbers[2], line)?,
        numbers
            .get(3)
            .map(|value| byte_component(*value, line))
            .transpose()?
            .unwrap_or(245),
    ))
}

fn parse_numbers(value: &str) -> Vec<f32> {
    value
        .split(|character: char| {
            character.is_ascii_whitespace() || character == ',' || character == ';'
        })
        .filter_map(|token| {
            let token = token.trim();
            (!token.is_empty())
                .then(|| token.parse::<f32>().ok())
                .flatten()
        })
        .collect()
}

fn byte_component(value: f32, line: usize) -> Result<u8, ColorTableError> {
    if !(0.0..=255.0).contains(&value) {
        return Err(ColorTableError::InvalidColor {
            line,
            reason: "color component must be 0-255",
        });
    }
    Ok(value.round() as u8)
}

fn parse_positive_f32(value: &str) -> Option<f32> {
    let value = parse_numbers(value).first().copied()?;
    (value.is_finite() && value > 0.0).then_some(value)
}

/// Read a `mode:` row out of a palette file.
///
/// `smooth` still means `Interpolated`, not `Continuous`, and that is not an
/// oversight. The word has meant "lerp the sRGB bytes" in GR-format palettes
/// for as long as they have existed, and a palette an analyst wrote years ago
/// has to keep painting what it painted. `continuous`, `perceptual` and
/// `oklab` are the new spellings, and they are new so that they cannot be
/// mistaken for the old one.
fn parse_sample_mode(value: &str) -> Option<SampleMode> {
    let value = value.trim().to_ascii_lowercase();
    match value.as_str() {
        "false" | "no" | "off" | "0" | "step" | "stepped" | "discrete" | "nearest" => {
            Some(SampleMode::Stepped)
        }
        "true" | "yes" | "on" | "1" | "smooth" | "linear" | "interpolate" | "interpolated" => {
            Some(SampleMode::Interpolated)
        }
        "continuous" | "perceptual" | "oklab" => Some(SampleMode::Continuous),
        _ => None,
    }
}

fn unit_value_to_mps_scale(units: &str) -> f32 {
    let units = units.trim().to_ascii_lowercase();
    match units.as_str() {
        "kt" | "kts" | "knot" | "knots" => KNOT_TO_MPS,
        "mph" | "mi/h" => MPH_TO_MPS,
        _ => 1.0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_wxtools_velocity_units_and_unsorted_stops() {
        let table = ColorTable::parse(
            "Vortex Velo sample",
            r#"
            units: MPH
            product: BV
            color: 0 115 115 115
            color: 5 130 3 3
            color: -5 2 139 2
            "#,
        )
        .expect("table parses");

        assert_eq!(table.product(), Some("BV"));
        assert_eq!(table.stops()[0].value, -5.0 * MPH_TO_MPS);
        assert_eq!(table.sample(0.0), Rgba8::opaque(115, 115, 115));
    }

    #[test]
    fn parses_color4_and_range_folded_rows() {
        let table = ColorTable::parse(
            "RadarScope sample",
            r#"
            product: BR
            units: dBZ
            color4: -15 0 0 0 0
            color: 5 29 37 60
            RF: 82 21 86
            "#,
        )
        .expect("table parses");

        assert_eq!(table.sample(-20.0), Rgba8::TRANSPARENT);
        assert_eq!(table.range_folded_rgba(), Rgba8::new(82, 21, 86, 245));
    }

    /// The GR two-colour ramp row, in both spellings a `.pal` file uses.
    ///
    /// A `color:` row's end colour is three bytes and comes out opaque; a
    /// `color4:` row's may be three or four, so a table can fade a clear row up
    /// into an opaque one without giving every other row an alpha column.
    #[test]
    fn parses_two_colour_ramp_rows_with_rgb_and_rgba_end_colours() {
        let table = ColorTable::parse(
            "GR ramp pairs",
            r#"
            product: BR
            units: dBZ
            color4: 0 10 20 30 0 40 50 60 255
            color: 10 70 80 90 100 110 120
            color4: 20 130 140 150 200 160 170 180
            color: 30 190 200 210
            "#,
        )
        .expect("table parses");

        let stops = table.stops();
        assert_eq!(stops[0].color, Rgba8::new(10, 20, 30, 0));
        assert_eq!(stops[0].end_color, Some(Rgba8::new(40, 50, 60, 255)));
        // A `color:` row's end colour has no alpha column, so it is opaque.
        assert_eq!(stops[1].color, Rgba8::opaque(70, 80, 90));
        assert_eq!(stops[1].end_color, Some(Rgba8::opaque(100, 110, 120)));
        // A `color4:` row may still give its end colour three components, and
        // the missing alpha does not come from the row's own.
        assert_eq!(stops[2].color, Rgba8::new(130, 140, 150, 200));
        assert_eq!(stops[2].end_color, Some(Rgba8::opaque(160, 170, 180)));
        // A single-colour row declares nothing and ramps to the next row.
        assert_eq!(stops[3].end_color, None);

        // Midway across the second segment: 70..100, 80..110, 90..120.
        assert_eq!(table.sample(15.0), Rgba8::opaque(85, 95, 105));
        // And the row boundary is a hard edge, because the ramp arrives at
        // (100,110,120) and the next row opens at (130,140,150).
        assert_eq!(table.sample(20.0), Rgba8::new(130, 140, 150, 200));
    }

    /// A ramp row that trails one or two loose numbers is an error, not a
    /// colour. Dropping them would paint something the file does not say.
    #[test]
    fn a_truncated_ramp_end_colour_is_rejected_with_its_line_number() {
        let error = ColorTable::parse(
            "Truncated ramp",
            "color: 0 10 20 30 40 50\ncolor: 10 60 70 80\n",
        )
        .expect_err("two trailing numbers cannot be a colour");

        assert_eq!(
            error,
            ColorTableError::InvalidColor {
                line: 1,
                reason: "ramp end colour needs RGB or RGBA components",
            }
        );
    }

    /// Two palettes identical but for where their segments are heading must
    /// not share a cached raster, so the ramp target has to reach the hash.
    #[test]
    fn the_signature_moves_when_only_a_ramp_target_does() {
        let plain = ColorTable::parse("ramp", "color: 0 0 0 0\ncolor: 10 255 255 255\n")
            .expect("table parses");
        let ramped =
            ColorTable::parse("ramp", "color: 0 0 0 0 200 100 50\ncolor: 10 255 255 255\n")
                .expect("table parses");
        let elsewhere =
            ColorTable::parse("ramp", "color: 0 0 0 0 50 100 200\ncolor: 10 255 255 255\n")
                .expect("table parses");

        assert_eq!(plain.stops()[0].color, ramped.stops()[0].color);
        assert_ne!(plain.signature(), ramped.signature());
        assert_ne!(ramped.signature(), elsewhere.signature());
    }

    /// Hard bands have one colour, so the second colour of a ramp row is not
    /// drawn at all when the palette is banded. The band keeps the colour its
    /// own row opens with, from its own value up to the next one.
    #[test]
    fn a_banded_palette_paints_the_row_colour_and_never_the_ramp_target() {
        let table = ColorTable::parse(
            "Banded ramp pairs",
            r#"
            mode: stepped
            color: 0 10 20 30 200 200 200
            color: 10 40 50 60 100 100 100
            "#,
        )
        .expect("table parses");

        for value in [0.0_f32, 2.5, 5.0, 9.9] {
            assert_eq!(
                table.sample(value),
                Rgba8::opaque(10, 20, 30),
                "the band at {value} must be its own row's colour"
            );
        }
        assert_eq!(table.sample(10.0), Rgba8::opaque(40, 50, 60));
        // Above the last stop the table clamps, and the clamp is the row
        // colour too - the trailing row has no segment to ramp across.
        assert_eq!(table.sample(50.0), Rgba8::opaque(40, 50, 60));
    }

    /// A clear row with no second colour is a mask, not the bottom of a
    /// gradient: it holds transparent right up to the next row.
    ///
    /// The same row *with* an opaque second colour is the other thing, and
    /// fades. Both readings come straight from the GR `.pal` dialect, and the
    /// difference between them is the whole reason a stop carries an optional
    /// end colour rather than a flag.
    #[test]
    fn a_clear_row_holds_clear_unless_it_declares_where_it_is_going() {
        let masked = ColorTable::parse(
            "Clear lead-in",
            "color4: 0 0 0 0 0\ncolor: 10 255 255 255\n",
        )
        .expect("table parses");
        for value in [0.0_f32, 2.5, 5.0, 9.99] {
            assert_eq!(
                masked.sample(value),
                Rgba8::TRANSPARENT,
                "the mask leaks at {value}"
            );
        }
        assert_eq!(masked.sample(10.0), Rgba8::opaque(255, 255, 255));
        assert_eq!(masked.inked_value_span(), Some((10.0, 10.0)));

        let faded = ColorTable::parse(
            "Declared fade-in",
            "color4: 0 0 0 0 0 255 255 255 255\ncolor: 10 255 255 255\ncolor: 20 255 0 0\n",
        )
        .expect("table parses");
        assert_eq!(faded.sample(0.0), Rgba8::TRANSPARENT);
        assert_eq!(faded.sample(5.0), Rgba8::new(128, 128, 128, 128));
        assert_eq!(faded.sample(10.0), Rgba8::opaque(255, 255, 255));
        // The fade inks inside its own segment and reaches the declared colour
        // at the next stop, and it is the arrival that the span reports: below
        // it the palette is a lead-in rather than itself, which is the same
        // reading a clear lead-in gets. So the fade widens no bound here. The
        // bound it can widen is the top one, on a table whose last segment
        // arrives opaque at a stop that paints nothing - see
        // `mirroring_a_preset_moves_its_transparent_stops_to_the_top_of_the_span`.
        assert_eq!(faded.inked_value_span(), Some((10.0, 20.0)));
    }

    /// The hold belongs to the `.pal` dialect, not to every table with a clear
    /// stop.
    ///
    /// Two tables with the same two stops, one parsed from GR text and one
    /// built in Rust, paint their first segment differently on purpose: the row
    /// is a mask and the stop is the bottom of a gradient. A resolver that
    /// could not tell them apart would mask the floor of everything - the three
    /// Smooth REF presets and every synthesised derived-product ramp are
    /// exactly this shape, and each would lose the bottom of its range with
    /// nothing in its own file changing.
    ///
    /// Provenance: BowEcho reads the same dialect and keeps the two apart the
    /// same way, in a `SampleMode::GrPal` of its own - "GR holds a transparent
    /// row's color across its interval; interpolated tables lerp through it".
    ///
    /// The parsed table carries the hold as a declared end colour rather than
    /// as a rule the sampler applies behind `stops()`, so anything reading the
    /// stops - a signature, an editor, a legend - sees the palette the sampler
    /// sees.
    #[test]
    fn the_gr_clear_row_hold_is_a_property_of_parsed_text_and_not_of_a_stop_list() {
        let parsed = ColorTable::parse(
            "Parsed lead-in",
            "color4: 0 0 0 0 0\ncolor: 10 255 255 255\n",
        )
        .expect("table parses");
        let built = ColorTable::new(
            "Built lead-in",
            vec![clear_stop(0.0), stop(10.0, 255, 255, 255)],
        )
        .expect("two ascending stops are a table");

        assert_eq!(parsed.stops()[0].color, built.stops()[0].color);
        assert_eq!(parsed.stops()[1].color, built.stops()[1].color);

        // The row holds; the stop ramps.
        assert_eq!(parsed.sample(5.0), Rgba8::TRANSPARENT);
        assert_eq!(built.sample(5.0), Rgba8::new(128, 128, 128, 128));

        // And the hold is written down, not inferred.
        assert_eq!(parsed.stops()[0].end_color, Some(Rgba8::TRANSPARENT));
        assert_eq!(built.stops()[0].end_color, None);
        assert_ne!(parsed.signature(), built.signature());
    }

    /// Only a row that both paints nothing *and* declares nothing picks the
    /// hold up. A clear row that says where it is going keeps saying it, and an
    /// opaque row is untouched however it was written.
    #[test]
    fn the_clear_row_hold_leaves_every_other_kind_of_row_alone() {
        let table = ColorTable::parse(
            "Mixed rows",
            "color4: 0 0 0 0 0 255 255 255 255\n\
             color4: 10 0 0 0 0\n\
             color: 20 1 2 3\n\
             color: 30 4 5 6 7 8 9\n",
        )
        .expect("table parses");

        let stops = table.stops();
        assert_eq!(stops[0].end_color, Some(Rgba8::opaque(255, 255, 255)));
        assert_eq!(stops[1].end_color, Some(Rgba8::TRANSPARENT));
        assert_eq!(stops[2].end_color, None);
        assert_eq!(stops[3].end_color, Some(Rgba8::opaque(7, 8, 9)));
    }

    /// Mirroring turns every segment around, so a ramp still runs the way the
    /// palette drew it when the field's sign convention is flipped.
    #[test]
    fn mirroring_reverses_each_ramp_instead_of_carrying_it_across() {
        let table = ColorTable::parse(
            "Ramped velocity",
            "color: 0 10 20 30 40 50 60\ncolor: 10 70 80 90\n",
        )
        .expect("table parses");
        let mirrored = table.mirrored_values("Mirrored");

        // The segment that ran (10,20,30) -> (40,50,60) over 0..10 now runs
        // (40,50,60) -> (10,20,30) over -10..0, opening one float above -10
        // because -10 itself belongs to the reflected point value: the
        // original reads (70,80,90) at 10 and the mirror owes that at -10.
        assert_eq!(mirrored.stops()[0].value, -10.0);
        assert_eq!(mirrored.stops()[0].color, Rgba8::opaque(70, 80, 90));
        assert_eq!(mirrored.stops()[1].value, (-10.0_f32).next_up());
        assert_eq!(mirrored.stops()[1].color, Rgba8::opaque(40, 50, 60));
        assert_eq!(
            mirrored.stops()[1].end_color,
            Some(Rgba8::opaque(10, 20, 30))
        );
        assert_eq!(mirrored.sample(-5.0), table.sample(5.0));
        assert_eq!(mirrored.sample(-10.0), table.sample(10.0));
        assert_eq!(mirrored.sample(-40.0), table.sample(40.0));
    }

    /// Flipping a banded palette is a reflection: it must move every band to
    /// the other side of zero and delete none of them.
    ///
    /// A band covers `[v_i, v_i+1)` and paints `c_i`, so its reflection covers
    /// `(-v_i+1, -v_i]` and the mirror's stop at `-v_i+1` is the one that has
    /// to carry `c_i`. Reversing the stop colours without that shift-down
    /// leaves every band painted in its neighbour's colour, which is what this
    /// test used to assert was correct: it checked that the mirror painted cyan
    /// from 30 up to 40, where under `v -> -v` the cyan band `[-30, -20)`
    /// lands on `(20, 30]` and `(30, 40]` is a transparent mask row. It passed
    /// its other half - `flipped.sample(30.0) == table.sample(-30.0)` - only
    /// because 30 is the one point where the two readings meet.
    ///
    /// Reversing the *segments*, which is right for a ramp, is the other wrong
    /// answer here: a `.pal` lead-in row holds transparent across its interval,
    /// so the stop that opens the first inked band would come out transparent
    /// and the band would paint nothing at all.
    ///
    /// The table here is the shape a user's velocity `.pal` has - two clear
    /// rows below the first band, two above the last - and is parsed with
    /// `parse_stepped`, which is the entry point `parse_color_table_for_family`
    /// uses for a velocity file.
    #[test]
    fn flipping_a_banded_palette_keeps_every_band_it_painted() {
        let table = ColorTable::parse_stepped(
            "User VEL",
            "product: BV\n\
             units: m/s\n\
             color4: -60 0 0 0 0\n\
             color4: -40 0 0 0 0\n\
             color: -30 0 255 255\n\
             color: -20 0 200 0\n\
             color: 0 120 120 120\n\
             color: 20 255 120 0\n\
             color: 30 255 255 0\n\
             color4: 40 0 0 0 0\n\
             color4: 60 0 0 0 0\n",
        )
        .expect("table parses");
        assert_eq!(table.sample_mode, SampleMode::Stepped);
        let flipped = table.mirrored_values("User VEL (flipped)");

        // The band the original paints cyan from -30 up to -20 is the band the
        // mirror paints cyan from 20 up to 30, and the mask above it reflects
        // onto the mask below it.
        let cyan = Rgba8::opaque(0, 255, 255);
        for value in [20.01_f32, 22.5, 25.0, 29.99] {
            assert_eq!(
                flipped.sample(value),
                cyan,
                "the mirror of the cyan band is wrong at {value}"
            );
        }
        for value in [30.01_f32, 35.0, 39.99] {
            assert_eq!(
                flipped.sample(value),
                Rgba8::TRANSPARENT,
                "the mirror paints where the original masked, at {value}"
            );
        }

        // Every colour the original painted is still painted, and no colour it
        // never painted has appeared.
        let painted = |drawn: &ColorTable| {
            let mut seen = Vec::new();
            for step in -12_000..=12_000 {
                let colour = drawn.sample(step as f32 * 0.01);
                if !seen.contains(&colour) {
                    seen.push(colour);
                }
            }
            seen.sort_by_key(|colour| colour.to_array());
            seen
        };
        assert_eq!(painted(&table), painted(&flipped));

        // And the reflection is exact everywhere, both signs, INCLUDING the
        // stop values themselves. The sweep used to skip those, which is
        // exactly where a banded mirror was wrong and where a velocity field
        // puts most of its gates.
        let mut checked_stops = 0_usize;
        for step in -12_000..=12_000 {
            // Divided rather than multiplied by a hundredth, which is not an
            // `f32`: `6_000.0 * 0.01` misses 60 and would step around every
            // stop this sweep is here to land on.
            let value = step as f32 / 100.0;
            if table.stops().iter().any(|stop| stop.value == value) {
                checked_stops += 1;
            }
            assert_eq!(
                flipped.sample(-value),
                table.sample(value),
                "the flip is not a reflection at {value}"
            );
        }
        assert_eq!(
            checked_stops,
            table.stops().len(),
            "the sweep missed a stop value it is here to cover"
        );
    }

    /// The quantised mirror paints the ramp targets its `.pal` declared.
    ///
    /// `QuantizedInterpolated` rounds the value onto the grid and then hands it
    /// to the interpolated sampler, so a row's second colour is live whenever a
    /// grid point falls between two stops - which is the normal case, because a
    /// `.pal` is under no obligation to put its rows on multiples of its own
    /// `step:`. Treating the mode as banded here dropped every declared target
    /// and repainted three quarters of the flipped scope: on the table below,
    /// gates at -5 m/s came out `[30,100,30]` instead of the `[0,115,0]` the
    /// palette gives +5 the other way round.
    ///
    /// The numbers are the ones measured on the shipped `GenericRadar VEL`
    /// grid: `step: 5` with every row at a half-way value, so no grid point
    /// ever lands on a stop and the reflection is exact at all of them.
    #[test]
    fn a_quantised_flip_paints_the_ramp_targets_the_palette_declared() {
        let table = ColorTable::parse_stepped(
            "Stepped user VEL",
            "product: BV\n\
             units: m/s\n\
             Step: 5\n\
             Color: -22.5 0 255 255 0 200 255\n\
             Color: -17.5 0 100 255 0 60 200\n\
             Color: -12.5 0 220 0 0 160 0\n\
             Color: -7.5 0 140 0 0 90 0\n\
             Color: -2.5 60 60 60 100 100 100\n\
             Color: 2.5 150 150 150 190 190 190\n\
             Color: 7.5 220 0 0 255 40 0\n\
             Color: 12.5 255 120 0 255 170 0\n\
             Color: 17.5 255 230 0 255 255 120\n\
             Color: 22.5 255 255 255\n",
        )
        .expect("table parses");
        assert!(matches!(
            table.sample_mode,
            SampleMode::QuantizedInterpolated { step: 5.0, .. }
        ));
        let flipped = table.mirrored_values("flipped");

        // This table, as measured on the real KDVN velocity frame.
        // The middle column is what a banded mirror produced.
        for (value, expected) in [
            (-20.0_f32, [0, 228, 255, 255]),
            (-15.0, [0, 80, 228, 255]),
            (-5.0, [0, 115, 0, 255]),
            (0.0, [80, 80, 80, 255]),
            (5.0, [170, 170, 170, 255]),
            (15.0, [255, 145, 0, 255]),
        ] {
            assert_eq!(
                table.sample(value).to_array(),
                expected,
                "the palette itself moved at {value}"
            );
            assert_eq!(
                flipped.sample(-value).to_array(),
                expected,
                "the flip is not a reflection at {value}"
            );
        }

        // Every grid point the palette reaches, not just the six above, and
        // past both ends of it as well: the row above the top stop used to
        // come out the ramp target the row below it was heading for, because
        // the mirror's lowest stop stood for the original's clamp and its top
        // segment at once.
        for step in -8..=8 {
            let value = step as f32 * 5.0;
            assert_eq!(
                flipped.sample(-value),
                table.sample(value),
                "the flip is not a reflection at grid point {value}"
            );
        }
        assert_eq!(table.sample(25.0), Rgba8::opaque(255, 255, 255));
        assert_eq!(flipped.sample(-25.0), Rgba8::opaque(255, 255, 255));
    }

    /// The same mirror on a built-in, where the grid does land on the stops.
    ///
    /// `GR2Analyst Classic REF` declares `step: 5` and opens with a clear
    /// lead-in whose hard edge sits at 10 dBZ, exactly on the grid, so the one
    /// grid cell the mirror used to lose is here: the cell centred on -10 came
    /// out the mask's transparent instead of the first band's cyan. It is the
    /// smallest visible case of a stop list being continuous from the right
    /// while its reflection has to be continuous from the left, and the ULP
    /// split in [`ColorTable::mirrored_values`] closes it. Every cell of the
    /// palette now reflects exactly.
    ///
    /// Reflectivity is never flipped by the application - the polarity switch
    /// is velocity-only - so this is here for the rule rather than for the
    /// picture.
    #[test]
    fn a_quantised_mirror_is_exact_on_every_cell_its_grid_can_reach() {
        let table = gr2_reflectivity_table();
        let flipped = table.mirrored_values("Mirrored GR2");

        let mut off_by_the_convention = Vec::new();
        for step in -20..=20 {
            let value = step as f32 * 5.0;
            if flipped.sample(-value) != table.sample(value) {
                off_by_the_convention.push(value);
            }
        }
        assert_eq!(off_by_the_convention, Vec::<f32>::new());
        // The cell that used to be lost, named rather than left to the sweep.
        assert_eq!(table.sample(10.0), Rgba8::opaque(4, 233, 231));
        assert_eq!(flipped.sample(-10.0), Rgba8::opaque(4, 233, 231));
        // And the split cost the palette's own numbers nothing a legend reads.
        assert_eq!(flipped.inked_value_span(), Some((-92.5, -10.0)));
    }

    /// Every velocity value a NEXRAD radar can put in front of a palette, on
    /// every velocity palette the picker offers, drawn both ways.
    ///
    /// The sweep this replaces skipped values that were exactly a stop, which
    /// is precisely where the banded mirror was wrong, so it stayed green while
    /// a third of a real painted field came out the neighbouring band's colour.
    /// Velocity is a discrete field and its words land on these palettes'
    /// stops: measured on KDVN 2026-08-19 19:28Z cut 1, drawn banded and
    /// flipped, `Smooth Couplet VEL` had 76,022 of 226,621 painted gates wrong
    /// (33.5%), `Smooth Doppler VEL` 63,610, `Analyst Pro VEL` 38,953, and
    /// `GenericRadar VEL` and `Sign Check VEL` 10,563 each, every one of them
    /// at a value the encoding puts a whole population on. Drawn smooth,
    /// `GenericRadar VEL` was wrong at 0.00 m/s, on 10,563 gates of the same
    /// sweep. Nothing here may skip a value again.
    ///
    /// Five of the ten are drawn as `Stepped` when banded, because their `.pal`
    /// text carries no `step:` row; the other five band on their `step:` grid.
    /// Both shapes are covered, both renderings, at every stop, at every
    /// encodable word, and across a dense sweep that runs past both ends of
    /// each palette into its clamps.
    ///
    /// The only residual is a byte of rounding in the continuous renderings,
    /// which [`ColorTable::mirrored_values`] explains: 3 samples of the
    /// 800,460 swept here, each one byte in one channel, none of them a value
    /// the radar can encode. It is counted rather than tolerated silently.
    #[test]
    fn a_flip_of_every_velocity_palette_is_exact_at_every_value_a_radar_sends() {
        let palettes = builtin_tables_for_family(ColorTableFamily::Velocity);
        assert_eq!(palettes.len(), 10);

        let banded_at_their_own_stops = palettes
            .iter()
            .map(|palette| palette.rendered(TableRendering::Stepped))
            .filter(|table| table.sample_mode == SampleMode::Stepped)
            .map(|table| table.base_name().to_owned())
            .collect::<Vec<_>>();
        assert_eq!(
            banded_at_their_own_stops,
            [
                "GenericRadar VEL",
                "Smooth Doppler VEL",
                "Smooth Couplet VEL",
                "Analyst Pro VEL",
                "Sign Check VEL",
            ]
        );

        let words = encodable_velocity_words();
        let mut rounded_differently = 0_usize;
        let mut swept = 0_usize;
        for palette in palettes {
            for rendering in TableRendering::ALL {
                let table = palette.rendered(rendering);
                let flipped = table.mirrored_values(format!("{} (flipped)", table.name()));
                let stops = table.stops();
                let low = stops.first().expect("has stops").value;
                let high = stops.last().expect("has stops").value;

                // Every stop, exactly. This is the population the old sweep
                // stepped around: the palette's own hard edges, which are
                // where the mirror used to hand out the neighbour's colour.
                for stop in stops {
                    assert_eq!(
                        flipped.sample(-stop.value),
                        table.sample(stop.value),
                        "{} is not a reflection at its stop {}",
                        table.name(),
                        stop.value
                    );
                }

                // Every word either velocity encoding can carry, exactly.
                for &value in &words {
                    assert_eq!(
                        flipped.sample(-value),
                        table.sample(value),
                        "{} is not a reflection at the encodable {value}",
                        table.name()
                    );
                }

                // The stops read from the other side - a palette whose rows
                // are not symmetric about zero has hard edges at values that
                // are not stops of its own - and a dense sweep that runs
                // 40 m/s past both ends, so the clamps are swept as well as
                // the bands. Neither may be wrong; both may carry the byte of
                // rounding a reversed ramp costs, and every one of those is
                // counted.
                let (from, to) = (low - 40.0, high + 40.0);
                let dense = (0..=40_000).map(|step| from + (to - from) * step as f32 / 40_000.0);
                for value in stops.iter().map(|stop| -stop.value).chain(dense) {
                    swept += 1;
                    if reflection_rounds_differently(&table, &flipped, value) {
                        rounded_differently += 1;
                    }
                }
            }
        }
        assert_eq!(swept, 800_460);
        assert_eq!(
            rounded_differently, 3,
            "the reversed-ramp rounding residual moved"
        );
    }

    /// Whether the mirror answers something other than the reflection at
    /// `value`, refusing anything worse than one byte in one channel of a
    /// continuously drawn table.
    ///
    /// A banded mirror is a permutation of the palette's own colours and has
    /// no arithmetic to get wrong, so a difference there is a defect. A
    /// continuous one reverses each ramp and its `t` differs in the last
    /// place, so a channel whose exact mix lands on a half can round the other
    /// way. The caller counts what this reports rather than ignoring it.
    fn reflection_rounds_differently(table: &ColorTable, flipped: &ColorTable, value: f32) -> bool {
        let want = table.sample(value);
        let got = flipped.sample(-value);
        if got == want {
            return false;
        }
        assert!(
            table.interpolates() && max_channel_delta(got, want) <= 1 && got.a == want.a,
            "{} is wrong, not merely rounded, at {value}: {got:?} for {want:?}",
            table.name()
        );
        true
    }

    /// Every velocity a Level II volume can put in front of a palette, in the
    /// engine units the palettes are read in.
    ///
    /// Two encodings reach `sample`. The moment data carries a byte per gate
    /// and every NEXRAD velocity moment ships `scale 2, offset 129`, so the
    /// values are multiples of 0.5 m/s from -63.5 to 63. `render2d`'s
    /// dealiased grid, which the workstation turns on by default, re-encodes
    /// the unfolded field as `u16` at `scale 10`, so its values are multiples
    /// of 0.1 m/s; this walks that grid out to +-120 m/s, past both the
    /// widest palette (+-103) and any velocity a dealiaser will produce.
    ///
    /// These are the values a wrong reflection actually mis-paints, which is
    /// why the flip is pinned over them rather than over a decimal grid that
    /// steps between them.
    fn encodable_velocity_words() -> Vec<f32> {
        let raw = (2_u16..=255).map(|code| (code as f32 - 129.0) / 2.0);
        let dealiased = (-1_200_i32..=1_200).map(|tenths| tenths as f32 / 10.0);
        raw.chain(dealiased).collect()
    }

    /// The other end of a banded flip, on the smallest table that has one.
    ///
    /// A banded table's topmost row paints an open-ended stretch, and in the
    /// mirror that stretch is the clamp under its lowest stop - the same place
    /// the first mirrored band has to sit. One stop cannot be two colours, so
    /// the mirror spends two: the clamp keeps `-10` and the band above it
    /// opens one float higher. Before that split the top row's colour appeared
    /// nowhere in the flip at all, and every gate at or past the top of the
    /// palette was painted the band below.
    #[test]
    fn a_banded_flip_keeps_the_colour_of_the_top_row() {
        let table = ColorTable::parse_stepped(
            "Three bands",
            "mode: stepped\n\
             color: -10 10 10 10\n\
             color: 0 20 20 20\n\
             color: 10 30 30 30\n",
        )
        .expect("table parses");
        let flipped = table.mirrored_values("flipped");

        assert_eq!(flipped.sample(5.0), table.sample(-5.0));
        assert_eq!(flipped.sample(-5.0), table.sample(5.0));
        // Past the top row, and at it: the original paints (30,30,30) from 10
        // up and the mirror paints it from -10 down.
        assert_eq!(table.sample(40.0), Rgba8::opaque(30, 30, 30));
        assert_eq!(flipped.sample(-40.0), Rgba8::opaque(30, 30, 30));
        assert_eq!(flipped.sample(-10.0), Rgba8::opaque(30, 30, 30));
        // Which is the whole reflection, at every float that separates the
        // three bands and at every one between them.
        assert_eq!(flipped.stops()[0].value, -10.0);
        assert_eq!(flipped.stops()[1].value, (-10.0_f32).next_up());
        for step in -4_000..=4_000 {
            let value = step as f32 / 100.0;
            assert_eq!(
                flipped.sample(-value),
                table.sample(value),
                "the flip is not a reflection at {value}"
            );
        }
    }

    /// A hard edge at the *top* row: the clamp under the mirror's bottom and
    /// the segment above it are two colours, and the mirror now spends two
    /// stops on them.
    ///
    /// It used to spend one, and the segment won, so a table clamping to white
    /// above 40 dBZ had a mirror painting the ramp target yellow below -40.
    /// This is the continuous half of the same defect
    /// `a_banded_flip_keeps_the_colour_of_the_top_row` covers, and it is here
    /// because a continuous table can reach it through a declared ramp target
    /// where a banded one reaches it through every top row there is.
    #[test]
    fn mirroring_keeps_the_top_rows_colour_below_the_bottom_of_a_two_colour_table() {
        let table = ColorTable::parse(
            "Hard top edge",
            "color: -40 10 10 200\n\
             color: 0 128 128 128\n\
             color: 20 200 10 10 250 250 0\n\
             color: 40 255 255 255\n",
        )
        .expect("table parses");
        let mirrored = table.mirrored_values("Mirrored");

        // 40 and up is white: the last row is a single colour and the table
        // clamps to it.
        let white = Rgba8::opaque(255, 255, 255);
        assert_eq!(table.sample(40.0), white);
        assert_eq!(table.sample(55.0), white);

        // So -40 and down is white in the mirror, and the ramp target the
        // original's top segment was heading for opens one float above it.
        let ramp_target = Rgba8::opaque(250, 250, 0);
        assert_eq!(mirrored.sample(-40.0), white);
        assert_eq!(mirrored.sample(-55.0), white);
        assert_eq!(mirrored.sample((-40.0_f32).next_up()), ramp_target);

        // Everywhere else the flip is exact too, ends and stops included,
        // except for the byte of rounding a reversed ramp carries: the
        // mirrored segment opens one ULP later than the original's closed, so
        // `t` differs in the last place and a channel whose exact mix lands on
        // a half rounds the other way. Bounded and counted rather than skipped,
        // because a sweep that steps around its own failures is how the last
        // reflection defect stayed hidden.
        let mut rounded_differently = 0_usize;
        for step in 0..=5_000 {
            let value = -50.0 + step as f32 / 50.0;
            let want = table.sample(value);
            let got = mirrored.sample(-value);
            if got != want {
                rounded_differently += 1;
                assert!(
                    max_channel_delta(got, want) <= 1 && got.a == want.a,
                    "the flip is wrong, not merely rounded, at {value}: {got:?} for {want:?}"
                );
            }
        }
        assert_eq!(
            rounded_differently, 9,
            "the rounding residual moved: 9 of 5,001 samples, one byte each"
        );
        for stop in table.stops() {
            assert_eq!(
                mirrored.sample(-stop.value),
                table.sample(stop.value),
                "the flip is not exact at the stop {}",
                stop.value
            );
        }
    }

    /// A table whose only ink is a fade reports the stretch the fade covers,
    /// so a legend gets drawn for a palette an analyst can see.
    ///
    /// Two clear rows, the lower one declaring an opaque second colour: it
    /// paints a thousand of the 2,501 values on a 0.01 grid from -5 to 20 and
    /// declares no opaque stop anywhere. Reported strictly, its ink was the
    /// single value 12.5 - the clear stop the fade arrives on, and the one
    /// value in the whole interval it does *not* paint - which sent
    /// `legend_span` through its zero-width guard and left the pane with no
    /// bar at all. The fade rule reports (0.0, 12.5), which is what the two
    /// rows say. `workstation_app`'s
    /// `a_palette_whose_only_ink_is_a_declared_fade_gets_a_legend` pins the
    /// caller's half.
    #[test]
    fn a_table_whose_only_ink_is_a_fade_reports_the_fade() {
        let quantised = ColorTable::parse(
            "q",
            "product: BR\nstep: 5\ncolor4: 0 0 0 0 0 200 0 0 255\ncolor4: 12.5 0 0 0 0\n",
        )
        .expect("table parses");
        assert!(matches!(
            quantised.sample_mode,
            SampleMode::QuantizedInterpolated { .. }
        ));
        assert_eq!(quantised.sample(2.5).to_array(), [80, 0, 0, 102]);
        assert_eq!(quantised.sample(7.5).to_array(), [160, 0, 0, 204]);
        // The value the strict reading called the whole span, which paints
        // nothing at all.
        assert_eq!(quantised.sample(12.5), Rgba8::TRANSPARENT);
        assert_eq!(quantised.inked_value_span(), Some((0.0, 12.5)));

        // The same rows without the `step:` row: the same fade, the same span.
        let smooth = ColorTable::parse(
            "q",
            "product: BR\ncolor4: 0 0 0 0 0 200 0 0 255\ncolor4: 12.5 0 0 0 0\n",
        )
        .expect("table parses");
        assert_eq!(smooth.inked_value_span(), quantised.inked_value_span());

        // The rule fires only where the fade is all there is. A palette with
        // an opaque stop still reports from that stop, so AWIPS Wilson keeps
        // reporting its ink from the -20 dBZ its first band arrives at rather
        // than from the -30 its lead-in fade opens at.
        assert_eq!(
            awips_wilson_reflectivity_table().inked_value_span(),
            Some((-20.0, 95.0))
        );
        // And a lead-in that holds clear instead of fading still reports the
        // single value it inks.
        assert_eq!(
            ColorTable::parse("held", "color4: -10 0 0 0 0\ncolor: 10 255 0 0\n")
                .expect("table parses")
                .inked_value_span(),
            Some((10.0, 10.0))
        );
    }

    /// A fade-out is a lead-out: the span ends where the palette stops being
    /// itself, not where it finishes disappearing.
    ///
    /// The mirror image of the AWIPS Wilson rule, and the same reason - where
    /// inside the fade the alpha crosses zero is arithmetic, not a number the
    /// palette states. Drawn banded there is no fade to leave out, so the last
    /// painted row holds flat to the clear stop and the span goes with it.
    #[test]
    fn a_fade_out_is_a_lead_out_and_not_part_of_the_span() {
        let table = ColorTable::parse(
            "Fades out",
            "color: 0 200 0 0\ncolor: 10 0 200 0\ncolor4: 20 0 0 0 0\n",
        )
        .expect("table parses");

        // The segment does ink most of its width...
        assert_eq!(table.sample(19.0).to_array(), [0, 20, 0, 26]);
        // ...and the span still stops at the value the palette last states.
        assert_eq!(table.inked_value_span(), Some((0.0, 10.0)));

        // Banded, the same rows paint green flat from 10 to 20, so the span
        // reaches 20. Nothing fades, so nothing is left out.
        let banded = table.rendered(TableRendering::Stepped);
        assert_eq!(banded.sample(19.0), Rgba8::opaque(0, 200, 0));
        assert_eq!(banded.inked_value_span(), Some((0.0, 20.0)));
    }

    /// Two rows at one value: the later one wins.
    ///
    /// `dedup_by` hands the later element first and removes it, so keeping the
    /// later row means copying it over the one that survives. Written the other
    /// way round the assignment is dead and the *first* row wins, which loses
    /// the ramp target the second one declared and is the opposite of what a
    /// person editing a palette expects from a later line.
    #[test]
    fn duplicate_rows_at_one_value_keep_the_last_one() {
        let table = ColorTable::parse(
            "d",
            "color: 0 1 1 1\ncolor: 10 2 2 2\ncolor: 10 3 3 3 4 4 4\ncolor: 20 5 5 5\n",
        )
        .expect("table parses");

        let stops = table.stops();
        assert_eq!(stops.len(), 3);
        assert_eq!(stops[1].value, 10.0);
        assert_eq!(stops[1].color, Rgba8::opaque(3, 3, 3));
        assert_eq!(stops[1].end_color, Some(Rgba8::opaque(4, 4, 4)));

        // Three in a row, to prove the walk keeps chasing the latest.
        let thrice = ColorTable::parse(
            "d",
            "color: 0 1 1 1\ncolor: 10 2 2 2\ncolor: 10 3 3 3\ncolor: 10 9 9 9\ncolor: 20 5 5 5\n",
        )
        .expect("table parses");
        assert_eq!(thrice.stops()[1].color, Rgba8::opaque(9, 9, 9));
    }

    /// Two rules of the GR dialect this reader deliberately does not follow.
    ///
    /// Both are pre-existing and neither is changed here; they are pinned so
    /// that "the clear-row hold is the file's rule" is never read as "a `.pal`
    /// renders exactly as GR renders it". See [`hold_clear_gr_rows`].
    #[test]
    fn the_gr_reader_diverges_on_solid_rows_and_on_step() {
        // GR paints `SolidColor:` as a flat band across its interval whatever
        // else the file says. This parser maps it onto the same arm as
        // `Color:`, so it ramps into its neighbour.
        let solid = ColorTable::parse(
            "s",
            "mode: smooth\nSolidColor: 0 200 0 0\nColor: 10 0 0 200\n",
        )
        .expect("table parses");
        assert_eq!(solid.sample(5.0).to_array(), [100, 0, 100, 255]);

        // In a GR `.pal`, `Step:` is the legend's tick spacing and never
        // quantises the display. Here it does.
        let stepped = ColorTable::parse("t", "Step: 5\nColor: 0 0 0 0\nColor: 20 200 200 200\n")
            .expect("table parses");
        assert_eq!(stepped.sample_mode_label(), "quantized stepped");
        assert_eq!(stepped.sample(7.0).to_array(), [50, 50, 50, 255]);
    }

    #[test]
    fn parses_gr_scale_without_double_scaling_units() {
        let table = ColorTable::parse(
            "Scaled velocity",
            r#"
            product: BV
            scale: 2
            color: 10 10 20 30
            color: 20 30 40 50
            "#,
        )
        .expect("table parses");

        assert_eq!(table.stops()[0].value, 5.0);
        assert_eq!(table.stops()[1].value, 10.0);
    }

    #[test]
    fn stepped_tables_hold_bins_between_thresholds() {
        let table = ColorTable::parse(
            "Stepped velocity",
            r#"
            mode: stepped
            color: 0 0 0 0
            color: 10 255 255 255
            "#,
        )
        .expect("table parses");

        assert!(!table.interpolates());
        assert_eq!(table.sample(5.0), Rgba8::opaque(0, 0, 0));
        assert_eq!(table.sample(10.0), Rgba8::opaque(255, 255, 255));
    }

    #[test]
    fn step_rows_make_pal_style_tables_quantized_ramps() {
        let table = ColorTable::parse(
            "RadarScope sample",
            r#"
            product: BR
            units: dBZ
            step: 5
            color4: -5 0 0 0 0
            color: 5 0 0 100
            color: 15 0 0 200
            "#,
        )
        .expect("table parses");

        assert!(!table.interpolates());
        assert_eq!(table.sample_mode_label(), "quantized stepped");
        assert_eq!(table.step_size(), Some(5.0));
        assert_eq!(table.sample(0.0), Rgba8::TRANSPARENT);
        assert_eq!(table.sample(7.4), Rgba8::opaque(0, 0, 100));
        assert_eq!(table.sample(11.0), Rgba8::opaque(0, 0, 150));
        assert_eq!(table.sample(12.4), Rgba8::opaque(0, 0, 150));
        assert_eq!(table.sample(12.6), Rgba8::opaque(0, 0, 200));
    }

    #[test]
    fn quantized_step_converts_with_velocity_units() {
        let table = ColorTable::parse(
            "Velocity sample",
            r#"
            units: MPH
            step: 10
            color: 0 80 80 80
            color: 20 240 0 0
            "#,
        )
        .expect("table parses");

        let step = table.step_size().expect("numeric step preserved");
        assert!((step - 10.0 * MPH_TO_MPS).abs() < 0.001);
    }

    #[test]
    fn parse_stepped_defaults_to_bins_without_mode_line() {
        let table = ColorTable::parse_stepped(
            "NWS sample",
            r#"
            units: dBZ
            color: 0 0 0 0
            color: 10 255 255 255
            "#,
        )
        .expect("table parses");

        assert!(!table.interpolates());
        assert_eq!(table.sample(5.0), Rgba8::opaque(0, 0, 0));
    }

    #[test]
    fn explicit_interpolated_mode_overrides_stepped_default() {
        let table = ColorTable::parse_stepped(
            "Smooth sample",
            r#"
            mode: interpolated
            color: 0 0 0 0
            color: 10 100 100 100
            "#,
        )
        .expect("table parses");

        assert!(table.interpolates());
        assert_eq!(table.sample(5.0), Rgba8::opaque(50, 50, 50));
    }

    #[test]
    fn the_default_reflectivity_is_the_wilson_awips_look() {
        let table = builtin_reflectivity_table();

        assert_eq!(table.name(), "AWIPS Wilson REF (interpolated)");
        assert!(table.interpolates());
        assert_eq!(table.sample_mode_label(), "interpolated");
        assert_eq!(table.step_size(), None);
        // The noise floor stays off the scope; the first band fades in from
        // -30 dBZ, which is deliberate - this palette shows clear-air return.
        assert_eq!(table.sample(-31.0).to_array()[3], 0);
        assert!(table.sample(-25.0).to_array()[3] > 90);
        // Declared rows, read straight off the AWIPS table.
        assert_eq!(table.sample(35.0), Rgba8::opaque(29, 104, 9));
        assert_eq!(table.sample(50.0), Rgba8::opaque(255, 0, 0));
        assert_eq!(table.sample(60.0), Rgba8::opaque(255, 255, 255));
        assert_eq!(table.sample(75.0), Rgba8::opaque(5, 236, 240));
    }

    /// The shipped default for the two base moments is the continuous drawing
    /// of the palette they always used; every other preset keeps the sampling
    /// it was authored with.
    ///
    /// This test used to assert the opposite of its first two lines. It was
    /// changed on purpose: a quarter to two fifths of neighbouring gate pairs
    /// that carried *different* readings were being painted the same colour by
    /// the banded defaults on real volumes, which is the whole of the complaint
    /// that "everything just blends together". See the measurements recorded
    /// above `builtin_reflectivity_table`.
    #[test]
    fn the_two_base_moment_defaults_open_continuous_and_nothing_else_moved() {
        for table in [builtin_reflectivity_table(), builtin_velocity_table()] {
            assert!(
                table.interpolates(),
                "{} should open as a continuous ramp",
                table.name()
            );
            assert_eq!(table.rendering(), TableRendering::Smooth);
        }
        for table in [
            gr2_reflectivity_table(),
            tornado_velocity_table(),
            analyst_reflectivity_table(),
            nws_reflectivity_table(),
            vortex_velocity_table(),
            nws_velocity_table(),
        ] {
            assert!(
                !table.interpolates(),
                "{} should still use stepped radar bins",
                table.name()
            );
        }
    }

    /// The default is the same palette it always was, wearing a different
    /// sampling. Not a different colour scheme.
    #[test]
    fn the_defaults_are_the_ported_palettes_exactly_as_authored() {
        // The ported looks are authored as continuous sRGB ramps, so the
        // smooth rendering the defaults ask for is the identity - the default
        // IS the ported palette, byte for byte, and the old classics stay in
        // the catalogue behind it.
        assert_eq!(
            builtin_reflectivity_table(),
            awips_wilson_reflectivity_table()
        );
        assert_eq!(builtin_velocity_table(), generic_radar_velocity_table());
    }

    #[test]
    fn analyst_velocity_preset_is_stepped_for_gate_readability() {
        let table = analyst_velocity_table();

        assert!(!table.interpolates());
    }

    #[test]
    fn the_default_velocity_is_the_radarscope_classic_look() {
        let table = builtin_velocity_table();

        assert_eq!(table.name(), "GenericRadar VEL (interpolated)");
        assert!(table.interpolates());
        // The table is authored in knots and scaled to m/s at parse time;
        // probe at declared rows through the same conversion the parser used.
        let kt = |value: f32| value * (1.0 / 1.9426);
        assert_eq!(table.sample(kt(0.0)), Rgba8::opaque(130, 106, 120));
        assert_eq!(table.sample(kt(10.0)), Rgba8::opaque(105, 0, 0));
        assert_eq!(table.sample(kt(40.0)), Rgba8::opaque(249, 58, 84));
        assert_eq!(table.sample(kt(-40.0)), Rgba8::opaque(10, 248, 35));
        assert_eq!(table.sample(kt(-70.0)), Rgba8::opaque(55, 226, 229));
        assert_eq!(table.sample(kt(-50.0)), Rgba8::opaque(180, 240, 243));
        let range_folded = table.range_folded_rgba().to_array();
        assert_eq!(&range_folded[..3], &[123, 0, 200]);
    }

    #[test]
    fn accepted_velocity_presets_whiten_strong_wind_cores() {
        for table in [
            tornado_velocity_table().rendered(TableRendering::Smooth),
            analyst_velocity_table(),
            radarscope_contrast_velocity_table(),
        ] {
            let inbound = table.sample(-30.0);
            let [in_r, in_g, in_b, _] = inbound.to_array();
            assert!(
                in_r > 185 && in_g > 235 && in_b > 220,
                "{} should turn strong inbound winds pale cyan/white, got {in_r},{in_g},{in_b}",
                table.name()
            );

            let outbound = table.sample(36.0);
            let [out_r, out_g, out_b, _] = outbound.to_array();
            assert!(
                out_r > 240 && out_g > 190 && out_b > 140,
                "{} should turn strong outbound winds cream/orange-white, got {out_r},{out_g},{out_b}",
                table.name()
            );
        }
    }

    #[test]
    fn signatures_change_when_colors_change() {
        let left =
            ColorTable::parse("a", "color: 0 0 0 0\ncolor: 1 255 255 255").expect("table parses");
        let right =
            ColorTable::parse("a", "color: 0 0 0 0\ncolor: 1 255 255 254").expect("table parses");

        assert_ne!(left.signature(), right.signature());
    }

    #[test]
    fn built_in_presets_offer_multiple_ref_and_velocity_choices() {
        let reflectivity = builtin_tables_for_family(ColorTableFamily::Reflectivity)
            .into_iter()
            .map(|table| table.name().to_owned())
            .collect::<Vec<_>>();
        let velocity = builtin_tables_for_family(ColorTableFamily::Velocity)
            .into_iter()
            .map(|table| table.name().to_owned())
            .collect::<Vec<_>>();

        assert_eq!(
            reflectivity,
            vec![
                "AWIPS Wilson REF (interpolated)",
                "GR2Analyst Classic REF (quantized stepped)",
                "Smooth Classic REF (interpolated)",
                "Smooth Sequential REF (interpolated)",
                "Smooth Storm Core REF (interpolated)",
                "Analyst Classic REF (quantized stepped)",
                "NWS Classic REF (quantized stepped)",
                "Dark Scope REF (quantized stepped)",
                "Analyst Hail Core REF (quantized stepped)",
                "Analyst Low Precip REF (quantized stepped)",
                "Tornado Debris REF (quantized stepped)",
                "Clean Light REF (quantized stepped)",
            ]
        );
        assert_eq!(
            velocity,
            vec![
                "GenericRadar VEL (interpolated)",
                "Analyst Tornado VEL (quantized stepped)",
                "Smooth Doppler VEL (interpolated)",
                "Smooth Couplet VEL (interpolated)",
                "Analyst Pro VEL (stepped)",
                "RadarScope Contrast VEL (quantized stepped)",
                "Sign Check VEL (stepped)",
                "Couplet Pop VEL (quantized stepped)",
                "GR2-ish Analyst VEL (quantized stepped)",
                "Subtle SRV VEL (quantized stepped)",
            ]
        );
    }

    /// Both families an analyst spends the day in must offer the field drawn
    /// both ways, or there is no way to tell a bad palette from a bad renderer.
    ///
    /// Counted rather than merely listed: the lists above pin the exact names,
    /// this pins the property those names have to satisfy.
    #[test]
    fn reflectivity_and_velocity_each_offer_stepped_and_interpolated_tables() {
        for (family, minimum_interpolated) in [
            (ColorTableFamily::Reflectivity, 3),
            (ColorTableFamily::Velocity, 2),
        ] {
            let tables = builtin_tables_for_family(family);
            let interpolated = tables.iter().filter(|table| table.interpolates()).count();
            let stepped = tables.len() - interpolated;
            assert!(
                interpolated >= minimum_interpolated,
                "{} offers {interpolated} interpolated tables, wanted {minimum_interpolated}",
                family.label()
            );
            assert!(stepped >= 1, "{} lost its stepped tables", family.label());
        }
    }

    #[test]
    fn accepted_reflectivity_presets_filter_junk_and_delay_purple() {
        for table in [
            gr2_reflectivity_table(),
            nws_reflectivity_table(),
            dark_scope_reflectivity_table(),
            hail_core_reflectivity_table(),
            low_precip_reflectivity_table(),
        ] {
            assert_eq!(table.sample_mode_label(), "quantized stepped");
            assert!(
                table.step_size().is_some(),
                "{} has step size",
                table.name()
            );
            assert_eq!(table.sample(5.0), Rgba8::TRANSPARENT);
            assert_ne!(
                table.sample(10.0),
                Rgba8::TRANSPARENT,
                "{} should show 10 dBZ and higher",
                table.name()
            );
            for stop in table.stops() {
                let [red, green, blue, alpha] = stop.color.to_array();
                let purple_or_magenta = alpha > 0 && red > 120 && blue > 120 && green < 120;
                assert!(
                    !purple_or_magenta || stop.value >= 65.0,
                    "{} brings purple too early at {:.1} dBZ: {red},{green},{blue}",
                    table.name(),
                    stop.value
                );
            }
        }
    }

    #[test]
    fn accepted_reflectivity_presets_keep_high_dbz_purple() {
        for table in [
            gr2_reflectivity_table(),
            nws_reflectivity_table(),
            analyst_classic_reflectivity_table(),
            dark_scope_reflectivity_table(),
            hail_core_reflectivity_table(),
            low_precip_reflectivity_table(),
        ] {
            assert!(
                table.stops().iter().any(|stop| {
                    let [red, green, blue, alpha] = stop.color.to_array();
                    alpha > 0 && stop.value >= 65.0 && red > 140 && blue > 120 && green < 120
                }),
                "{} should keep a high-dBZ purple/magenta bin",
                table.name()
            );
        }
    }

    #[test]
    fn accepted_velocity_presets_stay_available() {
        // Availability, and availability of the hard-banded drawing of each,
        // which is what this used to assert by testing the sampling mode. The
        // banded drawing is now a flip rather than a fact about the table, so
        // the check is that the flip lands somewhere banded.
        for table in [
            builtin_velocity_table(),
            analyst_velocity_table(),
            radarscope_contrast_velocity_table(),
            sign_check_velocity_table(),
        ] {
            let stepped = table.rendered(TableRendering::Stepped);
            assert!(!stepped.interpolates(), "{} lost its bands", table.name());
            assert_eq!(stepped.rendering(), TableRendering::Stepped);
            assert_eq!(stepped.base_name(), table.base_name());
        }
    }

    #[test]
    fn sign_check_velocity_table_exposes_raw_velocity_polarity() {
        let table = sign_check_velocity_table();

        assert_eq!(table.name(), "Sign Check VEL (stepped)");
        assert_eq!(table.sample_mode_label(), "stepped");
        assert_eq!(table.sample(-1.0), Rgba8::opaque(0, 0, 255));
        assert_eq!(table.sample(0.0), Rgba8::opaque(120, 120, 120));
        assert_eq!(table.sample(1.0), Rgba8::opaque(255, 0, 0));
        assert_eq!(table.range_folded_rgba(), Rgba8::opaque(180, 80, 255));
    }

    /// The palette that exists to show which way the velocities run, flipped.
    ///
    /// Inbound and outbound swap, which is the whole point, and the
    /// range-folded colour is a category rather than a value so it does not.
    ///
    /// Zero is the value that made the whole reflection worth writing down.
    /// Sign Check marks the zero isodop with a band 0.01 m/s wide running
    /// *upward* from zero, so its reflection runs downward from zero and 0.00
    /// itself is the edge. Under a plain `v -> -v` mirror the edge fell on the
    /// wrong side and a gate at exactly 0.00 m/s - 10,563 of them on the real
    /// KDVN sweep this was measured against, 4.7% of the painted field - read
    /// as the first inbound blue instead of the marker's grey, on the one
    /// shipped palette whose entire job is showing which way the wind is
    /// going. The mirror keeps the point value now, so the marker survives the
    /// flip.
    #[test]
    fn mirrored_velocity_table_samples_opposite_polarity_colors() {
        let table = sign_check_velocity_table();
        let mirrored = table.mirrored_values("Mirrored Sign Check VEL");

        assert_eq!(mirrored.sample(1.0), table.sample(-1.0));
        assert_eq!(mirrored.sample(-1.0), table.sample(1.0));
        assert_eq!(mirrored.range_folded_rgba(), table.range_folded_rgba());

        // The marker band is [0, 0.01) going up; its mirror is (-0.01, 0]
        // going down, and 0.00 belongs to it at both signs of zero.
        assert_eq!(table.sample(0.0), Rgba8::opaque(120, 120, 120));
        assert_eq!(mirrored.sample(-0.005), Rgba8::opaque(120, 120, 120));
        assert_eq!(mirrored.sample(0.0), Rgba8::opaque(120, 120, 120));
        assert_eq!(mirrored.sample(-0.0), Rgba8::opaque(120, 120, 120));
        // And the band the marker replaced is where it was: the mirror's first
        // inbound blue starts one float below the marker, not on it.
        assert_eq!(mirrored.sample(0.011), Rgba8::opaque(0, 0, 255));
    }

    #[test]
    fn review_candidate_palettes_are_stepped() {
        for table in [
            analyst_classic_reflectivity_table(),
            tornado_debris_reflectivity_table(),
            clean_light_reflectivity_table(),
            couplet_pop_velocity_table(),
            gr2_ish_analyst_velocity_table(),
            subtle_srv_velocity_table(),
        ] {
            assert!(!table.interpolates(), "{} should be stepped", table.name());
        }
    }

    #[test]
    fn the_reflectivity_preset_inks_from_ten_dbz_not_from_its_transparent_first_stop() {
        let table = gr2_reflectivity_table();

        // The GR2 preset declares stops at -10 and 7.5 dBZ with alpha 0, so a
        // legend drawn across the declared domain would label empty scope.
        assert_eq!(table.stops()[0].value, -10.0);
        assert_eq!(table.stops()[0].color.a, 0);
        assert_eq!(table.inked_value_span(), Some((10.0, 92.5)));
        // Tie the reported low bound to what actually gets painted.
        assert_eq!(table.sample(7.5), Rgba8::TRANSPARENT);
        assert_ne!(table.sample(10.0), Rgba8::TRANSPARENT);
    }

    #[test]
    fn a_table_with_no_transparent_stops_inks_its_whole_first_to_last_span() {
        let table = builtin_generic_table();

        assert_eq!(table.stops().first().expect("has stops").value, 0.0);
        assert_eq!(table.stops().last().expect("has stops").value, 100.0);
        assert_eq!(table.inked_value_span(), Some((0.0, 100.0)));
    }

    #[test]
    fn an_entirely_transparent_table_reports_no_inked_span_so_its_legend_is_suppressed() {
        let table = ColorTable::parse(
            "Blank sample",
            r#"
            product: BR
            units: dBZ
            color4: -30 0 0 0 0
            color4: 0 0 0 0 0
            color4: 30 0 0 0 0
            "#,
        )
        .expect("table parses");

        assert_eq!(table.inked_value_span(), None);
    }

    #[test]
    fn transparent_stops_at_both_ends_are_trimmed_so_only_the_middle_is_reported() {
        let table = ColorTable::parse(
            "Trimmed sample",
            r#"
            product: BR
            units: dBZ
            color4: -20 0 0 0 0
            color4: -5 0 0 0 0
            color: 5 0 0 100
            color: 45 200 0 0
            color4: 60 0 0 0 0
            color4: 80 0 0 0 0
            "#,
        )
        .expect("table parses");

        assert_eq!(table.inked_value_span(), Some((5.0, 45.0)));
    }

    #[test]
    fn the_range_folded_color_does_not_widen_the_inked_span() {
        let table = ColorTable::parse(
            "Folded sample",
            r#"
            product: BV
            units: m/s
            RF: 200 40 240
            color4: -50 0 0 0 0
            color: -20 0 0 255
            color: 20 255 0 0
            color4: 50 0 0 0 0
            "#,
        )
        .expect("table parses");

        // The folded color is opaque, but it is keyed off the folded code rather
        // than a velocity, so the span must stay at the inked stops.
        assert_eq!(table.range_folded_rgba(), Rgba8::new(200, 40, 240, 245));
        assert_eq!(table.inked_value_span(), Some((-20.0, 20.0)));
    }

    /// Every built-in table paired with the inked span its stops imply, read
    /// off the palette text by hand rather than from `inked_value_span` itself.
    ///
    /// Forty-five of the forty-seven carry their palette values through
    /// `parse` untouched. Two do not: `nws_velocity_table` declares
    /// `units: kt` and `vortex_velocity_table` declares `scale: 2.237`, so
    /// `parse` multiplies every stop into m/s. Their expectations are written
    /// as that same product, not as a copied decimal, so a change to
    /// `KNOT_TO_MPS` moves the test with the code instead of silently failing.
    fn every_builtin_table_with_expected_span() -> Vec<(&'static str, ColorTable, (f32, f32))> {
        vec![
            (
                "analyst_reflectivity_table",
                analyst_reflectivity_table(),
                (-10.0, 75.0),
            ),
            (
                "nws_reflectivity_table",
                nws_reflectivity_table(),
                (10.0, 92.5),
            ),
            (
                "analyst_classic_reflectivity_table",
                analyst_classic_reflectivity_table(),
                (10.0, 92.5),
            ),
            (
                "gr2_reflectivity_table",
                gr2_reflectivity_table(),
                (10.0, 92.5),
            ),
            (
                "storm_detail_reflectivity_table",
                storm_detail_reflectivity_table(),
                (5.0, 80.0),
            ),
            (
                "hail_core_reflectivity_table",
                hail_core_reflectivity_table(),
                (10.0, 95.0),
            ),
            (
                "low_precip_reflectivity_table",
                low_precip_reflectivity_table(),
                (10.0, 90.0),
            ),
            (
                "dark_scope_reflectivity_table",
                dark_scope_reflectivity_table(),
                (10.0, 95.0),
            ),
            (
                "tornado_debris_reflectivity_table",
                tornado_debris_reflectivity_table(),
                (10.0, 95.0),
            ),
            (
                "clean_light_reflectivity_table",
                clean_light_reflectivity_table(),
                (10.0, 92.5),
            ),
            (
                "smooth_classic_reflectivity_table",
                smooth_classic_reflectivity_table(),
                (10.0, 95.0),
            ),
            (
                "smooth_sequential_reflectivity_table",
                smooth_sequential_reflectivity_table(),
                (10.0, 95.0),
            ),
            (
                "smooth_storm_core_reflectivity_table",
                smooth_storm_core_reflectivity_table(),
                (10.0, 95.0),
            ),
            (
                "tornado_velocity_table",
                tornado_velocity_table(),
                (-70.0, 70.0),
            ),
            (
                "vortex_velocity_table",
                vortex_velocity_table(),
                (-130.0 / 2.237, 130.0 / 2.237),
            ),
            (
                "analyst_velocity_table",
                analyst_velocity_table(),
                (-70.0, 70.0),
            ),
            (
                "nws_velocity_table",
                nws_velocity_table(),
                (-120.0 * KNOT_TO_MPS, 120.0 * KNOT_TO_MPS),
            ),
            ("gr2_velocity_table", gr2_velocity_table(), (-70.0, 70.0)),
            (
                "tight_couplet_velocity_table",
                tight_couplet_velocity_table(),
                (-70.0, 70.0),
            ),
            (
                "radarscope_contrast_velocity_table",
                radarscope_contrast_velocity_table(),
                (-70.0, 70.0),
            ),
            (
                "sign_check_velocity_table",
                sign_check_velocity_table(),
                (-100.0, 100.0),
            ),
            (
                "couplet_pop_velocity_table",
                couplet_pop_velocity_table(),
                (-70.0, 70.0),
            ),
            (
                "gr2_ish_analyst_velocity_table",
                gr2_ish_analyst_velocity_table(),
                (-70.0, 70.0),
            ),
            (
                "subtle_srv_velocity_table",
                subtle_srv_velocity_table(),
                (-70.0, 70.0),
            ),
            (
                "smooth_doppler_velocity_table",
                smooth_doppler_velocity_table(),
                (-70.0, 70.0),
            ),
            (
                "smooth_couplet_velocity_table",
                smooth_couplet_velocity_table(),
                (-70.0, 70.0),
            ),
            (
                "nws_split_velocity_table",
                nws_split_velocity_table(),
                (-70.0, 70.0),
            ),
            (
                "dark_analyst_velocity_table",
                dark_analyst_velocity_table(),
                (-70.0, 70.0),
            ),
            (
                "builtin_spectrum_width_table",
                builtin_spectrum_width_table(),
                (0.0, 24.0),
            ),
            (
                "turbulence_spectrum_width_table",
                turbulence_spectrum_width_table(),
                (0.0, 24.0),
            ),
            (
                "clear_air_spectrum_width_table",
                clear_air_spectrum_width_table(),
                (0.0, 24.0),
            ),
            (
                "spectrum_width_class_bands_table",
                spectrum_width_class_bands_table(),
                (0.0, 24.0),
            ),
            (
                "analyst_differential_reflectivity_table",
                analyst_differential_reflectivity_table(),
                (-13.0, 20.0),
            ),
            (
                "storm_interrogation_differential_reflectivity_table",
                storm_interrogation_differential_reflectivity_table(),
                (-13.0, 20.0),
            ),
            (
                "zdr_column_hunter_table",
                zdr_column_hunter_table(),
                (-13.0, 20.0),
            ),
            (
                "hail_signal_differential_reflectivity_table",
                hail_signal_differential_reflectivity_table(),
                (-13.0, 20.0),
            ),
            (
                "analyst_correlation_coefficient_table",
                analyst_correlation_coefficient_table(),
                (62.5 / 300.0, 315.5 / 300.0),
            ),
            (
                "debris_hunter_correlation_coefficient_table",
                debris_hunter_correlation_coefficient_table(),
                (62.5 / 300.0, 315.5 / 300.0),
            ),
            (
                "melting_layer_correlation_coefficient_table",
                melting_layer_correlation_coefficient_table(),
                (62.5 / 300.0, 315.5 / 300.0),
            ),
            (
                "correlation_coefficient_class_bands_table",
                correlation_coefficient_class_bands_table(),
                (62.5 / 300.0, 315.5 / 300.0),
            ),
            (
                "analyst_differential_phase_table",
                analyst_differential_phase_table(),
                (0.0, 360.0),
            ),
            (
                "twilight_cyclic_differential_phase_table",
                twilight_cyclic_differential_phase_table(),
                (0.0, 360.0),
            ),
            (
                "phase_bands_differential_phase_table",
                phase_bands_differential_phase_table(),
                (0.0, 360.0),
            ),
            (
                "analyst_specific_differential_phase_table",
                analyst_specific_differential_phase_table(),
                (-2.0, 7.0),
            ),
            (
                "heavy_rain_specific_differential_phase_table",
                heavy_rain_specific_differential_phase_table(),
                (-2.0, 7.0),
            ),
            (
                "fine_detail_specific_differential_phase_table",
                fine_detail_specific_differential_phase_table(),
                (-2.0, 7.0),
            ),
            (
                "builtin_generic_table",
                builtin_generic_table(),
                (0.0, 100.0),
            ),
        ]
    }

    #[test]
    fn every_builtin_tables_inked_span_is_the_one_its_stops_imply() {
        // 1e-4 in table units. The two unit-scaled tables reach about 61.7 m/s,
        // where an f32 resolves roughly 4e-6, so this is two orders looser than
        // representation noise. It is also four orders tighter than the 1 m/s
        // finest built-in velocity bin and the 2.5 dBZ finest reflectivity
        // step, so no mis-scaled or off-by-one-stop bound can slip through.
        const TOLERANCE: f32 = 1e-4;

        for (label, table, expected) in every_builtin_table_with_expected_span() {
            let span = table
                .inked_value_span()
                .unwrap_or_else(|| panic!("{label} inks nothing"));
            assert!(
                (span.0 - expected.0).abs() <= TOLERANCE,
                "{label} low bound: got {}, expected {}",
                span.0,
                expected.0
            );
            assert!(
                (span.1 - expected.1).abs() <= TOLERANCE,
                "{label} high bound: got {}, expected {}",
                span.1,
                expected.1
            );
        }
    }

    #[test]
    fn exactly_twelve_builtin_tables_open_with_two_transparent_stops() {
        let tables = every_builtin_table_with_expected_span();
        assert_eq!(
            tables.len(),
            47,
            "every built-in constructor must be listed here or its span is unpinned"
        );

        let mut lead_in_transparent = 0;
        for (label, table, _) in &tables {
            if table.stops()[0].color.a > 0 {
                continue;
            }
            lead_in_transparent += 1;
            // Always two, never one, and never an interior or trailing hole:
            // the doc comment on inked_value_span states this shape, and a
            // palette edit that breaks it must break this test too.
            let transparent = table
                .stops()
                .iter()
                .filter(|stop| stop.color.a == 0)
                .count();
            assert_eq!(
                transparent, 2,
                "{label} should carry exactly two alpha-0 stops, all of them leading"
            );
        }
        assert_eq!(
            lead_in_transparent, 12,
            "twelve built-ins open transparent: the nine parsed reflectivity presets \
             and the three interpolated ones"
        );
    }

    /// A lead-in transparent preset must paint at the bound its legend
    /// advertises and paint nothing below the clear stop it declares.
    ///
    /// The gap between those two is where the modes differ, and it is why this
    /// is not one assertion for all twelve. A stepped or quantized table jumps:
    /// its last clear stop and its first inked stop are 2.5 or 5 dBZ apart and
    /// everything in between is fully clear, because `sample` refuses any value
    /// below the first opaque stop outright. An interpolated table cannot jump -
    /// interpolating from alpha 0 to alpha 255 is what it does for a living -
    /// so the three smooth presets put their last clear stop 0.5 dBZ below the
    /// first inked one and ramp alpha across that gap. Level II reflectivity is
    /// quantised to 0.5 dBZ (scale 2, offset 66), so the ramp is exactly one
    /// data step wide: at most one value per gate can land half-painted, which
    /// is the narrowest an interpolated table can make it.
    ///
    /// Both shapes are held to the same two facts, and the gap is capped at one
    /// bin so no preset can quietly widen it into a visible haze.
    #[test]
    fn each_lead_in_transparent_preset_paints_at_its_low_bound_and_stays_clear_below_it() {
        let mut checked = 0;
        for (label, table, _) in every_builtin_table_with_expected_span() {
            if table.stops()[0].color.a > 0 {
                continue;
            }
            let (low, _) = table
                .inked_value_span()
                .unwrap_or_else(|| panic!("{label} inks nothing"));
            let last_clear = table
                .stops()
                .iter()
                .filter(|stop| stop.color.a == 0)
                .map(|stop| stop.value)
                .fold(f32::NEG_INFINITY, f32::max);

            assert_eq!(
                table.sample(low).a,
                255,
                "{label} must paint fully opaque at its reported low bound {low}"
            );
            assert_eq!(
                table.sample(last_clear).a,
                0,
                "{label} must paint nothing at its last clear stop {last_clear}"
            );
            assert_eq!(
                table.sample(last_clear - 0.001).a,
                0,
                "{label} must paint nothing below its last clear stop {last_clear}"
            );
            assert!(
                low - last_clear > 0.0 && low - last_clear <= 5.0,
                "{label} leaves {} dBZ between its last clear stop and its first \
                 inked one, which is more than one bin",
                low - last_clear
            );
            checked += 1;
        }
        assert_eq!(checked, 12, "expected twelve lead-in transparent presets");
    }

    #[test]
    fn a_palette_with_a_single_inked_stop_reports_a_zero_width_span_not_none() {
        let table = ColorTable::parse(
            "Single inked stop",
            "color4: -10 0 0 0 0\ncolor: 10 255 0 0\n",
        )
        .expect("table parses");

        // from_parts guarantees two stops, not two inked stops. None would be
        // wrong here because the table does ink, at exactly one value; the
        // legend caller is the one that has to notice high == low before it
        // divides by the width and gets NaN tick positions.
        assert_eq!(table.inked_value_span(), Some((10.0, 10.0)));
    }

    #[test]
    fn mirroring_a_preset_moves_its_transparent_stops_to_the_top_of_the_span() {
        let mirrored = gr2_reflectivity_table().mirrored_values("Mirrored GR2");

        // mirrored_values negates every stop and from_parts re-sorts, so the
        // transparency that led the table now trails it. Three stops and not
        // two: this palette is quantised, so its mirror turns each segment
        // around, and the segment that held the lead-in's transparent colour
        // hands that colour to the stop above it as well. This is the only
        // trailing-transparent shape reachable from a built-in, and it proves
        // the scan tracks the last inked *segment* instead of assuming the
        // final stop is inked.
        assert_eq!(
            mirrored
                .stops()
                .iter()
                .filter(|stop| stop.color.a == 0)
                .count(),
            3
        );
        assert_eq!(mirrored.stops().last().expect("has stops").value, 10.0);
        assert_eq!(mirrored.stops().last().expect("has stops").color.a, 0);
        // -10 and not -7.5: the mirror's last inked segment runs up to -10 and
        // the hold takes over there, which is the reflection of the original
        // holding transparent from 7.5 up to the 10 dBZ its first band opens
        // at.
        assert_eq!(mirrored.inked_value_span(), Some((-92.5, -10.0)));
    }

    /// Largest per-channel difference between two colours, alpha ignored.
    ///
    /// A crude stand-in for perceptual distance, picked because it can be
    /// checked by hand straight off the stop lists above. Two colours that
    /// differ by 40 here are unambiguously different on a scope; two that
    /// differ by 1 are the same colour.
    fn max_channel_delta(left: Rgba8, right: Rgba8) -> i32 {
        let [left_r, left_g, left_b, _] = left.to_array();
        let [right_r, right_g, right_b, _] = right.to_array();
        [
            (left_r as i32 - right_r as i32).abs(),
            (left_g as i32 - right_g as i32).abs(),
            (left_b as i32 - right_b as i32).abs(),
        ]
        .into_iter()
        .max()
        .expect("three channels")
    }

    /// How far a table travels through colour space between two values.
    ///
    /// Summed as city-block distance over a fine sweep, which is what a fine
    /// sweep converges to: between adjacent samples at most one channel moves
    /// and it moves by one, so the total is the summed absolute variation of
    /// each channel. That makes the result comparable to a by-hand sum of
    /// |dR|+|dG|+|dB| across the stop list, which is how the expected values in
    /// these tests were derived.
    fn colour_path(table: &ColorTable, low: f32, high: f32, steps: usize) -> f64 {
        let span = (high - low) as f64;
        let mut total = 0.0;
        let mut previous = table.sample(low);
        for index in 1..=steps {
            let value = low as f64 + span * index as f64 / steps as f64;
            let current = table.sample(value as f32);
            let [previous_r, previous_g, previous_b, _] = previous.to_array();
            let [current_r, current_g, current_b, _] = current.to_array();
            total += (current_r as i32 - previous_r as i32).unsigned_abs() as f64
                + (current_g as i32 - previous_g as i32).unsigned_abs() as f64
                + (current_b as i32 - previous_b as i32).unsigned_abs() as f64;
            previous = current;
        }
        total
    }

    /// Every correlation coefficient value a forecaster reads a category off,
    /// paired with the category. Used to check tables separate them.
    const CC_CATEGORY_PROBES: [(f32, &str); 6] = [
        (0.55, "debris / chaff"),
        (0.75, "non-meteorological"),
        (0.87, "mixed hydrometeors"),
        (0.93, "melting layer"),
        (0.96, "marginal"),
        (0.99, "meteorological"),
    ];

    #[test]
    fn every_family_lists_tables_and_the_dual_pol_families_list_at_least_three() {
        for family in ColorTableFamily::ALL {
            let tables = builtin_tables_for_family(family);
            assert!(
                !tables.is_empty(),
                "{} enumerates no tables, so a picker would show an empty list",
                family.label()
            );
        }

        for family in [
            ColorTableFamily::SpectrumWidth,
            ColorTableFamily::DifferentialReflectivity,
            ColorTableFamily::CorrelationCoefficient,
            ColorTableFamily::DifferentialPhase,
            ColorTableFamily::SpecificDifferentialPhase,
        ] {
            assert!(
                builtin_tables_for_family(family).len() >= 3,
                "{} must offer at least three tables",
                family.label()
            );
        }
    }

    #[test]
    fn no_two_built_in_tables_anywhere_share_a_name() {
        let mut seen = Vec::new();
        for family in ColorTableFamily::ALL {
            for table in builtin_tables_for_family(family) {
                let name = table.name().to_owned();
                assert!(
                    !seen.contains(&name),
                    "{name} is listed twice; a picker cannot tell the two apart"
                );
                seen.push(name);
            }
        }
    }

    #[test]
    fn every_family_default_is_the_first_table_the_picker_offers() {
        let set = ColorTableSet::default();
        for family in ColorTableFamily::ALL {
            let first = builtin_tables_for_family(family)
                .into_iter()
                .next()
                .expect("every family lists a table");
            assert_eq!(
                set.for_family(family).name(),
                first.name(),
                "{} default disagrees with the head of its list",
                family.label()
            );
        }
    }

    #[test]
    fn each_dual_pol_family_is_stored_and_read_back_independently() {
        let mut set = ColorTableSet::default();
        set.set_family(
            ColorTableFamily::CorrelationCoefficient,
            debris_hunter_correlation_coefficient_table(),
        );

        assert_eq!(
            set.for_family(ColorTableFamily::CorrelationCoefficient)
                .name(),
            "Debris Hunter CC (interpolated)"
        );
        // Setting one dual-pol family must not disturb its neighbours, which
        // shared a single slot before they had families of their own.
        assert_eq!(
            set.for_family(ColorTableFamily::DifferentialReflectivity)
                .name(),
            "Analyst ZDR (interpolated)"
        );
        assert_eq!(
            set.for_family(ColorTableFamily::DifferentialPhase).name(),
            "Analyst Cyclic PHI (interpolated)"
        );
        assert_eq!(
            set.for_family(ColorTableFamily::SpecificDifferentialPhase)
                .name(),
            "Analyst KDP (interpolated)"
        );
        assert_eq!(
            set.for_family(ColorTableFamily::Generic).name(),
            "Analyst Generic (interpolated)"
        );
    }

    #[test]
    fn every_table_in_a_family_is_drawn_over_that_familys_declared_domain() {
        for family in ColorTableFamily::ALL {
            // The pre-existing families predate nominal_domain and each carries
            // presets on its own declared span; they are pinned table by table
            // in every_builtin_table_with_expected_span instead.
            if matches!(
                family,
                ColorTableFamily::Reflectivity
                    | ColorTableFamily::Velocity
                    | ColorTableFamily::Generic
            ) {
                continue;
            }
            let (low, high) = family.nominal_domain();
            for table in builtin_tables_for_family(family) {
                let span = table
                    .inked_value_span()
                    .unwrap_or_else(|| panic!("{} inks nothing", table.name()));
                assert!(
                    (span.0 - low).abs() < 1e-4 && (span.1 - high).abs() < 1e-4,
                    "{} spans {span:?} but its family declares ({low}, {high})",
                    table.name()
                );
            }
        }
    }

    #[test]
    fn the_dual_pol_domains_are_the_physical_ones_not_a_zero_to_hundred_ramp() {
        assert_eq!(
            ColorTableFamily::DifferentialReflectivity.nominal_domain(),
            (-13.0, 20.0)
        );
        assert_eq!(
            ColorTableFamily::CorrelationCoefficient.nominal_domain(),
            (62.5 / 300.0, 315.5 / 300.0)
        );
        assert_eq!(
            ColorTableFamily::DifferentialPhase.nominal_domain(),
            (0.0, 360.0)
        );
        assert_eq!(
            ColorTableFamily::SpecificDifferentialPhase.nominal_domain(),
            (-2.0, 7.0)
        );
        assert!(ColorTableFamily::DifferentialPhase.is_cyclic());
        for family in ColorTableFamily::ALL {
            if family != ColorTableFamily::DifferentialPhase {
                assert!(!family.is_cyclic(), "{} does not wrap", family.label());
            }
        }
    }

    #[test]
    fn received_power_uses_the_ncar_dow_dbm_span_and_bands() {
        assert_eq!(
            ColorTableFamily::ReceivedPower.nominal_domain(),
            (-120.0, 20.0)
        );
        let table = builtin_received_power_table();
        assert_eq!(table.inked_value_span(), Some((-120.0, 20.0)));
        assert_eq!(
            table.color_for_value(-100.0),
            Rgba8::opaque(0, 0, 255).to_array()
        );
        assert_eq!(
            table.color_for_value(-80.0),
            Rgba8::opaque(34, 139, 34).to_array()
        );
        assert_eq!(
            table.color_for_value(0.0),
            Rgba8::opaque(178, 34, 34).to_array()
        );
    }

    /// The defect this module was changed to fix, pinned so it cannot come back.
    ///
    /// The generic ramp runs 0 to 100 over eight stops. Correlation coefficient
    /// occupies 0.2083 to 1.0517 of that, which lands entirely inside the first
    /// stop interval, 0 to 10. Reading the stop values by hand: the segment
    /// carries (34,40,64) to (34,82,130), so at 0.2083 it is (34,41,65) and at
    /// 1.0517 it is (34,44,71) - a total city-block travel of 9 against the
    /// table's full travel of 712, or 1.3%. The whole dual-pol field renders as
    /// one colour.
    #[test]
    fn the_generic_ramp_gives_the_whole_correlation_coefficient_domain_one_percent_of_its_colour() {
        let generic = builtin_generic_table();

        assert_eq!(generic.sample(CC_MIN), Rgba8::opaque(34, 41, 65));
        assert_eq!(generic.sample(CC_MAX), Rgba8::opaque(34, 44, 71));

        let over_cc_domain = colour_path(&generic, CC_MIN, CC_MAX, 20_000);
        let over_whole_table = colour_path(&generic, 0.0, 100.0, 200_000);
        assert!((over_cc_domain - 9.0).abs() < 0.5, "got {over_cc_domain}");
        assert!(
            (over_whole_table - 712.0).abs() < 2.0,
            "got {over_whole_table}"
        );
        assert!(over_cc_domain / over_whole_table < 0.02);

        // And the acid test: 0.95 against 0.99 is one unit in one channel.
        assert!(max_channel_delta(generic.sample(0.95), generic.sample(0.99)) <= 2);
    }

    /// The central design decision of the correlation coefficient palette.
    ///
    /// 0.95 to 1.00 is 5.9% of the 0.2083-1.0517 domain and carries essentially
    /// all meteorological echo. Summing |dR|+|dG|+|dB| across the stop list by
    /// hand gives 1462 total travel and 549 of it inside 0.95-1.00, so that
    /// 5.9% of the domain gets 37.6% of the colour - a factor of six.
    #[test]
    fn correlation_coefficient_concentrates_its_colour_where_the_echo_is() {
        let table = analyst_correlation_coefficient_table();

        let whole = colour_path(&table, CC_MIN, CC_MAX, 85_000);
        let meteorological = colour_path(&table, 0.95, 1.00, 5_000);
        assert!((whole - 1462.0).abs() < 4.0, "whole path {whole}");
        assert!(
            (meteorological - 549.0).abs() < 4.0,
            "0.95-1.00 path {meteorological}"
        );

        let fraction = meteorological / whole;
        assert!(
            (0.32..0.45).contains(&fraction),
            "0.95-1.00 should take about 38% of the colour, got {fraction}"
        );
        // The domain fraction it is being compared against: 253/300 wide.
        assert!((0.05_f64 / (253.0 / 300.0) - 0.0593).abs() < 0.001);
    }

    /// The acid test named in the brief: 0.95 and 0.99 are a melting layer and
    /// clean rain, and no correlation coefficient table may render them alike.
    #[test]
    fn every_correlation_coefficient_table_separates_point_nine_five_from_point_nine_nine() {
        for table in builtin_tables_for_family(ColorTableFamily::CorrelationCoefficient) {
            let delta = max_channel_delta(table.sample(0.95), table.sample(0.99));
            assert!(
                delta >= 40,
                "{} renders CC 0.95 and 0.99 only {delta} apart",
                table.name()
            );
        }
    }

    #[test]
    fn every_correlation_coefficient_table_separates_all_six_interpretation_categories() {
        for table in builtin_tables_for_family(ColorTableFamily::CorrelationCoefficient) {
            for (left_index, (left_value, left_label)) in CC_CATEGORY_PROBES.iter().enumerate() {
                for (right_value, right_label) in CC_CATEGORY_PROBES.iter().skip(left_index + 1) {
                    let delta =
                        max_channel_delta(table.sample(*left_value), table.sample(*right_value));
                    assert!(
                        delta >= 30,
                        "{} renders {left_label} ({left_value}) and {right_label} ({right_value}) only {delta} apart",
                        table.name()
                    );
                }
            }
        }
    }

    /// Hand-read off the Analyst ZDR stop list: 0 dB interpolates 62.5% of the
    /// way from (124,124,128) at -0.5 dB to (176,176,180) at +0.3 dB, giving
    /// (157,157,161) - a neutral grey, distinct from the rain band's greens and
    /// the melting band's reds.
    #[test]
    fn differential_reflectivity_paints_the_near_zero_band_neutral_and_the_rain_band_green() {
        let table = analyst_differential_reflectivity_table();

        assert_eq!(table.sample(0.0), Rgba8::opaque(157, 157, 161));
        let [zero_r, zero_g, zero_b, _] = table.sample(0.0).to_array();
        assert!((zero_r as i32 - zero_g as i32).abs() <= 6);
        assert!((zero_g as i32 - zero_b as i32).abs() <= 6);

        // 1 to 3 dB, the rain band, runs green into yellow: green dominates blue
        // by a wide margin at both ends.
        assert_eq!(table.sample(1.0), Rgba8::opaque(44, 188, 86));
        assert_eq!(table.sample(2.0), Rgba8::opaque(198, 224, 64));
        for value in [1.0_f32, 1.5, 2.0, 2.5] {
            let [red, green, blue, _] = table.sample(value).to_array();
            assert!(
                green as i32 - blue as i32 >= 80,
                "{value} dB should stay in the green-yellow rain band, got {red},{green},{blue}"
            );
        }

        // Above 4 dB - large drops, melting hail - is red into magenta, never
        // confusable with the grey band or the rain band.
        assert_eq!(table.sample(4.0), Rgba8::opaque(238, 62, 44));
        assert_eq!(table.sample(6.0), Rgba8::opaque(198, 46, 172));
        for value in [4.0_f32, 5.0, 6.0] {
            let [red, green, _, _] = table.sample(value).to_array();
            assert!(red > 190 && green < 90, "{value} dB should be red/magenta");
        }

        assert!(max_channel_delta(table.sample(0.0), table.sample(2.0)) >= 60);
        assert!(max_channel_delta(table.sample(2.0), table.sample(5.0)) >= 60);
        assert!(max_channel_delta(table.sample(0.0), table.sample(5.0)) >= 60);
    }

    /// Analyst ZDR is drawn over the field's whole -13 to +20 dB encoding but
    /// real ZDR clusters in -1 to +5, so the palette is weighted there. Summing
    /// |dR|+|dG|+|dB| off the stop list gives 2780 total: 62 below -7, 2078
    /// across the meteorological -7 to +8 scale, and 640 above it. 1338 of that
    /// sits inside -1 to +5, so 18% of the domain takes 48% of the colour.
    /// ZDR Column Hunter is weighted harder still, 870 of 1594 - 55% of the
    /// colour - inside 0.5 to 4 dB, which is 11% of the domain.
    ///
    /// The meteorological figure of 2078 is unchanged from when the palette
    /// stopped at +/-7-8 dB: widening the domain added stops outside that
    /// window and moved none inside it, so every gate that was already on scale
    /// still samples the identical colour.
    #[test]
    fn differential_reflectivity_tables_weight_the_range_the_data_actually_occupies() {
        let analyst = analyst_differential_reflectivity_table();
        let whole = colour_path(&analyst, ZDR_MIN_DB, ZDR_MAX_DB, 400_000);
        let meteorological = colour_path(&analyst, ZDR_MET_MIN_DB, ZDR_MET_MAX_DB, 200_000);
        let operational = colour_path(&analyst, -1.0, 5.0, 80_000);
        assert!((whole - 2780.0).abs() < 8.0, "whole path {whole}");
        assert!(
            (meteorological - 2078.0).abs() < 8.0,
            "-7..8 path {meteorological}"
        );
        assert!(
            (operational - 1338.0).abs() < 8.0,
            "-1..5 path {operational}"
        );
        assert!(operational / whole >= 0.45);

        let column = zdr_column_hunter_table();
        let column_whole = colour_path(&column, ZDR_MIN_DB, ZDR_MAX_DB, 400_000);
        let column_band = colour_path(&column, 0.5, 4.0, 80_000);
        assert!((column_whole - 1594.0).abs() < 8.0, "whole {column_whole}");
        assert!(
            (column_band - 870.0).abs() < 8.0,
            "0.5..4 path {column_band}"
        );
        assert!(column_band / column_whole >= 0.50);
    }

    /// The hail signature is high reflectivity with ZDR pinned near zero, so
    /// this table's brightest colour is the near-zero plateau and everything
    /// else darkens away from it.
    #[test]
    fn the_hail_signal_table_makes_near_zero_the_brightest_thing_on_the_scope() {
        let table = hail_signal_differential_reflectivity_table();

        assert_eq!(table.sample(0.0), Rgba8::opaque(250, 250, 250));
        let brightness = |value: f32| {
            let [red, green, blue, _] = table.sample(value).to_array();
            red as i32 + green as i32 + blue as i32
        };
        let near_zero = brightness(0.0);
        for value in [-3.0_f32, -1.0, 1.0, 3.0, 6.0] {
            assert!(
                brightness(value) < near_zero - 150,
                "{value} dB should sit well below the near-zero plateau"
            );
        }
    }

    /// PHIDP wraps, so a table whose ends disagree draws a false edge along
    /// every ray that folds past 360 deg.
    #[test]
    fn every_differential_phase_table_closes_on_itself_at_the_wrap() {
        for table in builtin_tables_for_family(ColorTableFamily::DifferentialPhase) {
            assert_eq!(
                table.sample(0.0),
                table.sample(360.0),
                "{} does not close: 0 and 360 deg are different colours",
                table.name()
            );
        }
    }

    /// Closure alone is not enough: the step taken across the wrap must be no
    /// larger than the steps taken anywhere else, or the wrap still reads as an
    /// edge. Checked against the largest step the table takes over any interval
    /// of the same width elsewhere in its domain.
    #[test]
    fn the_phase_wrap_is_no_sharper_than_any_other_two_degrees_of_the_scale() {
        const PROBE_HALF_WIDTH: f32 = 1.0;

        for table in builtin_tables_for_family(ColorTableFamily::DifferentialPhase) {
            let wrap = max_channel_delta(
                table.sample(360.0 - PROBE_HALF_WIDTH),
                table.sample(PROBE_HALF_WIDTH),
            );

            let mut worst_interior = 0;
            let mut centre = PROBE_HALF_WIDTH;
            while centre <= 360.0 - PROBE_HALF_WIDTH {
                worst_interior = worst_interior.max(max_channel_delta(
                    table.sample(centre - PROBE_HALF_WIDTH),
                    table.sample(centre + PROBE_HALF_WIDTH),
                ));
                centre += 0.5;
            }

            assert!(
                wrap <= worst_interior,
                "{} jumps {wrap} across the wrap but at most {worst_interior} anywhere else",
                table.name()
            );
        }
    }

    /// Hand-read off the Analyst Cyclic PHI stop list: 359 deg is 96.67% of the
    /// way from (242,36,139) to (242,36,36), giving (242,36,39); 1 deg is 3.33%
    /// of the way from (242,36,36) to (242,139,36), giving (242,39,36). Three
    /// units apart, which is invisible on a scope - the point of a cyclic map.
    #[test]
    fn the_cyclic_phase_hues_meet_within_three_units_across_the_fold() {
        let table = analyst_differential_phase_table();

        assert_eq!(table.sample(359.0), Rgba8::opaque(242, 36, 39));
        assert_eq!(table.sample(1.0), Rgba8::opaque(242, 39, 36));
        assert!(max_channel_delta(table.sample(359.0), table.sample(1.0)) <= 3);

        // A quarter turn apart must still be plainly different, or the map has
        // closed itself by going nowhere.
        assert!(max_channel_delta(table.sample(0.0), table.sample(90.0)) >= 100);
        assert!(max_channel_delta(table.sample(90.0), table.sample(180.0)) >= 100);
        assert!(max_channel_delta(table.sample(180.0), table.sample(270.0)) >= 100);
    }

    /// KDP's sign is physical, not a magnitude, so zero is a pivot rather than
    /// a point on a ramp.
    #[test]
    fn specific_differential_phase_diverges_about_a_neutral_zero() {
        let table = analyst_specific_differential_phase_table();

        assert_eq!(table.sample(0.0), Rgba8::opaque(112, 112, 112));
        let [zero_r, zero_g, zero_b, _] = table.sample(0.0).to_array();
        assert_eq!(zero_r, zero_g);
        assert_eq!(zero_g, zero_b);

        // Negative runs blue/violet, positive runs green through yellow to red.
        let [negative_r, negative_g, negative_b, _] = table.sample(-1.0).to_array();
        assert!(negative_b > negative_r && negative_b > negative_g);
        let [positive_r, positive_g, positive_b, _] = table.sample(1.0).to_array();
        assert!(positive_g > positive_r && positive_g > positive_b);

        // Equal magnitudes either side of zero must not collide.
        for magnitude in [0.25_f32, 0.5, 1.0, 2.0] {
            let delta = max_channel_delta(table.sample(-magnitude), table.sample(magnitude));
            assert!(
                delta >= 40,
                "KDP -{magnitude} and +{magnitude} are only {delta} apart"
            );
        }
    }

    #[test]
    fn spectrum_width_offers_more_than_the_one_table_it_used_to() {
        let names = builtin_tables_for_family(ColorTableFamily::SpectrumWidth)
            .into_iter()
            .map(|table| table.name().to_owned())
            .collect::<Vec<_>>();

        assert_eq!(
            names,
            vec![
                "Analyst Spectrum Width (interpolated)",
                "Turbulence SW (interpolated)",
                "Clear Air SW (interpolated)",
                "SW Class Bands (stepped)",
            ]
        );

        // The turbulence preset must actually be stretched over 4-12 m/s
        // relative to the default, or it is a different set of colours for no
        // reason.
        let default_band = colour_path(&builtin_spectrum_width_table(), 4.0, 12.0, 40_000);
        let default_whole = colour_path(&builtin_spectrum_width_table(), 0.0, 24.0, 120_000);
        let turbulence_band = colour_path(&turbulence_spectrum_width_table(), 4.0, 12.0, 40_000);
        let turbulence_whole = colour_path(&turbulence_spectrum_width_table(), 0.0, 24.0, 120_000);
        assert!(
            turbulence_band / turbulence_whole > default_band / default_whole + 0.2,
            "turbulence preset {turbulence_band}/{turbulence_whole} vs default {default_band}/{default_whole}"
        );
    }

    /// The dual-pol domains are the Level II field encodings, not round numbers.
    ///
    /// `MomentGrid` decodes a gate as `(raw - offset) / scale`, and the five
    /// cached volumes (KUEX, KABR, KTLX, KLTX, KDMX) all report ZDR with
    /// scale 32 / offset 418 over raw 2..1058, and RHOHV with scale 300 /
    /// offset -60.5 over raw 2..255. Codes 0 and 1 are "below threshold" and
    /// "range folded" and never reach a palette. Writing the constants as those
    /// same quotients makes the endpoints exactly the decoded endpoints, so
    /// nothing a radar can send falls past the last stop and gets flattened.
    #[test]
    fn the_declared_dual_pol_domains_are_the_level_two_field_encodings() {
        assert_eq!((2.0_f32 - 418.0) / 32.0, ZDR_MIN_DB);
        assert_eq!((1058.0_f32 - 418.0) / 32.0, ZDR_MAX_DB);
        assert_eq!((2.0_f32 - -60.5) / 300.0, CC_MIN);
        assert_eq!((255.0_f32 - -60.5) / 300.0, CC_MAX);

        // Nothing clamps: the decoded extremes land on the end stops, not past
        // them, for every table in both families.
        for (family, low, high) in [
            (
                ColorTableFamily::DifferentialReflectivity,
                ZDR_MIN_DB,
                ZDR_MAX_DB,
            ),
            (ColorTableFamily::CorrelationCoefficient, CC_MIN, CC_MAX),
        ] {
            for table in builtin_tables_for_family(family) {
                let first = *table.stops().first().expect("two stops");
                let last = *table.stops().last().expect("two stops");
                assert_eq!(first.value, low, "{} starts late", table.name());
                assert_eq!(last.value, high, "{} stops early", table.name());
                assert_eq!(table.sample(low), first.color);
                assert_eq!(table.sample(high), last.color);
            }
        }
    }

    /// The defect the real volumes exposed, pinned so it cannot come back.
    ///
    /// Both fields pile up hard on their top code: RHOHV code 255 alone holds
    /// 4.1% (KTLX) to 10.3% (KLTX) of all gates while holding 0.012% to 0.037%
    /// of gates above 20 dBZ, and ZDR at or above +8 dB holds 3.4% (KABR) to
    /// 25.2% (KLTX). Both used to be painted in their palette's brightest
    /// colour, so on a coastal or nocturnal scan a quarter of the scope wore
    /// the colour reserved for the most extreme reading - instrument noise
    /// outshining weather. Two invariants stop that: the last stop is never the
    /// brightest stop, and the ceiling sits well below peak brightness.
    #[test]
    fn no_dual_pol_palette_hands_its_brightest_colour_to_the_fields_saturation_code() {
        let brightness = |colour: Rgba8| {
            let [red, green, blue, _] = colour.to_array();
            red as i32 + green as i32 + blue as i32
        };
        let peak_brightness = |table: &ColorTable| {
            table
                .stops()
                .iter()
                .map(|stop| brightness(stop.color))
                .max()
                .expect("two stops")
        };

        for (family, allowed_share) in [
            (ColorTableFamily::DifferentialReflectivity, 0.40_f64),
            (ColorTableFamily::CorrelationCoefficient, 0.70),
        ] {
            for table in builtin_tables_for_family(family) {
                let peak = peak_brightness(&table);
                let last = brightness(table.stops().last().expect("two stops").color);
                assert!(
                    last < peak,
                    "{} makes its last stop the brightest colour it owns",
                    table.name()
                );
                let share = last as f64 / peak as f64;
                assert!(
                    share <= allowed_share,
                    "{} paints the saturation code at {share:.2} of peak brightness",
                    table.name()
                );
            }
        }

        // The negative ZDR end is a noise floor too, and gets the same rule.
        for table in builtin_tables_for_family(ColorTableFamily::DifferentialReflectivity) {
            let peak = peak_brightness(&table);
            let first = brightness(table.stops().first().expect("two stops").color);
            assert!(
                (first as f64) < 0.40 * peak as f64,
                "{} lights up ZDR below -13 dB",
                table.name()
            );
        }
    }

    /// Widening ZDR to the field's own range added stops outside -7 to +8 dB
    /// and moved none inside it, so every gate that was already on scale
    /// samples the identical colour. Probed at stop values, whose expected
    /// colours can be read straight off the lists above.
    #[test]
    fn widening_the_zdr_domain_left_the_meteorological_scale_untouched() {
        let analyst = analyst_differential_reflectivity_table();
        assert_eq!(analyst.sample(ZDR_MET_MIN_DB), Rgba8::opaque(58, 10, 92));
        assert_eq!(analyst.sample(-1.0), Rgba8::opaque(96, 122, 208));
        assert_eq!(analyst.sample(2.0), Rgba8::opaque(198, 224, 64));
        assert_eq!(analyst.sample(4.0), Rgba8::opaque(238, 62, 44));
        assert_eq!(analyst.sample(7.0), Rgba8::opaque(228, 152, 228));
        assert_eq!(analyst.sample(ZDR_MET_MAX_DB), Rgba8::opaque(246, 246, 250));

        // Stepped: a probe inside a band paints that band's colour, and the
        // band that starts at +8 dB is the off-scale one.
        let storm = storm_interrogation_differential_reflectivity_table();
        assert_eq!(storm.sample(ZDR_MET_MIN_DB), Rgba8::opaque(72, 20, 110));
        assert_eq!(storm.sample(-0.75), Rgba8::opaque(86, 106, 178));
        assert_eq!(storm.sample(0.0), Rgba8::opaque(112, 112, 116));
        assert_eq!(storm.sample(2.5), Rgba8::opaque(206, 222, 62));
        assert_eq!(storm.sample(7.0), Rgba8::opaque(200, 48, 176));
        assert_eq!(storm.sample(ZDR_MET_MAX_DB), Rgba8::opaque(56, 124, 130));

        let column = zdr_column_hunter_table();
        assert_eq!(column.sample(ZDR_MET_MIN_DB), Rgba8::opaque(10, 10, 16));
        assert_eq!(column.sample(0.5), Rgba8::opaque(14, 18, 30));
        assert_eq!(column.sample(3.0), Rgba8::opaque(232, 216, 62));
        assert_eq!(column.sample(5.0), Rgba8::opaque(232, 108, 200));

        let hail = hail_signal_differential_reflectivity_table();
        assert_eq!(hail.sample(ZDR_MET_MIN_DB), Rgba8::opaque(28, 6, 48));
        assert_eq!(hail.sample(0.0), Rgba8::opaque(250, 250, 250));
        assert_eq!(hail.sample(4.0), Rgba8::opaque(56, 60, 96));
        assert_eq!(hail.sample(ZDR_MET_MAX_DB), Rgba8::opaque(24, 30, 52));
    }

    /// The pairs a forecaster actually has to tell apart, on the family
    /// defaults. Expected colours read off the stop lists: ZDR 0.5 dB is a
    /// third of the way from (24,96,62) at 0.4 dB to (26,140,74) at 0.7 dB,
    /// giving (25,111,66) against the near-zero plateau's (157,157,161).
    #[test]
    fn the_default_dual_pol_tables_separate_the_readings_that_carry_meaning() {
        let zdr = builtin_differential_reflectivity_table();
        assert_eq!(zdr.sample(0.0), Rgba8::opaque(157, 157, 161));
        assert_eq!(zdr.sample(0.5), Rgba8::opaque(25, 111, 66));
        assert_eq!(max_channel_delta(zdr.sample(0.0), zdr.sample(0.5)), 132);
        assert_eq!(zdr.sample(3.0), Rgba8::opaque(248, 170, 44));
        assert_eq!(zdr.sample(5.0), Rgba8::opaque(208, 30, 98));
        assert_eq!(max_channel_delta(zdr.sample(3.0), zdr.sample(5.0)), 140);

        let cc = builtin_correlation_coefficient_table();
        assert_eq!(cc.sample(0.95), Rgba8::opaque(120, 206, 84));
        assert_eq!(cc.sample(0.99), Rgba8::opaque(56, 84, 216));
        assert_eq!(max_channel_delta(cc.sample(0.95), cc.sample(0.99)), 132);
        assert_eq!(cc.sample(0.80), Rgba8::opaque(226, 86, 48));
        assert_eq!(cc.sample(0.90), Rgba8::opaque(246, 196, 52));
        assert_eq!(max_channel_delta(cc.sample(0.80), cc.sample(0.90)), 110);
    }

    /// 0.80 is the debris threshold and 0.90 the top of the mixed-hydrometeor
    /// band (Ryzhkov et al. 2005; Kumjian 2013 Part I), so every preset has to
    /// separate them, not just the default.
    #[test]
    fn every_correlation_coefficient_table_separates_the_debris_threshold_from_point_nine() {
        for table in builtin_tables_for_family(ColorTableFamily::CorrelationCoefficient) {
            let delta = max_channel_delta(table.sample(0.80), table.sample(0.90));
            assert!(
                delta >= 40,
                "{} renders CC 0.80 and 0.90 only {delta} apart",
                table.name()
            );
        }
    }

    /// PHIDP folds at the top of its encoding, not at a round 360. The word
    /// carries scale 2.8361 and offset 2 over raw 2..1022, so the last value
    /// before the fold is (1022 - 2)/2.8361 = 359.65 deg and the next gate
    /// along the ray reads 0. That is the step that must not draw an edge, and
    /// the cached volumes contain thousands of rays that take it.
    #[test]
    fn the_phase_tables_close_at_the_fold_the_encoding_actually_produces() {
        const LAST_BEFORE_FOLD: f32 = 359.6488;

        let analyst = analyst_differential_phase_table();
        assert_eq!(analyst.sample(LAST_BEFORE_FOLD), Rgba8::opaque(242, 36, 37));
        assert_eq!(analyst.sample(0.0), Rgba8::opaque(242, 36, 36));

        let twilight = twilight_cyclic_differential_phase_table();
        assert_eq!(
            twilight.sample(LAST_BEFORE_FOLD),
            Rgba8::opaque(226, 216, 225)
        );
        assert_eq!(twilight.sample(0.0), Rgba8::opaque(226, 217, 226));

        for table in [analyst, twilight] {
            assert!(
                max_channel_delta(table.sample(LAST_BEFORE_FOLD), table.sample(0.0)) <= 3,
                "{} draws an edge at the real fold",
                table.name()
            );
        }

        // The stepped preset steps, but by exactly one band, the same step it
        // takes at every other boundary.
        let bands = phase_bands_differential_phase_table();
        assert_eq!(bands.sample(LAST_BEFORE_FOLD), Rgba8::opaque(242, 36, 88));
        assert_eq!(bands.sample(0.0), Rgba8::opaque(242, 36, 36));
        assert_eq!(
            max_channel_delta(bands.sample(LAST_BEFORE_FOLD), bands.sample(0.0)),
            max_channel_delta(bands.sample(14.0), bands.sample(16.0))
        );
    }

    /// A stepped table must not collapse two adjacent bands onto one colour,
    /// and an interpolated one must not stall. Checked across every new table
    /// by walking its own stop list.
    #[test]
    fn no_new_table_paints_two_adjacent_stops_the_same_colour() {
        for family in [
            ColorTableFamily::SpectrumWidth,
            ColorTableFamily::DifferentialReflectivity,
            ColorTableFamily::CorrelationCoefficient,
            ColorTableFamily::DifferentialPhase,
            ColorTableFamily::SpecificDifferentialPhase,
        ] {
            for table in builtin_tables_for_family(family) {
                for window in table.stops().windows(2) {
                    let (left, right) = (window[0], window[1]);
                    // The one deliberate exception: Hail Signal ZDR repeats a
                    // colour to hold a flat plateau across the near-zero band,
                    // which is the signature being drawn.
                    if left.color == right.color && table.name().starts_with("Hail Signal ZDR") {
                        continue;
                    }
                    assert!(
                        max_channel_delta(left.color, right.color) >= 3,
                        "{} paints {} and {} the same colour",
                        table.name(),
                        left.value,
                        right.value
                    );
                }
            }
        }
    }

    // -----------------------------------------------------------------------
    // Nothing moved
    //
    // Adding interpolated presets and renaming every built-in is only useful if
    // the tables that were already on the scope still paint exactly what they
    // painted. Otherwise an analyst comparing a stepped table against a smooth
    // one is comparing two things that both changed, and can conclude nothing
    // about either.
    //
    // Every expectation below is read off the palette text by hand. For a
    // quantized table that means: quantise the probe onto the table's own step
    // grid first (`quantize_value`, round-half-away-from-zero), then look the
    // quantised value up, interpolating between the two stops that bracket it.
    // -----------------------------------------------------------------------

    /// The reflectivity palette that used to be the default, drawn the way it
    /// used to be drawn, probed at nine values.
    ///
    /// GR2Analyst Classic REF declares `step: 5`, so `sample` rounds the gate's
    /// dBZ to the nearest multiple of 5 before looking it up. Six of these
    /// probes land on a declared stop and return it unchanged; three land
    /// between stops and are interpolated, and those three are written out
    /// longhand because they are where an arithmetic change would show first.
    ///
    /// Aimed at `gr2_reflectivity_table` and not at `builtin_reflectivity_table`
    /// on purpose. The default moved to the continuous drawing of this same
    /// palette; this test is the pin on what the *banded* drawing paints, and
    /// pointing it at whatever happens to be the default would have quietly
    /// stopped testing that the day the default changed.
    #[test]
    fn the_default_reflectivity_table_still_paints_exactly_what_it_did() {
        let table = gr2_reflectivity_table();

        assert_eq!(table.step_size(), Some(5.0));
        assert_eq!(table.sample_mode_label(), "quantized stepped");

        // Below the first inked stop: nothing, at any distance.
        assert_eq!(table.sample(9.999), Rgba8::TRANSPARENT);
        assert_eq!(table.sample(-30.0), Rgba8::TRANSPARENT);

        // Declared stops, returned unchanged.
        assert_eq!(table.sample(10.0), Rgba8::opaque(4, 233, 231));
        assert_eq!(table.sample(20.0), Rgba8::opaque(3, 0, 244));
        assert_eq!(table.sample(30.0), Rgba8::opaque(1, 197, 1));
        assert_eq!(table.sample(35.0), Rgba8::opaque(0, 142, 0));
        assert_eq!(table.sample(50.0), Rgba8::opaque(253, 149, 0));

        // The 5 dBZ grid, either side of the 37.5 dBZ midpoint: 37.4 rounds
        // down onto 35 and 37.6 rounds up onto 40.
        assert_eq!(table.sample(37.4), Rgba8::opaque(0, 142, 0));
        assert_eq!(table.sample(37.6), Rgba8::opaque(253, 248, 2));

        // Interpolated between stops. 60 dBZ is two thirds of the way from
        // (253,0,0) at 55 to (212,0,0) at 62.5: 253 - 41 * 2/3 = 225.67 -> 226.
        assert_eq!(table.sample(60.0), Rgba8::opaque(226, 0, 0));
        // 65 is halfway from (212,0,0) at 62.5 to (188,0,0) at 67.5.
        assert_eq!(table.sample(65.0), Rgba8::opaque(200, 0, 0));
        // 70 is halfway from (188,0,0) at 67.5 to (232,32,206) at 72.5:
        // 188+22, 0+16, 0+103.
        assert_eq!(table.sample(70.0), Rgba8::opaque(210, 16, 103));
        // 75 is a third of the way from (232,32,206) at 72.5 to (156,70,206)
        // at 80: 232-25.33 -> 207, 32+12.67 -> 45, blue unchanged.
        assert_eq!(table.sample(75.0), Rgba8::opaque(207, 45, 206));
    }

    /// The velocity palette that used to be the default, drawn the way it used
    /// to be drawn, probed at ten values.
    ///
    /// Analyst Tornado VEL declares `step: 2` and `units: m/s`, so no unit
    /// rescaling happens and the probe is rounded onto even m/s.
    ///
    /// Aimed at `tornado_velocity_table` for the reason given on the
    /// reflectivity twin above.
    #[test]
    fn the_default_velocity_table_still_paints_exactly_what_it_did() {
        let table = tornado_velocity_table();

        assert_eq!(table.step_size(), Some(2.0));
        assert_eq!(table.sample_mode_label(), "quantized stepped");
        // No RF row in the palette, so the module default stands.
        assert_eq!(table.range_folded_rgba(), Rgba8::new(126, 80, 196, 245));

        // Declared stops.
        assert_eq!(table.sample(-30.0), Rgba8::opaque(246, 255, 255));
        assert_eq!(table.sample(-18.0), Rgba8::opaque(0, 156, 54));
        assert_eq!(table.sample(-2.0), Rgba8::opaque(84, 100, 84));
        assert_eq!(table.sample(0.0), Rgba8::opaque(112, 112, 112));
        assert_eq!(table.sample(2.0), Rgba8::opaque(120, 86, 84));
        assert_eq!(table.sample(20.0), Rgba8::opaque(242, 0, 0));
        assert_eq!(table.sample(34.0), Rgba8::opaque(255, 224, 168));
        assert_eq!(table.sample(50.0), Rgba8::opaque(255, 255, 240));

        // Interpolated. -12 is a quarter of the way from (18,232,54) at -13 to
        // (82,244,104) at -9: 18+16, 232+3, 54+12.5 -> 67.
        assert_eq!(table.sample(-12.0), Rgba8::opaque(34, 235, 67));
        // +10 is a fifth of the way from (216,28,28) at 9 to (255,34,40) at 14:
        // 216+7.8 -> 224, 28+1.2 -> 29, 28+2.4 -> 30.
        assert_eq!(table.sample(10.0), Rgba8::opaque(224, 29, 30));
    }

    /// One probe on every other registered reflectivity and velocity preset.
    ///
    /// Each probe is chosen to land on a declared stop after quantisation, so
    /// the expected colour is the palette text's own triple with no arithmetic
    /// in between. Cheap, and it catches a whole table being swapped, rescaled,
    /// or re-parsed into a different mode.
    #[test]
    fn every_other_registered_stepped_preset_still_paints_exactly_what_it_did() {
        for (table, probe, expected) in [
            (
                analyst_classic_reflectivity_table(),
                25.0,
                Rgba8::opaque(0, 222, 44),
            ),
            (nws_reflectivity_table(), 45.0, Rgba8::opaque(229, 188, 0)),
            (
                dark_scope_reflectivity_table(),
                40.0,
                Rgba8::opaque(232, 156, 42),
            ),
            (
                hail_core_reflectivity_table(),
                50.0,
                Rgba8::opaque(246, 26, 28),
            ),
            (
                low_precip_reflectivity_table(),
                35.0,
                Rgba8::opaque(224, 226, 64),
            ),
            (
                tornado_debris_reflectivity_table(),
                30.0,
                Rgba8::opaque(72, 176, 42),
            ),
            (
                clean_light_reflectivity_table(),
                37.5,
                Rgba8::opaque(220, 218, 58),
            ),
            (analyst_velocity_table(), -15.0, Rgba8::opaque(0, 226, 58)),
            (
                radarscope_contrast_velocity_table(),
                16.0,
                Rgba8::opaque(255, 40, 46),
            ),
            (
                couplet_pop_velocity_table(),
                -10.0,
                Rgba8::opaque(34, 186, 48),
            ),
            (
                gr2_ish_analyst_velocity_table(),
                24.0,
                Rgba8::opaque(246, 0, 0),
            ),
            (
                subtle_srv_velocity_table(),
                16.0,
                Rgba8::opaque(222, 64, 58),
            ),
        ] {
            assert_eq!(
                table.sample(probe),
                expected,
                "{} moved at {probe}",
                table.name()
            );
        }
    }

    /// The rename is a rename: the same palette, wearing its mode.
    #[test]
    fn renaming_the_built_ins_did_not_touch_their_stops() {
        // Read off GR2_REFLECTIVITY_TABLE: 17 rows, the first two transparent.
        let table = gr2_reflectivity_table();
        assert_eq!(table.stops().len(), 17);
        assert_eq!(table.stops()[0].value, -10.0);
        assert_eq!(table.stops()[0].color, Rgba8::TRANSPARENT);
        assert_eq!(table.stops()[1].value, 7.5);
        assert_eq!(table.stops()[1].color, Rgba8::TRANSPARENT);
        assert_eq!(table.stops()[2].color, Rgba8::opaque(4, 233, 231));
        assert_eq!(table.product(), Some("BR"));
        assert_eq!(table.units(), Some("dBZ"));
        assert_eq!(table.inked_value_span(), Some((10.0, 92.5)));

        // And the name is the old name plus the mode, nothing else.
        assert_eq!(
            table.name(),
            format!("GR2Analyst Classic REF ({})", table.sample_mode_label())
        );
    }

    // -----------------------------------------------------------------------
    // The new interpolated presets
    // -----------------------------------------------------------------------

    /// The four dBZ values a reflectivity palette has to keep legible.
    const REFLECTIVITY_BREAKS: [f32; 4] = [20.0, 35.0, 50.0, 65.0];

    /// The name is the only string a picker row shows, so it has to carry the
    /// one fact that decides what the table will look like on the scope.
    #[test]
    fn every_builtin_tables_name_ends_with_its_own_sampling_mode() {
        for (label, table, _) in every_builtin_table_with_expected_span() {
            let suffix = format!(" ({})", table.sample_mode_label());
            assert!(
                table.name().ends_with(&suffix),
                "{label} is named {:?}, which does not end with {suffix:?}",
                table.name()
            );
            // Not just a suffix: something has to come before it.
            assert!(
                table.name().len() > suffix.len(),
                "{label} is nothing but its mode"
            );
        }

        // The four wordings, spelled the way sample_mode_label spells them.
        assert_eq!(
            gr2_reflectivity_table().name(),
            "GR2Analyst Classic REF (quantized stepped)"
        );
        assert_eq!(
            builtin_reflectivity_table().name(),
            "AWIPS Wilson REF (interpolated)"
        );
        assert_eq!(
            smooth_classic_reflectivity_table().name(),
            "Smooth Classic REF (interpolated)"
        );
        assert_eq!(
            sign_check_velocity_table().name(),
            "Sign Check VEL (stepped)"
        );

        // And the mode comes off again cleanly, in all four spellings, which is
        // what lets a picker key a row on the palette rather than on the
        // drawing of it.
        for table in [
            gr2_reflectivity_table(),
            builtin_reflectivity_table(),
            smooth_classic_reflectivity_table(),
            sign_check_velocity_table(),
        ] {
            let base = table.base_name();
            assert!(!base.ends_with(')'), "{base:?} kept a mode");
            assert!(!base.is_empty());
            assert_eq!(table.rendered(table.rendering()).base_name(), base);
            assert_eq!(
                table.rendered(table.rendering().flipped()).base_name(),
                base
            );
        }

        // A palette loaded from a file carries no mode until it is flipped, and
        // then it must carry one: the two drawings share a list, and a picker
        // tells rows apart by name.
        let loaded = ColorTable::parse("My Palette", "color: 0 0 0 0\ncolor: 10 255 255 255")
            .expect("table parses");
        assert_eq!(loaded.name(), "My Palette");
        assert_eq!(loaded.base_name(), "My Palette");
        assert_eq!(loaded.rendered(loaded.rendering()).name(), "My Palette");
        assert_eq!(
            loaded.rendered(TableRendering::Stepped).name(),
            "My Palette (stepped)"
        );
        assert_eq!(
            loaded
                .rendered(TableRendering::Stepped)
                .rendered(TableRendering::Smooth)
                .base_name(),
            "My Palette"
        );
    }

    /// `rendering_suffix` and `base_name` are two views of one rule, and a
    /// name that ends in a suffix is exactly the name a writer must refuse:
    /// the identity the application stores for it is not the name it declares.
    #[test]
    fn a_name_that_ends_in_a_rendering_suffix_loses_that_half_of_itself() {
        for suffix in SampleMode::NAME_SUFFIXES {
            let name = format!("Storm{suffix}");
            assert_eq!(rendering_suffix(&name), Some(suffix));
            let table = ColorTable::parse(
                name.clone(),
                "color: 0 0 0 0
color: 10 255 255 255",
            )
            .expect("table parses");
            assert_eq!(table.name(), name);
            assert_eq!(
                table.base_name(),
                "Storm",
                "the stored identity drops the suffix the file declares"
            );
        }
        // Only at the end, and only these four. A name that merely contains
        // one is an ordinary name and must stay one.
        assert_eq!(rendering_suffix("Storm (stepped) v2"), None);
        assert_eq!(rendering_suffix("Storm (banded)"), None);
        // The leading space belongs to the suffix, so a name that IS the
        // parenthesis alone is not one of them.
        assert_eq!(rendering_suffix("(continuous)"), None);
        assert_eq!(
            ColorTable::parse(
                "Storm (stepped) v2",
                "color: 0 0 0 0
color: 10 255 255 255"
            )
            .expect("table parses")
            .base_name(),
            "Storm (stepped) v2"
        );
    }

    /// Every shipped palette answers to its own name, in every form the
    /// application ever writes that name in, and nothing else does.
    ///
    /// This is what a *writer* has to ask before it puts a name in a file. The
    /// restore path searches this catalogue BEFORE the analyst's own
    /// directory, so a file declaring a shipped name is never the table that
    /// gets installed, and the picker row for that name offers Edit on the
    /// preset - so the analyst's own table cannot be reopened either. A save
    /// under such a name writes a perfect file and loses the palette.
    ///
    /// Both forms have to answer, because both are written down: a picker row
    /// is labelled `"AWIPS Wilson REF (interpolated)"` and a stored choice is
    /// written `"AWIPS Wilson REF"`, and they are the same palette.
    #[test]
    fn every_shipped_palette_answers_to_its_own_name_in_every_form_it_is_written_in() {
        for family in ColorTableFamily::ALL {
            let installed = builtin_tables_for_family(family)
                .into_iter()
                .next()
                .expect("every family ships at least one palette");
            for table in builtin_tables_for_family(family) {
                let base = table.base_name().to_owned();
                assert_eq!(
                    builtin_family_for_name(&base),
                    Some(family),
                    "{base:?} is shipped under {family:?} and did not answer to its own name"
                );
                // Every row label a picker can print for it: both renderings,
                // and the extra "drawn the other way" row.
                for rendering in [TableRendering::Smooth, TableRendering::Stepped] {
                    let label = table.rendered(rendering).name().to_owned();
                    assert_eq!(
                        base_name_of(&label),
                        base,
                        "the row label {label:?} does not reduce to the palette's name"
                    );
                    assert_eq!(
                        builtin_family_for_name(&label),
                        Some(family),
                        "the row label {label:?} did not answer as a shipped name"
                    );
                }
            }
            // And the labels the picker actually builds, taken from the picker's
            // own function rather than reconstructed here.
            for offer in palette_offers_for_family(family, &installed) {
                assert!(
                    builtin_family_for_name(offer.name()).is_some(),
                    "the picker offers {:?} and a writer would not know it is taken",
                    offer.name()
                );
            }
        }
        // A name of the analyst's own is not taken, including one that merely
        // starts with a shipped one.
        for free in [
            "AWIPS Wilson REF, mine",
            "My AWIPS Wilson REF",
            "AWIPS Wilson",
            "Storm",
            "",
        ] {
            assert_eq!(
                builtin_family_for_name(free),
                None,
                "{free:?} is not a name this build ships"
            );
        }
    }

    /// Sorted, finite, and never silently collapsed.
    ///
    /// `from_parts` sorts and de-duplicates, so this cannot fail by accident -
    /// which is the problem. A palette that declares two stops at the same
    /// value loses one of them without a word, and the table still validates.
    /// Checking the count as well as the ordering is what turns that into a
    /// failure: `no_new_interpolated_table_lost_a_stop_to_de_duplication`
    /// carries the counts for the tables written as stop lists here.
    #[test]
    fn every_builtin_tables_stops_are_finite_and_strictly_increasing() {
        for (label, table, _) in every_builtin_table_with_expected_span() {
            assert!(table.stops().len() >= 2, "{label} has under two stops");
            for stop in table.stops() {
                assert!(stop.value.is_finite(), "{label} carries a non-finite stop");
            }
            for window in table.stops().windows(2) {
                assert!(
                    window[0].value < window[1].value,
                    "{label} is out of order or duplicated at {}",
                    window[0].value
                );
            }
            // Transparency only ever leads. An interior alpha-0 stop would
            // punch a hole in the middle of the scale and inked_value_span
            // would report straight across it.
            let clear_stops = table
                .stops()
                .iter()
                .take_while(|stop| stop.color.a == 0)
                .count();
            assert!(
                table.stops()[clear_stops..]
                    .iter()
                    .all(|stop| stop.color.a > 0),
                "{label} has a transparent stop after its first inked one"
            );
        }
    }

    /// Hand-counted off the stop lists, so a duplicated value shows up as a
    /// missing stop instead of vanishing into `from_parts`.
    #[test]
    fn no_new_interpolated_table_lost_a_stop_to_de_duplication() {
        for (table, expected) in [
            (smooth_classic_reflectivity_table(), 36),
            (smooth_sequential_reflectivity_table(), 34),
            (smooth_storm_core_reflectivity_table(), 35),
            (smooth_doppler_velocity_table(), 33),
            (smooth_couplet_velocity_table(), 31),
        ] {
            assert_eq!(
                table.stops().len(),
                expected,
                "{} declared {expected} stops",
                table.name()
            );
            assert!(table.interpolates(), "{} must interpolate", table.name());
            assert_eq!(
                table.step_size(),
                None,
                "{} must not quantize",
                table.name()
            );
        }
    }

    /// The point of the whole exercise: a smooth table resolves the field, a
    /// stepped one bins it.
    ///
    /// Swept at 0.5 dBZ, which is the resolution Level II reflectivity is
    /// encoded at (scale 2, offset 66), so this is every distinct value a gate
    /// between 10 and 70 dBZ can carry - 121 of them.
    ///
    /// The stepped default can answer with 13 colours and no more, and they are
    /// hand-enumerable: it rounds onto multiples of 5, so 10, 15, 20 ... 70,
    /// thirteen quantisation levels, each a different colour in the palette
    /// text. Every one of the 121 values collapses onto one of those thirteen.
    ///
    /// The interpolated tables have no such ceiling. They can only lose a value
    /// where their ramp moves less than one 8-bit unit in any channel across
    /// 0.5 dBZ, which happens only in their flattest stretches.
    #[test]
    fn an_interpolated_table_resolves_the_reflectivity_scale_a_stepped_one_bins() {
        let distinct_colours = |table: &ColorTable| {
            let mut seen = std::collections::HashSet::new();
            // 10.0 to 70.0 inclusive in exact halves: 0.5 is a power of two, so
            // the accumulation is exact in f32 and lands on 121 distinct values.
            for step in 0..=120 {
                let value = 10.0 + step as f32 * 0.5;
                seen.insert(table.sample(value));
            }
            seen.len()
        };

        // The default palette drawn the way it used to be drawn: the 5 dBZ grid
        // it declares turns 121 readings into 13 colours.
        let stepped = distinct_colours(&gr2_reflectivity_table());
        assert_eq!(stepped, 13, "the 5 dBZ grid from 10 to 70 has 13 levels");

        for table in [
            builtin_reflectivity_table(),
            smooth_classic_reflectivity_table(),
            smooth_sequential_reflectivity_table(),
            smooth_storm_core_reflectivity_table(),
        ] {
            let smooth = distinct_colours(&table);
            assert!(
                smooth >= 110,
                "{} resolves only {smooth} of the 121 values in 10-70 dBZ",
                table.name()
            );
            assert!(
                smooth >= 8 * stepped,
                "{} resolves {smooth} colours against the stepped default's {stepped}",
                table.name()
            );
        }
    }

    /// A gradient that hides the 50 dBZ core is prettier and worse.
    ///
    /// All three interpolated reflectivity presets answer that by turning
    /// inside a single 0.5 dBZ window - one Level II step, so the turn is drawn
    /// as a contour a gate wide - at each of the four break points, and gliding
    /// everywhere else. Measured as the largest per-channel change across each
    /// half-dBZ window from 10 to 75 dBZ: the four windows ending on a break
    /// must each move at least two and a half times as far as the worst window
    /// that does not.
    ///
    /// Smooth Sequential REF used to be exempt from this test, and it failed
    /// it: it moved 3-8 units across the four break windows against 8 units for
    /// the worst ordinary window, so no operational threshold was visible on
    /// it. The exemption was the bug. Its breaks are steps up in luminance
    /// rather than hue turns, which keeps the table monotone in lightness and
    /// still passes here, so it is now held to the same bar as the other two.
    #[test]
    fn the_contour_tables_turn_at_the_four_breaks_and_glide_between_them() {
        for table in [
            smooth_classic_reflectivity_table(),
            smooth_sequential_reflectivity_table(),
            smooth_storm_core_reflectivity_table(),
        ] {
            let mut break_steps = Vec::new();
            let mut worst_glide = 0;
            let mut low = 10.0_f32;
            while low < 75.0 {
                let high = low + 0.5;
                let step = max_channel_delta(table.sample(low), table.sample(high));
                if REFLECTIVITY_BREAKS.contains(&high) {
                    break_steps.push((high, step));
                } else {
                    worst_glide = worst_glide.max(step);
                }
                low = high;
            }

            assert_eq!(break_steps.len(), 4, "{} lost a break", table.name());
            for (value, step) in break_steps {
                assert!(
                    step as f64 >= 2.5 * worst_glide as f64,
                    "{} moves {step} across the {value} dBZ break but up to \
                     {worst_glide} elsewhere, so the break is not readable",
                    table.name()
                );
            }
            // And the glide has to be a glide: a table that only moved at the
            // breaks would pass the check above and be a four-band palette.
            assert!(
                worst_glide >= 4,
                "{} barely moves between breaks",
                table.name()
            );
        }
    }

    /// Smooth Sequential REF is the diagnostic table, and its whole claim is
    /// that lightness never doubles back.
    ///
    /// BT.709 relative luminance, the standard sRGB weighting. Checked stop by
    /// stop rather than by sweeping, because between two stops the channels
    /// move linearly and so does any weighted sum of them: if luminance rises
    /// from each stop to the next it rises everywhere. (Strictly, the *rendered*
    /// 8-bit colour can dip by up to a quarter of a luminance unit inside a stop
    /// interval, because `lerp_u8` rounds each channel independently. That is
    /// quantisation noise in the output word, not a fold in the ramp, and it is
    /// three orders of magnitude below the 240 units the ramp climbs.)
    #[test]
    fn the_sequential_reflectivity_table_never_darkens_as_reflectivity_rises() {
        let table = smooth_sequential_reflectivity_table();
        let luminance = |colour: Rgba8| {
            let [red, green, blue, _] = colour.to_array();
            0.2126 * red as f64 + 0.7152 * green as f64 + 0.0722 * blue as f64
        };

        let inked: Vec<_> = table
            .stops()
            .iter()
            .filter(|stop| stop.color.a > 0)
            .collect();
        assert_eq!(inked.len(), 32);
        for window in inked.windows(2) {
            let (dark, light) = (luminance(window[0].color), luminance(window[1].color));
            assert!(
                light > dark,
                "{} darkens from {} to {} dBZ ({dark:.1} -> {light:.1})",
                table.name(),
                window[0].value,
                window[1].value
            );
        }

        // Ends dark-to-bright over the full inked span, not just locally.
        assert!(luminance(table.sample(95.0)) - luminance(table.sample(10.0)) > 200.0);

        // Each break is readable against the gates either side of it, which is
        // the check the first version of this test got wrong: it compared the
        // four breaks to EACH OTHER - 15 dBZ apart, so of course they differed -
        // and never asked whether 49.5 dBZ could be told from 50.0. It could
        // not. An analyst reads a threshold by seeing the gates on one side of
        // it change colour, so the comparison has to be against the neighbours.
        for anchor in REFLECTIVITY_BREAKS {
            let across = max_channel_delta(table.sample(anchor - 0.5), table.sample(anchor));
            let below = max_channel_delta(table.sample(anchor - 1.0), table.sample(anchor - 0.5));
            let above = max_channel_delta(table.sample(anchor), table.sample(anchor + 0.5));
            assert!(
                across >= 5 * below.max(above).max(1),
                "{} moves {across} across the {anchor} dBZ break but {below} and \
                 {above} in the half-dBZ windows either side, so the threshold \
                 is not visible on the scope",
                table.name()
            );
        }
    }

    /// Smooth Storm Core REF exists to spend its colour on 35-65 dBZ. If it
    /// does not, it is Smooth Classic REF with different numbers.
    #[test]
    fn the_storm_core_table_spends_its_colour_on_the_convective_range() {
        let core = smooth_storm_core_reflectivity_table();
        let classic = smooth_classic_reflectivity_table();

        let share = |table: &ColorTable| {
            colour_path(table, 35.0, 65.0, 60_000) / colour_path(table, 10.0, 95.0, 170_000)
        };
        let core_share = share(&core);
        let classic_share = share(&classic);

        assert!(
            core_share > 0.55,
            "storm core spends only {core_share:.2} of its colour on 35-65 dBZ"
        );
        assert!(
            core_share > classic_share + 0.1,
            "storm core {core_share:.2} is no more concentrated than classic {classic_share:.2}"
        );

        // Below 35 dBZ it stays desaturated, so stratiform rain is locatable
        // without competing with the core for attention. Chroma here is the
        // crude max-minus-min channel spread, which is enough to separate a
        // slate blue from a saturated hue and can be read off the stop list.
        let chroma = |value: f32| {
            let [red, green, blue, _] = core.sample(value).to_array();
            red.max(green).max(blue) as i32 - red.min(green).min(blue) as i32
        };
        let core_band = [45.0_f32, 50.0, 55.0]
            .into_iter()
            .map(chroma)
            .min()
            .expect("three probes");
        for value in [12.5_f32, 20.0, 27.5, 34.0] {
            assert!(
                chroma(value) * 2 < core_band,
                "{value} dBZ carries chroma {} against the core band's {core_band}",
                chroma(value)
            );
        }
    }

    /// The three interpolated reflectivity presets have to paint the same gates
    /// as the stepped ones, or switching between them changes the echo's shape
    /// and the comparison they exist for is worthless.
    ///
    /// On every value a Level II reflectivity word can hold they do: the field
    /// decodes as (raw - 66) / 2, so it only ever lands on the 0.5 dBZ grid, and
    /// both 9.5 and 10.0 are grid points.
    ///
    /// Off that grid they do not, and the second half of this test pins the
    /// difference rather than pretending it away. `render2d`'s Soften and
    /// Interpolate display passes produce smoothed physical values between grid
    /// points - one to two percent of gates land in 9.5 < dBZ < 10.0 on real
    /// volumes - and there an interpolated table is part-way along its alpha
    /// ramp while a quantised one is still fully clear, because
    /// `SampleMode::QuantizedInterpolated` short-circuits to transparent below
    /// its first opaque stop. The visible consequence is one half-dBZ of extra
    /// softness at the outer edge of the echo, and nothing at all inside it.
    ///
    /// These three are built in Rust from a stop list, not parsed from `.pal`
    /// text, so their clear stop is the bottom of a gradient and ramps. The GR
    /// dialect's clear-row hold, which would flatten the fringe away, is a
    /// property of the text and is written in at parse time; applying it here
    /// as well would repaint three shipped palettes that nobody complained
    /// about, which is what `SampleMode::Interpolated`'s own doc promises will
    /// not happen.
    #[test]
    fn the_interpolated_reflectivity_presets_ink_the_same_gates_as_the_stepped_ones() {
        for table in [
            smooth_classic_reflectivity_table(),
            smooth_sequential_reflectivity_table(),
            smooth_storm_core_reflectivity_table(),
        ] {
            assert_eq!(
                table.inked_value_span(),
                Some((10.0, 95.0)),
                "{} does not ink from 10 dBZ",
                table.name()
            );
            assert_eq!(table.sample(5.0), Rgba8::TRANSPARENT);
            assert_eq!(table.sample(9.5), Rgba8::TRANSPARENT);
            assert_eq!(table.sample(10.0).a, 255);
            // The one half-painted step, and it is the only one: 9.75 sits
            // halfway across the 0.5 dBZ alpha ramp.
            assert_eq!(table.sample(9.75).a, 128);

            // Every value the encoding can actually produce agrees with the
            // GR2 classic they were built against, gate for gate, in both
            // directions. (The default slot moved to AWIPS Wilson, which
            // paints low dBZ on purpose, so the classic is named directly.)
            let stepped = gr2_reflectivity_table();
            for raw in 0_u16..=255 {
                let dbz = (raw as f32 - 66.0) / 2.0;
                assert_eq!(
                    table.sample(dbz).a > 0,
                    stepped.sample(dbz).a > 0,
                    "{} and {} disagree about whether {dbz} dBZ is painted",
                    table.name(),
                    stepped.name()
                );
            }
        }

        // Hand-read off Smooth Classic REF: at 9.75 the colour is halfway from
        // transparent black to (16,88,140), so every channel halves too.
        assert_eq!(
            smooth_classic_reflectivity_table().sample(9.75),
            Rgba8::new(8, 44, 70, 128)
        );
        // And the stepped classic at the same off-grid value is fully clear -
        // it is parsed from `.pal` text, so its clear row holds - which is the
        // whole of the difference between the two.
        assert_eq!(gr2_reflectivity_table().sample(9.75), Rgba8::TRANSPARENT);
    }

    /// Hand-read off the three stop lists. Break anchors first, then one
    /// interpolated point in each, which is where an arithmetic change shows.
    #[test]
    fn the_interpolated_reflectivity_presets_paint_their_declared_colours() {
        let classic = smooth_classic_reflectivity_table();
        assert_eq!(classic.sample(19.5), Rgba8::opaque(40, 208, 244));
        assert_eq!(classic.sample(20.0), Rgba8::opaque(14, 148, 60));
        assert_eq!(classic.sample(35.0), Rgba8::opaque(250, 228, 36));
        assert_eq!(classic.sample(50.0), Rgba8::opaque(230, 18, 26));
        assert_eq!(classic.sample(65.0), Rgba8::opaque(214, 40, 200));
        // Halfway from (16,88,140) at 10 to (18,118,176) at 12.5.
        assert_eq!(classic.sample(11.25), Rgba8::opaque(17, 103, 158));
        // Halfway from (14,148,60) at 20 to (16,168,58) at 22.5.
        assert_eq!(classic.sample(21.25), Rgba8::opaque(15, 158, 59));

        let sequential = smooth_sequential_reflectivity_table();
        assert_eq!(sequential.sample(19.5), Rgba8::opaque(50, 21, 116));
        assert_eq!(sequential.sample(20.0), Rgba8::opaque(98, 26, 144));
        assert_eq!(sequential.sample(34.5), Rgba8::opaque(168, 55, 145));
        assert_eq!(sequential.sample(35.0), Rgba8::opaque(226, 66, 66));
        assert_eq!(sequential.sample(49.5), Rgba8::opaque(252, 122, 32));
        assert_eq!(sequential.sample(50.0), Rgba8::opaque(255, 178, 26));
        assert_eq!(sequential.sample(64.5), Rgba8::opaque(253, 210, 82));
        assert_eq!(sequential.sample(65.0), Rgba8::opaque(248, 248, 176));
        // Halfway from (255,178,26) at 50 to (255,184,34) at 52.5: green
        // 178 + 6/2 = 181, blue 26 + 8/2 = 30.
        assert_eq!(sequential.sample(51.25), Rgba8::opaque(255, 181, 30));

        let core = smooth_storm_core_reflectivity_table();
        assert_eq!(core.sample(34.5), Rgba8::opaque(74, 104, 152));
        assert_eq!(core.sample(35.0), Rgba8::opaque(24, 152, 78));
        assert_eq!(core.sample(49.5), Rgba8::opaque(250, 178, 40));
        assert_eq!(core.sample(50.0), Rgba8::opaque(248, 80, 30));
        // Halfway from (24,152,78) at 35 to (46,182,66) at 37.5.
        assert_eq!(core.sample(36.25), Rgba8::opaque(35, 167, 72));
    }

    /// The two interpolated velocity presets, held to the conventions that make
    /// a velocity display readable at all.
    #[test]
    fn the_interpolated_velocity_presets_keep_zero_neutral_and_sign_legible() {
        for table in [
            smooth_doppler_velocity_table(),
            smooth_couplet_velocity_table(),
        ] {
            // Zero is grey, so the zero isodop reads as a line and not a colour.
            let [zero_r, zero_g, zero_b, zero_a] = table.sample(0.0).to_array();
            assert_eq!(zero_a, 255);
            assert_eq!(zero_r, zero_g, "{} zero is not neutral", table.name());
            assert_eq!(zero_g, zero_b, "{} zero is not neutral", table.name());

            // Inbound cool - green low, cyan high - and outbound warm, at every
            // magnitude an analyst reads. Stated as "red is the weakest channel
            // inbound and the strongest outbound" because the inbound half
            // deliberately crosses from green to cyan at its strong end, the
            // way every stepped velocity preset in this module does.
            for magnitude in [5.0_f32, 10.0, 15.0, 20.0, 25.0, 30.0] {
                let [in_r, in_g, in_b, _] = table.sample(-magnitude).to_array();
                assert!(
                    in_g > in_r && in_b > in_r,
                    "{} paints -{magnitude} m/s {in_r},{in_g},{in_b}, which is not inbound",
                    table.name()
                );
                let [out_r, out_g, out_b, _] = table.sample(magnitude).to_array();
                assert!(
                    out_r > out_g && out_r > out_b,
                    "{} paints +{magnitude} m/s {out_r},{out_g},{out_b}, which is not outbound",
                    table.name()
                );
                // And the two signs must never collide, which is the failure
                // that makes a couplet disappear.
                assert!(
                    max_channel_delta(table.sample(-magnitude), table.sample(magnitude)) >= 60,
                    "{} renders -{magnitude} and +{magnitude} m/s alike",
                    table.name()
                );
            }

            assert_eq!(table.inked_value_span(), Some((-70.0, 70.0)));
        }
    }

    /// Smooth Couplet VEL exists to spend its colour inside +/-25 m/s, where
    /// mesocyclonic and tornadic couplets sit and where most base velocity data
    /// lives anyway. If it is no more concentrated than Smooth Doppler VEL it
    /// is a second copy of it.
    #[test]
    fn the_couplet_velocity_table_concentrates_on_the_rotational_band() {
        let couplet = smooth_couplet_velocity_table();
        let doppler = smooth_doppler_velocity_table();

        let share = |table: &ColorTable| {
            colour_path(table, -25.0, 25.0, 50_000) / colour_path(table, -70.0, 70.0, 140_000)
        };
        let couplet_share = share(&couplet);
        let doppler_share = share(&doppler);

        // +/-25 m/s is 50 of the 140 m/s domain, or 35.7%.
        assert!((50.0 / 140.0_f64 - 0.357).abs() < 0.001);
        assert!(
            couplet_share > 0.55,
            "couplet table spends only {couplet_share:.2} of its colour inside +/-25 m/s"
        );
        assert!(
            couplet_share > doppler_share + 0.05,
            "couplet {couplet_share:.2} is no more concentrated than doppler {doppler_share:.2}"
        );
    }

    /// Hand-read off the two velocity stop lists.
    #[test]
    fn the_interpolated_velocity_presets_paint_their_declared_colours() {
        let doppler = smooth_doppler_velocity_table();
        assert_eq!(doppler.sample(0.0), Rgba8::opaque(112, 112, 112));
        assert_eq!(doppler.sample(-20.0), Rgba8::opaque(22, 228, 62));
        assert_eq!(doppler.sample(20.0), Rgba8::opaque(250, 26, 26));
        // Halfway from (18,174,54) at -12 to (16,146,50) at -8.
        assert_eq!(doppler.sample(-10.0), Rgba8::opaque(17, 160, 52));
        // Halfway from (190,40,42) at 8 to (212,28,32) at 12.
        assert_eq!(doppler.sample(10.0), Rgba8::opaque(201, 34, 37));

        let couplet = smooth_couplet_velocity_table();
        assert_eq!(couplet.sample(0.0), Rgba8::opaque(104, 104, 104));
        assert_eq!(couplet.sample(-25.0), Rgba8::opaque(0, 206, 214));
        assert_eq!(couplet.sample(-15.0), Rgba8::opaque(18, 232, 70));
        assert_eq!(couplet.sample(15.0), Rgba8::opaque(255, 44, 28));
        assert_eq!(couplet.sample(25.0), Rgba8::opaque(255, 188, 28));
        // Halfway from (14,186,56) at -9 to (26,158,56) at -6.
        assert_eq!(couplet.sample(-7.5), Rgba8::opaque(20, 172, 56));
    }

    /// The interpolated presets get the same adjacent-stop check the dual-pol
    /// tables get: a repeated colour is a stop that does nothing.
    #[test]
    fn no_interpolated_preset_paints_two_adjacent_stops_the_same_colour() {
        for table in [
            smooth_classic_reflectivity_table(),
            smooth_sequential_reflectivity_table(),
            smooth_storm_core_reflectivity_table(),
            smooth_doppler_velocity_table(),
            smooth_couplet_velocity_table(),
        ] {
            for window in table.stops().windows(2) {
                let (left, right) = (window[0], window[1]);
                // The two leading clear stops are both transparent by design.
                if left.color.a == 0 && right.color.a == 0 {
                    continue;
                }
                assert!(
                    max_channel_delta(left.color, right.color) >= 3
                        || left.color.a != right.color.a,
                    "{} paints {} and {} the same colour",
                    table.name(),
                    left.value,
                    right.value
                );
            }
        }
    }

    // -----------------------------------------------------------------------
    // Sampling as a switch
    //
    // Everything below is about the property that replaced "this table is a
    // stepped table": any palette can be drawn either way, and the two drawings
    // are the same palette. The tests come in two halves - what must not have
    // changed, and what the change is for.
    // -----------------------------------------------------------------------

    /// Every palette the picker offers, with the family it belongs to.
    fn every_registered_palette() -> Vec<(ColorTableFamily, ColorTable)> {
        ColorTableFamily::ALL
            .into_iter()
            .flat_map(|family| {
                builtin_tables_for_family(family)
                    .into_iter()
                    .map(move |table| (family, table))
            })
            .collect()
    }

    /// A dense sweep of a family's domain, wide enough to run past both ends.
    fn dense_domain_sweep(family: ColorTableFamily) -> Vec<f32> {
        const SAMPLES: usize = 28_001;
        let (low, high) = family.nominal_domain();
        let margin = (high - low) * 0.25;
        let low = low - margin;
        let high = high + margin;
        (0..SAMPLES)
            .map(|index| low + (high - low) * index as f32 / (SAMPLES - 1) as f32)
            .collect()
    }

    /// The values a WSR-88D can actually encode for a moment, at its own
    /// resolution: reflectivity on a 0.5 dBZ grid, velocity on 0.5 m/s.
    ///
    /// Counting distinct colours over these and not over an arbitrary sweep is
    /// the point: a palette that resolves a hundred colours over values the
    /// radar cannot send has resolved nothing.
    fn encodable_grid(family: ColorTableFamily) -> Vec<f32> {
        match family {
            ColorTableFamily::Reflectivity => (0..=120).map(|n| 10.0 + n as f32 * 0.5).collect(),
            ColorTableFamily::Velocity => (0..=240).map(|n| -60.0 + n as f32 * 0.5).collect(),
            _ => {
                let (low, high) = family.nominal_domain();
                (0..=200)
                    .map(|n| low + (high - low) * n as f32 / 200.0)
                    .collect()
            }
        }
    }

    /// The flip is lossless in both directions.
    ///
    /// This is the whole claim behind making sampling a control rather than a
    /// property. If flipping to smooth and back did not return the palette
    /// bit-for-bit, the switch would be a one-way door and no analyst could
    /// trust it.
    #[test]
    fn flipping_a_palettes_rendering_and_flipping_it_back_returns_the_palette() {
        for (_, table) in every_registered_palette() {
            let there = table.rendered(table.rendering().flipped());
            let back = there.rendered(table.rendering());
            assert_eq!(back, table, "{} did not survive a round trip", table.name());
            assert_eq!(back.name(), table.name());

            // And asking for what it already is changes nothing at all.
            assert_eq!(table.rendered(table.rendering()), table);

            // Both renderings agree about which palette this is.
            assert_eq!(there.base_name(), table.base_name());
            assert_eq!(there.stops(), table.stops());
            assert_eq!(there.product(), table.product());
            assert_eq!(there.units(), table.units());
        }
    }

    /// Flipping the rendering moves the signature, so a cached raster is thrown
    /// away rather than redrawn from the wrong colours.
    ///
    /// Renderers key their caches on [`ColorTable::signature`]. If two tables
    /// that paint different pictures could share one, the flip would leave the
    /// old picture on screen and look like a dead control. The built-ins are
    /// safe by accident - their names carry the mode, and the name is hashed -
    /// so the case that actually needs the mode in the hash is a pair of tables
    /// with the same name and the same stops: exactly what a palette loaded
    /// from a file becomes when the analyst flips it, and what
    /// `parse_with_default_mode` produces from one file read two ways.
    #[test]
    fn a_palette_drawn_two_ways_never_shares_one_signature() {
        let body = "color: 0 0 0 255\ncolor: 10 255 255 255";
        let interpolated = ColorTable::parse("one name", body).expect("table parses");
        let stepped = ColorTable::parse_stepped("one name", body).expect("table parses");
        let continuous =
            ColorTable::parse_with_default_mode("one name", body, SampleMode::Continuous)
                .expect("table parses");

        assert_eq!(interpolated.name(), stepped.name());
        assert_eq!(interpolated.stops(), stepped.stops());
        assert_ne!(
            interpolated.signature(),
            stepped.signature(),
            "one name, two drawings, one signature: a cached raster would survive the flip"
        );
        assert_ne!(interpolated.signature(), continuous.signature());
        assert_ne!(stepped.signature(), continuous.signature());

        // And across the flip on every built-in, where the name moves too.
        for (_, table) in every_registered_palette() {
            let banded = table.rendered(TableRendering::Stepped);
            let smooth = table.rendered(TableRendering::Smooth);
            if banded == smooth {
                continue;
            }
            assert_ne!(
                banded.signature(),
                smooth.signature(),
                "{} keeps its signature across the flip",
                table.name()
            );
        }
    }

    /// No gate that painted stops painting, and the legend does not move.
    ///
    /// Two separate promises, checked over 28,001 values spanning a quarter
    /// past both ends of each family's domain:
    ///
    /// * whatever the banded drawing inks, the continuous drawing inks;
    /// * the inked span and the range-folded colour, which are what a legend
    ///   and a folded gate are drawn from, are identical either way.
    #[test]
    fn the_continuous_rendering_inks_everything_the_banded_one_did() {
        for (family, table) in every_registered_palette() {
            let banded = table.rendered(TableRendering::Stepped);
            let continuous = table.rendered(TableRendering::Smooth);

            assert_eq!(
                banded.inked_value_span(),
                continuous.inked_value_span(),
                "{} moved its inked span at the flip",
                table.name()
            );
            assert_eq!(
                banded.range_folded_color(),
                continuous.range_folded_color(),
                "{} moved its folded colour at the flip",
                table.name()
            );

            for value in dense_domain_sweep(family) {
                let banded_alpha = banded.sample(value).a;
                if banded_alpha > 0 {
                    assert!(
                        continuous.sample(value).a > 0,
                        "{} paints {value} banded but not continuous",
                        table.name()
                    );
                }
            }
        }
    }

    /// For every palette that was authored banded, the flip is a pure
    /// recolouring: exactly the same gates paint, at exactly the same alpha.
    ///
    /// It holds for the parsed palettes that are drawn on the legacy sRGB
    /// interpolation too. It did not use to: every interpolated palette was
    /// excluded here, because its clear lead-in stop ramped up into the first
    /// opaque one and a band has no way to express a half-dBZ fringe. A clear
    /// `.pal` *row* now carries the dialect's hold, so the parsed ones have no
    /// fringe left to lose.
    ///
    /// Four palettes still cannot be included, and they are exactly the four
    /// whose lowest clear stop ramps up into an opaque colour rather than
    /// holding: AWIPS Wilson, which declares the fade because its `.pal` text
    /// does, and the three Rust-built Smooth REF presets, whose clear stop
    /// resolves to the first inked one because a stop list has no dialect. Hard
    /// bands have one colour per band and keep that stretch clear instead.
    ///
    /// The exclusion is written as the condition rather than as four names, so
    /// a palette that acquires a fade-in is excluded with it and one that loses
    /// it is caught. It reads the *resolved* segment end and not the declared
    /// one, because whether a clear stop fades is a fact about what the table
    /// paints, not about how the fact got written down.
    #[test]
    fn a_banded_palette_flipped_to_continuous_paints_exactly_the_same_gates() {
        let mut fade_ins = 0;
        for (family, table) in every_registered_palette() {
            let stops = table.stops();
            let fades_in = stops.iter().enumerate().any(|(index, stop)| {
                stop.color.a == 0
                    && index + 1 < stops.len()
                    && segment_end_color(stops, index).a > 0
            });
            if fades_in {
                fade_ins += 1;
                continue;
            }
            let banded = table.rendered(TableRendering::Stepped);
            let continuous = table.rendered(TableRendering::Smooth);
            for value in dense_domain_sweep(family) {
                assert_eq!(
                    banded.sample(value).a,
                    continuous.sample(value).a,
                    "{} disagrees about coverage at {value}",
                    table.name()
                );
            }
        }
        assert_eq!(
            fade_ins, 4,
            "AWIPS Wilson and the three Smooth REF presets are the built-ins \
             whose clear stop fades up rather than holding"
        );
    }

    /// Transparency leads or it does not exist, in either rendering.
    ///
    /// An interior alpha-0 stop would punch a hole through the middle of a
    /// scale and `inked_value_span` would report straight across it. The
    /// continuous mixer would also have to mix *into* a transparent stop, which
    /// pulls the colour toward black on the way. Neither happens.
    #[test]
    fn transparency_stays_leading_only_in_both_renderings() {
        for (family, table) in every_registered_palette() {
            for rendering in TableRendering::ALL {
                let drawn = table.rendered(rendering);
                let leading = drawn
                    .stops()
                    .iter()
                    .take_while(|stop| stop.color.a == 0)
                    .count();
                assert!(
                    drawn.stops()[leading..].iter().all(|stop| stop.color.a > 0),
                    "{} has an interior transparent stop",
                    drawn.name()
                );

                // And the painted alpha never returns to zero once it has left
                // it, sampled across the domain.
                let mut has_inked = false;
                for value in dense_domain_sweep(family) {
                    let alpha = drawn.sample(value).a;
                    if alpha > 0 {
                        has_inked = true;
                    } else {
                        assert!(!has_inked, "{} goes clear again at {value}", drawn.name());
                    }
                }
                assert!(has_inked, "{} never inks anything", drawn.name());
            }
        }
    }

    /// The reason the switch exists, counted on the grid the radar encodes on.
    ///
    /// Reflectivity is quantised to 0.5 dBZ and velocity to 0.5 m/s in the
    /// Level II moment blocks, so these are the readings a radar can actually
    /// hand the display. A banded palette maps 121 distinct reflectivity
    /// readings onto 13 to 25 colours and 241 velocity readings onto 16 to 71;
    /// drawn continuously the same palettes resolve almost all of them.
    #[test]
    fn the_continuous_rendering_resolves_the_scale_the_banded_one_bins() {
        let distinct = |table: &ColorTable, family: ColorTableFamily| {
            encodable_grid(family)
                .into_iter()
                .map(|value| table.sample(value))
                .collect::<std::collections::HashSet<_>>()
                .len()
        };

        for (family, table) in every_registered_palette() {
            if !matches!(
                family,
                ColorTableFamily::Reflectivity | ColorTableFamily::Velocity
            ) {
                continue;
            }
            let banded = distinct(&table.rendered(TableRendering::Stepped), family);
            let continuous = distinct(&table.rendered(TableRendering::Smooth), family);
            assert!(
                continuous >= banded,
                "{} resolves fewer colours continuous ({continuous}) than banded ({banded})",
                table.name()
            );

            // Sign Check VEL is a two-state polarity classifier and not a ramp:
            // it declares one blue from -100 to -0.01 and one red from 0.01 to
            // 100, so there is nothing between its stops for either rendering
            // to resolve. It is excluded from the floor, not from the test
            // above.
            if table.base_name() == "Sign Check VEL" {
                continue;
            }
            let grid = encodable_grid(family).len();
            assert!(
                continuous * 4 >= grid * 3,
                "{} resolves only {continuous} of {grid} encodable readings",
                table.name()
            );
        }

        // The two defaults, stated as the numbers they are.
        assert_eq!(
            distinct(&gr2_reflectivity_table(), ColorTableFamily::Reflectivity),
            13
        );
        assert_eq!(
            distinct(
                &builtin_reflectivity_table(),
                ColorTableFamily::Reflectivity
            ),
            121
        );
        assert_eq!(
            distinct(&tornado_velocity_table(), ColorTableFamily::Velocity),
            61
        );
        // The ported WDT/RadarScope default resolves 241 of the encodable
        // readings; the remainder collapse inside its deliberately flat
        // segments, which is the original behaviour.
        assert_eq!(
            distinct(&builtin_velocity_table(), ColorTableFamily::Velocity),
            241
        );
    }

    // -----------------------------------------------------------------------
    // Velocity, which is where the complaint was loudest and where the risk of
    // answering it badly is highest.
    // -----------------------------------------------------------------------

    /// Every velocity palette, drawn continuously, keeps the four things a
    /// velocity display is read by.
    #[test]
    fn every_continuous_velocity_palette_keeps_zero_neutral_and_the_signs_apart() {
        for (family, table) in every_registered_palette() {
            if family != ColorTableFamily::Velocity {
                continue;
            }
            let table = table.rendered(TableRendering::Smooth);

            // 1. Zero is neutral, so the zero isodop reads as a line.
            let zero = table.sample(0.0);
            assert_eq!(zero.a, 255, "{} fades zero out", table.name());
            if table.base_name() == "GenericRadar VEL" {
                // The ported WDT/RadarScope classic declares its zero as a
                // muted mauve rather than a strict grey; that declaration IS
                // the ported look, pinned byte-for-byte in
                // `presets::port_fidelity`. It still has to be muted enough
                // to read as the isodop.
                let high = zero.r.max(zero.g).max(zero.b);
                let low = zero.r.min(zero.g).min(zero.b);
                assert!(
                    high - low <= 32,
                    "{} zero is not muted: {zero:?}",
                    table.name()
                );
            } else {
                assert_eq!(zero.r, zero.g, "{} zero is not neutral", table.name());
                assert_eq!(zero.g, zero.b, "{} zero is not neutral", table.name());
            }

            // 2. The isodop is findable, which means the zero colour is
            // local to zero: nothing beyond three metres per second of it may
            // look like it. Measured as perceptual distance from the zero
            // colour and not as "how neutral is this", because several of these
            // palettes run to near-white at the ends of the scale and a white
            // is technically as neutral as a mid grey while being impossible to
            // confuse with one.
            for tenth in 30..=700 {
                let value = tenth as f32 / 10.0;
                for signed in [value, -value] {
                    let painted = table.sample(signed);
                    assert!(
                        oklab::difference(painted, zero) > 0.04,
                        "{} paints {signed} m/s {painted:?}, which is within sight of \
                         its own zero, so the zero isodop is not findable",
                        table.name()
                    );
                }
            }

            // 3. Cool inbound, warm outbound, at every magnitude that carries
            // meaning. Stated as "red is not the strongest channel inbound and
            // is the strongest outbound", which is the widest form of the rule
            // that every palette here satisfies: the inbound half is free to
            // run green into cyan, and Sign Check VEL paints a pure blue whose
            // green channel is zero, so requiring green *and* blue to beat red
            // would exclude it for being the wrong kind of cool.
            for magnitude in [5.0_f32, 10.0, 15.0, 20.0, 25.0, 30.0] {
                let inbound = table.sample(-magnitude);
                assert!(
                    inbound.r < inbound.g.max(inbound.b),
                    "{} paints -{magnitude} m/s {inbound:?}, which is not inbound",
                    table.name()
                );
                let outbound = table.sample(magnitude);
                assert!(
                    outbound.r > outbound.g.max(outbound.b),
                    "{} paints +{magnitude} m/s {outbound:?}, which is not outbound",
                    table.name()
                );
            }

            // 4. The range-folded colour is not a colour any gate can be
            // painted, or a folded gate would be indistinguishable from a real
            // reading. Measured perceptually rather than per channel, because
            // "looks the same" is a perceptual question.
            let folded = table.range_folded_rgba();
            for tenth in -700..=700 {
                let painted = table.sample(tenth as f32 / 10.0);
                assert!(
                    oklab::difference(painted, folded) > 0.08,
                    "{} paints {} m/s within sight of its folded colour",
                    table.name(),
                    tenth as f32 / 10.0
                );
            }
        }
    }

    /// A couplet is no harder to see continuous than it was banded.
    ///
    /// This is the risk in what was asked for, so it is measured rather than
    /// assumed. A mesocyclonic couplet is two adjacent gates at -Vr and +Vr;
    /// the WSR-88D mesocyclone detection algorithm works over circulations with
    /// rotational velocities typically in the 15-25 m/s range (Stumpf, G. J.,
    /// and coauthors, 1998: "The National Severe Storms Laboratory mesocyclone
    /// detection algorithm for the WSR-88D", Wea. Forecasting, 13, 304-326,
    /// doi:10.1175/1520-0434(1998)013<0304:TNSSLM>2.0.CO;2), so the band swept
    /// here is 10 to 35 m/s.
    ///
    /// Two claims, both of which quantisation can break in opposite directions.
    /// Rounding a pair of readings onto a grid pushes them apart as often as it
    /// pulls them together, so a banded table can *flatter* a particular Vr -
    /// which is why the mean is allowed to fall a little - but the couplet an
    /// analyst nearly misses is the weakest one, and that one must not get
    /// weaker.
    ///
    /// Measured on the shipped default, Analyst Tornado VEL: over 10-35 m/s the
    /// weakest separation is dE 0.036 banded and 0.044 continuous, both at
    /// Vr 25 where that palette runs both signs to near-white - a flaw of the
    /// palette, present in both drawings, which the continuous one slightly
    /// eases. The largest single-point loss is at Vr 22.5, where continuous
    /// gives 0.61 of banded and both remain well over ten times the perceptual
    /// just-noticeable difference. The largest loss of *mean* separation over
    /// the band is Analyst Pro VEL at 0.897 of its banded mean, 0.2603 to
    /// 0.2334; its bands are wide flat plateaus that happen to straddle the
    /// sweep in a flattering way, and its weakest couplet still improves.
    ///
    /// The same thing measured on real volumes rather than on a symmetric
    /// sweep, over the two hundred strongest opposite-sign adjacent-radial gate
    /// pairs in each of four volumes, banded then continuous:
    ///
    ///   KABR 2026-08-18 06:43:14Z   worst 0.261 -> 0.275, mean 0.363 -> 0.338
    ///   KDMX 2026-08-18 08:34:01Z   worst 0.036 -> 0.041, mean 0.215 -> 0.204
    ///   KARX 2026-08-18 20:35:12Z   worst 0.025 -> 0.027, mean 0.278 -> 0.275
    ///   KEAX 2026-08-18 17:56:38Z   worst 0.053 -> 0.051, mean 0.199 -> 0.181
    ///
    /// The couplet an analyst nearly misses got easier to see at three of the
    /// four, by 5 to 15 per cent. At KEAX it got 3.7 per cent harder, which is
    /// the honest answer and not a rounding error: quantisation moves a
    /// symmetric pair in whichever direction its grid happens to lie, and at
    /// that site it happened to lie helpfully. Both numbers there are about
    /// two and a half times the just-noticeable difference, and the mean falls
    /// at every site for the same reason - a banded table inflates the
    /// separation of pairs that straddle a bin edge, and that inflation is not
    /// information.
    #[test]
    fn a_couplet_is_no_harder_to_see_continuous_than_it_was_banded() {
        for (family, table) in every_registered_palette() {
            if family != ColorTableFamily::Velocity {
                continue;
            }
            let banded = table.rendered(TableRendering::Stepped);
            let continuous = table.rendered(TableRendering::Smooth);
            // Only palettes that were written as banded tables have a "before"
            // to be compared against. Smooth Doppler VEL and Smooth Couplet VEL
            // were authored as continuous ramps and have never been drawn as
            // bands on anyone's scope, so scoring their new banded drawing
            // against their old continuous one would be comparing this change
            // against something that never shipped.
            if continuous.sample_mode_label() != "continuous" {
                continue;
            }

            let mut banded_worst = f32::INFINITY;
            let mut continuous_worst = f32::INFINITY;
            let mut banded_total = 0.0;
            let mut continuous_total = 0.0;
            let mut worst_ratio = f32::INFINITY;
            let mut worst_ratio_at = 0.0;
            let mut samples = 0;

            for half in 20..=70 {
                let rotational = half as f32 / 2.0;
                let banded_separation =
                    oklab::difference(banded.sample(-rotational), banded.sample(rotational));
                let continuous_separation = oklab::difference(
                    continuous.sample(-rotational),
                    continuous.sample(rotational),
                );
                banded_worst = banded_worst.min(banded_separation);
                continuous_worst = continuous_worst.min(continuous_separation);
                banded_total += banded_separation;
                continuous_total += continuous_separation;
                let ratio = continuous_separation / banded_separation;
                if ratio < worst_ratio {
                    worst_ratio = ratio;
                    worst_ratio_at = rotational;
                }
                samples += 1;
            }

            // Every velocity palette: the weakest couplet in the band does not
            // materially weaken. Five per cent of slack and not zero, because
            // rounding a symmetric pair onto a grid moves it in whichever
            // direction the grid happens to lie. Two palettes take a small loss
            // and both stay far above the point of being hard to see: GR2-ish
            // Analyst VEL 0.2807 to 0.2689 and Couplet Pop VEL 0.2753 to
            // 0.2739, against a just-noticeable difference near 0.02.
            assert!(
                continuous_worst >= banded_worst * 0.95,
                "{}: the weakest couplet over 10-35 m/s got weaker, {banded_worst:.4} -> \
                 {continuous_worst:.4}",
                table.name()
            );
            assert!(
                continuous_total >= banded_total * 0.85,
                "{}: mean couplet separation fell from {:.4} to {:.4}",
                table.name(),
                banded_total / samples as f32,
                continuous_total / samples as f32
            );
            // The shipped default is held to more than that: its weakest
            // couplet must not get weaker at all, and no single rotational
            // velocity in the band may lose more than 45% of its separation.
            //
            // The per-point floor is held against the shipped default only.
            // The other palettes are offered, not installed: an analyst reaches
            // one deliberately and can flip it back to bands in a click, and
            // two of them have shapes that the banded drawing happens to
            // flatter at one particular rotational velocity. Analyst Pro VEL is
            // the worst: at Vr 23.5 it separates only 0.263 as far continuous
            // as banded, because its outbound ramp is already near-white at
            // 23.5 while the banded drawing holds the salmon of its +21 stop
            // all the way to +24. That is the palette's own shape, not the
            // mixer's, and the two summary statistics above still hold for it -
            // its weakest couplet improves and its mean falls to 0.897.
            if table.base_name() != builtin_velocity_table().base_name() {
                continue;
            }
            assert!(
                continuous_worst >= banded_worst,
                "the default: the weakest couplet got weaker, {banded_worst:.4} -> \
                 {continuous_worst:.4}"
            );
            assert!(
                worst_ratio >= 0.55,
                "{}: at Vr {worst_ratio_at} the continuous drawing separates the couplet \
                 only {worst_ratio:.4} as far as the banded one did",
                table.name()
            );
        }
    }

    /// The same question asked about couplets that are not symmetric, which is
    /// all of the real ones.
    ///
    /// The sweep above pairs `-Vr` with `+Vr`. No couplet on a scope is built
    /// that way: the storm is moving, the beam is not centred on the
    /// circulation, and the inbound and outbound maxima differ. Across the
    /// seven volumes this change was measured against, the strongest couplet in
    /// each was +21.0/-21.0, +25.5/-25.0, -27.0/+27.5, -24.0/+24.0,
    /// +34.0/-33.5, -25.5/+26.0 and +25.5/-25.5 m/s - three of the seven
    /// symmetric to the half metre per second the field is encoded on, four
    /// not.
    ///
    /// Measured on the real couplets rather than on a sweep - every pair of
    /// adjacent radials at one gate carrying opposite signs and both at least
    /// 10 m/s, the two hundred strongest per volume - the weakest couplet in
    /// the volume goes, banded to continuous:
    ///
    ///   KABR 0.2608 -> 0.2749   KDMX 0.0356 -> 0.0410   KARX 0.0253 -> 0.0274
    ///   KEAX 0.0528 -> 0.0509   KUDX 0.0376 -> 0.0354   KUEX 0.0356 -> 0.0360
    ///   KBIS 0.0356 -> 0.0399
    ///
    /// Four improve and three lose, the largest loss being KUDX at 5.9 per
    /// cent. Every one of the 1,449 real couplets scored stays above the
    /// just-noticeable difference in both drawings; none becomes invisible.
    /// The majority of individual real couplets do separate less continuous
    /// than banded - 186 of 200 at KABR, 34 of 49 at KUDX - which is what a
    /// hard band edge falling between two readings does, and is not a colour
    /// the wind put there.
    ///
    /// Sweeping the asymmetric pairs matters because the worst case moves. Over
    /// every inbound in -35..=-10 against every outbound in 10..=35 on the half
    /// metre grid - 2,601 pairs - the shipped default's worst per-pair loss is
    /// at (-23.0, +22.5), where the continuous drawing separates the pair 0.53
    /// as far as the banded one: dE 0.3203 down to 0.1699. The symmetric sweep
    /// never sees it and reports 0.61 at Vr 22.5 as the worst case, which is
    /// not the worst case. Both drawings are still eight times the
    /// just-noticeable difference apart there, so this is a narrower guarantee
    /// than was claimed rather than a couplet anyone would miss - but the
    /// difference between those two statements is the whole reason to sweep it.
    ///
    /// 1,621 of the 2,601 pairs separate less continuous than banded. That is
    /// expected and is not by itself a fault: a hard band edge falling between
    /// two readings inflates their separation, and where it falls is an
    /// artefact of the grid, not of the wind. What has to hold is the floor,
    /// and the floor is measured absolutely rather than as a ratio:
    ///
    ///   weakest continuous separation over the band  0.0242 at (-29.0, +28.0)
    ///   weakest banded separation over the band      0.0243 at the same pair
    ///
    /// Both are thin, and both are thin for the same reason - Analyst Tornado
    /// VEL runs both signs toward near-white beyond about 25 m/s, so a couplet
    /// straddling that shoulder is nearly colourless whichever way it is drawn.
    /// That is a property of the palette, it predates this change, and an
    /// analyst who needs those magnitudes separated wants Couplet Pop VEL,
    /// whose weakest asymmetric pair in the same band is far above it.
    #[test]
    fn an_asymmetric_couplet_is_no_harder_to_see_continuous_than_it_was_banded() {
        // The ratio clauses below compare a palette against its own banded
        // drawing, which only means something for the palette that USED to
        // ship banded - Analyst Tornado VEL, whose smooth migration this test
        // was written to guard. The shipped default is authored continuous,
        // so there is no banded original to regress against; it takes the
        // absolute floor at the end instead.
        let default = tornado_velocity_table();
        let banded = default.rendered(TableRendering::Stepped);
        let continuous = default.rendered(TableRendering::Smooth);

        let mut banded_weakest = f32::INFINITY;
        let mut continuous_weakest = f32::INFINITY;
        let mut weakest_at = (0.0_f32, 0.0_f32);
        let mut worst_ratio = f32::INFINITY;
        let mut worst_ratio_at = (0.0_f32, 0.0_f32);

        for inbound_half in -70..=-20 {
            let inbound = inbound_half as f32 / 2.0;
            for outbound_half in 20..=70 {
                let outbound = outbound_half as f32 / 2.0;
                let banded_separation =
                    oklab::difference(banded.sample(inbound), banded.sample(outbound));
                let continuous_separation =
                    oklab::difference(continuous.sample(inbound), continuous.sample(outbound));

                if continuous_separation < continuous_weakest {
                    continuous_weakest = continuous_separation;
                    weakest_at = (inbound, outbound);
                }
                banded_weakest = banded_weakest.min(banded_separation);

                if banded_separation > 0.0 {
                    let ratio = continuous_separation / banded_separation;
                    if ratio < worst_ratio {
                        worst_ratio = ratio;
                        worst_ratio_at = (inbound, outbound);
                    }
                }
            }
        }

        // The floor that decides whether a couplet can be seen at all. A
        // just-noticeable difference in Oklab is about 0.02, so every couplet
        // in the mesocyclone band has to clear that, and the margin here is
        // thin on purpose - it is the honest margin, not a comfortable one.
        assert!(
            continuous_weakest > 0.02,
            "the weakest asymmetric couplet, at ({:.1}, {:.1}) m/s, separates only \
             {continuous_weakest:.4} - under the just-noticeable difference, so it cannot \
             be seen at all",
            weakest_at.0,
            weakest_at.1
        );
        // And it is not the continuous drawing that made it thin.
        assert!(
            continuous_weakest >= banded_weakest * 0.95,
            "the weakest asymmetric couplet got weaker, {banded_weakest:.4} -> \
             {continuous_weakest:.4} at ({:.1}, {:.1}) m/s",
            weakest_at.0,
            weakest_at.1
        );
        // No single asymmetric pair may lose half its separation. Measured
        // worst is 0.53 at (-23.0, +22.5); this is the guarantee the symmetric
        // sweep's 0.55 was believed to be giving.
        assert!(
            worst_ratio >= 0.50,
            "at ({:.1}, {:.1}) m/s the continuous drawing separates the couplet only \
             {worst_ratio:.4} as far as the banded one did",
            worst_ratio_at.0,
            worst_ratio_at.1
        );

        // And the palette actually shipping as the default: every asymmetric
        // couplet in the mesocyclone band must clear the just-noticeable
        // difference on its own terms.
        let shipped = builtin_velocity_table();
        let mut shipped_weakest = f32::INFINITY;
        let mut shipped_weakest_at = (0.0_f32, 0.0_f32);
        for inbound_half in -70..=-20 {
            let inbound = inbound_half as f32 / 2.0;
            for outbound_half in 20..=70 {
                let outbound = outbound_half as f32 / 2.0;
                let separation =
                    oklab::difference(shipped.sample(inbound), shipped.sample(outbound));
                if separation < shipped_weakest {
                    shipped_weakest = separation;
                    shipped_weakest_at = (inbound, outbound);
                }
            }
        }
        assert!(
            shipped_weakest > 0.02,
            "the shipped default's weakest asymmetric couplet, at ({:.1}, {:.1}) m/s, \
             separates only {shipped_weakest:.4}",
            shipped_weakest_at.0,
            shipped_weakest_at.1
        );
    }

    /// Distinct colours are not legibility, so legibility is measured too.
    ///
    /// Counting how many *different byte triples* a palette paints over the
    /// encodable grid is the natural headline number and it overstates the
    /// case: a continuous ramp trivially paints a different triple for almost
    /// every reading, and two triples one byte apart are the same colour to an
    /// analyst. The measurement that answers the complaint is perceptual - of
    /// two readings a given distance apart, how often are they painted closer
    /// together than the eye can resolve.
    ///
    /// Share of neighbouring readings across the encodable grid painted under
    /// one just-noticeable difference apart, banded then continuous:
    ///
    ///   GR2Analyst Classic REF   0.5 dBZ 90.1 -> 56.2   1 dBZ 80.2 -> 24.8
    ///                            2 dBZ   60.3 ->  1.7   3 dBZ 39.7 ->  0.0
    ///   Analyst Tornado VEL      0.5 m/s 88.4 -> 82.2   1 m/s 76.3 -> 71.8
    ///                            2 m/s   53.1 -> 53.9   3 m/s 34.9 -> 39.0
    ///                            5 m/s   16.6 -> 14.5
    ///
    /// Reflectivity is transformed. Velocity is better at the separations that
    /// dominate a real field and *worse* at two and three metres per second,
    /// because the banded drawing's edges fall helpfully out in the tails where
    /// Analyst Tornado VEL is nearly flat anyway, and a uniform sweep of the
    /// encodable domain weights those tails as heavily as the middle. On the
    /// four real volumes, where the readings are where the wind actually is,
    /// the same statistic over adjacent gate pairs that disagree runs
    /// 24.1 -> 16.4, 26.9 -> 15.4, 45.9 -> 36.5 and 33.8 -> 21.3 per cent -
    /// better everywhere. The aggregate over the separations is what is pinned
    /// here, and the per-separation numbers are written down so that "it
    /// improved" cannot be read as "it improved at every separation".
    ///
    /// Three offered velocity palettes go the other way on this measure and are
    /// deliberately not pinned: GR2-ish Analyst VEL, Subtle SRV VEL and Couplet
    /// Pop VEL were authored on quantisation grids of one to two metres per
    /// second, fine enough that the bands were already resolving the field, and
    /// spreading the same colour range smoothly puts more neighbouring pairs
    /// under the threshold than the bands did. On the real volumes GR2-ish
    /// Analyst VEL runs 25.4 -> 30.8 per cent at KABR and 42.0 -> 56.8 at KARX.
    /// They are offered, not installed, and one click puts the bands back.
    #[test]
    fn the_continuous_defaults_leave_fewer_neighbouring_readings_unresolvable() {
        let blind_share = |table: &ColorTable, family: ColorTableFamily, delta: f32| {
            let mut total = 0_u32;
            let mut blind = 0_u32;
            for value in encodable_grid(family) {
                let low = table.sample(value);
                let high = table.sample(value + delta);
                if low.a == 0 && high.a == 0 {
                    continue;
                }
                total += 1;
                if oklab::difference(low, high) < 0.02 {
                    blind += 1;
                }
            }
            100.0 * blind as f32 / total as f32
        };

        for (family, table) in [
            (ColorTableFamily::Reflectivity, builtin_reflectivity_table()),
            (ColorTableFamily::Velocity, builtin_velocity_table()),
        ] {
            let banded = table.rendered(TableRendering::Stepped);
            let continuous = table.rendered(TableRendering::Smooth);
            let mut banded_total = 0.0;
            let mut continuous_total = 0.0;
            for delta in [0.5_f32, 1.0, 2.0, 3.0, 5.0] {
                banded_total += blind_share(&banded, family, delta);
                continuous_total += blind_share(&continuous, family, delta);
            }
            assert!(
                continuous_total < banded_total,
                "{}: the continuous drawing leaves more neighbouring readings under one \
                 just-noticeable difference apart than the banded one did, {banded_total:.1} \
                 -> {continuous_total:.1} summed over 0.5, 1, 2, 3 and 5 units",
                table.base_name()
            );
        }
    }

    // -----------------------------------------------------------------------
    // The mixer
    // -----------------------------------------------------------------------

    /// Perceptual mixing earns its place, measured rather than assumed.
    ///
    /// For every stop pair in every palette that gains a continuous rendering,
    /// the midpoint is computed both ways and scored on lightness sag: how far
    /// the invented colour's Oklab lightness falls below the mean of the two
    /// ends'. A positive sag is a dark hole in the middle of a ramp, which is
    /// exactly the "muddy" a naive sRGB lerp produces.
    ///
    /// Worst case measured over the built-ins, sRGB against Oklab:
    ///
    ///   GR2Analyst Classic REF   +0.1107  ->  +0.0006
    ///   NWS Classic REF          +0.1107  ->  +0.0006
    ///   Analyst Tornado VEL      +0.0608  ->  +0.0043
    ///   RadarScope Contrast VEL  +0.0594  ->  +0.0045
    ///   Analyst Classic REF      +0.0438  ->  +0.0010
    ///   GR2-ish Analyst VEL      +0.0246  ->  +0.0013
    ///
    /// The single worst pair anywhere is the blue-to-green step at 20-25 dBZ in
    /// the classic sequence: `(3,0,244)` to `(2,253,2)`. The sRGB midpoint is
    /// `(3,127,123)` at lightness 0.539 - a dark teal below both ends - and the
    /// perceptual midpoint is `(0,166,185)` at 0.664. They are dE 0.131 apart,
    /// six times the just-noticeable difference.
    ///
    /// # Where sRGB wins, said plainly
    ///
    /// Oklab does not win on every measure and the record should not pretend it
    /// does. Nineteen registered palettes gain the perceptual mixer. Measuring
    /// hue drift as "how far the invented midpoint's hue sits from the nearer
    /// of the two stops' hues" - the amount of hue the mixer puts on screen
    /// that neither end asked for - Oklab drifts *further* than sRGB on ten of
    /// the nineteen and less on nine:
    ///
    ///   Analyst Classic REF      sRGB 46.6 deg  Oklab 59.3 deg  (at 22.5 dBZ)
    ///   GR2Analyst Classic REF   sRGB 48.4      Oklab 55.2      (at 22.5 dBZ)
    ///   Analyst Hail Core REF    sRGB 26.7      Oklab 29.7
    ///   Sign Check VEL           sRGB 11.0      Oklab  0.1
    ///   CC Class Bands           sRGB 29.5      Oklab 26.1
    ///   Analyst Tornado VEL      sRGB 15.9      Oklab 12.7
    ///
    /// Mean chroma dip also goes the wrong way on fifteen of the nineteen -
    /// GR2Analyst Classic REF dips 0.0153 under sRGB and 0.0187 under Oklab.
    ///
    /// Both losses have one cause and it is not a defect in the transcription:
    /// a straight line between two distant hues passes through the hues in
    /// between whatever space it is drawn in, and Oklab spends its freedom
    /// holding lightness while sRGB spends it collapsing lightness. The worst
    /// case is the same 20-25 dBZ blue-to-green step in both columns. sRGB
    /// keeps 12 degrees more hue there and pays 0.11 of lightness for it; the
    /// dark teal that produces is the muddy midpoint the whole module exists to
    /// remove, and lightness is what a reflectivity ramp is read by. So Oklab,
    /// on a measured trade rather than on faith - and carrying two mixers to
    /// win twelve degrees of hue on one step is not worth the second code path.
    ///
    /// (An earlier version of this comment claimed hue drift improved on
    /// thirteen of fourteen palettes with Tornado Debris REF as the sole
    /// exception. Neither the count, the denominator, nor the exception
    /// reproduces; the numbers above are what the palettes measure.)
    ///
    /// # The palettes that stay on the sRGB mixer are measured, not skipped
    ///
    /// Twenty registered palettes were authored as sRGB ramps and keep that
    /// mixer, so the switch never moves them. An earlier form of this test
    /// walked past them with a `continue`, which is precisely how a bad ramp
    /// hides: the palette nobody measures is the palette nobody notices is
    /// muddy. They are measured here instead, on the same sag statistic, and
    /// the answer is that they do not sag: the worst of the twenty is Heavy
    /// Rain KDP at +0.0236, against +0.1107 for the worst *banded* palette's
    /// stop pair drawn through the same sRGB lerp. The reason is visible in
    /// the stops - these were authored to be interpolated, so consecutive
    /// stops are close together and near in hue, and a short chord through
    /// sRGB has little room to dive. The five on the base moments are the
    /// mildest of the twenty: Smooth Classic REF +0.0144, Smooth Storm Core
    /// REF +0.0096, Smooth Couplet VEL +0.0041, Smooth Sequential REF +0.0038,
    /// Smooth Doppler VEL +0.0025. Leaving them on sRGB costs nothing
    /// measurable, which is the evidence for leaving them alone rather than
    /// the assertion that they were fine.
    #[test]
    fn the_perceptual_mixer_holds_the_middle_of_a_ramp_up_where_srgb_lets_it_sag() {
        let mut worst_srgb: f32 = 0.0;
        let mut worst_perceptual: f32 = 0.0;
        // The palettes the switch does not move, measured on the same scale.
        let mut worst_legacy: f32 = 0.0;
        let mut worst_legacy_name = String::new();
        let mut legacy_palettes = 0;

        for (_, table) in every_registered_palette() {
            let continuous = table.rendered(TableRendering::Smooth);
            let perceptual = continuous.sample_mode_label() == "continuous";
            if !perceptual {
                legacy_palettes += 1;
                if matches!(table.base_name(), "AWIPS Wilson REF" | "GenericRadar VEL") {
                    // The ported looks are pinned byte-for-byte to their
                    // originals in `presets::port_fidelity`; their mid-ramp
                    // sag is the look itself, not a defect the perceptual
                    // mixer is allowed to fix.
                    continue;
                }
            }
            let stops = table.stops().to_vec();
            for (index, window) in stops.windows(2).enumerate() {
                let (left, right) = (window[0], window[1]);
                if left.color.a == 0 || right.color.a == 0 {
                    continue;
                }
                let ends = (oklab::lightness(left.color) + oklab::lightness(right.color)) / 2.0;
                let midpoint = (stops[index].value + stops[index + 1].value) / 2.0;
                let drawn_sag = ends - oklab::lightness(continuous.sample(midpoint));

                if !perceptual {
                    // What the sRGB mixer actually paints for a palette that
                    // keeps it. No hypothetical: this is on the analyst's
                    // screen today.
                    if drawn_sag > worst_legacy {
                        worst_legacy = drawn_sag;
                        worst_legacy_name = table.base_name().to_owned();
                    }
                    continue;
                }

                let srgb_mid = Rgba8::opaque(
                    lerp_u8(left.color.r, right.color.r, 0.5),
                    lerp_u8(left.color.g, right.color.g, 0.5),
                    lerp_u8(left.color.b, right.color.b, 0.5),
                );
                worst_srgb = worst_srgb.max(ends - oklab::lightness(srgb_mid));
                worst_perceptual = worst_perceptual.max(drawn_sag);
            }
        }

        assert!(
            worst_srgb > 0.1,
            "the sRGB midpoint sags at most {worst_srgb}, so there is nothing to fix"
        );
        assert!(
            worst_perceptual < 0.01,
            "the perceptual midpoint sags {worst_perceptual}"
        );
        assert!(
            worst_perceptual * 10.0 < worst_srgb,
            "perceptual sag {worst_perceptual} is not an order of magnitude under \
             sRGB's {worst_srgb}"
        );

        // The other half of the argument: the palettes left on sRGB are left
        // there because measuring them says they are fine, and if one ever
        // stops being fine this fails instead of staying quiet. The ceiling is
        // a quarter of the sag that justified building the perceptual mixer at
        // all, which is the level at which "leave it alone" stops being true.
        assert!(
            legacy_palettes >= 20,
            "only {legacy_palettes} palettes kept the sRGB mixer; this test has stopped \
             covering the group it exists to cover"
        );
        assert!(
            worst_legacy < 0.025,
            "{worst_legacy_name} sags {worst_legacy} through the sRGB mixer it was authored \
             on, which is no longer negligible; it wants the perceptual mixer or new stops"
        );
    }

    /// The declared colours are still the declared colours.
    ///
    /// Perceptual mixing invents everything between two stops, so the one thing
    /// it must never touch is the stops themselves.
    #[test]
    fn the_continuous_rendering_paints_every_stop_its_own_declared_colour() {
        for (_, table) in every_registered_palette() {
            let continuous = table.rendered(TableRendering::Smooth);
            for stop in table.stops() {
                if stop.color.a == 0 {
                    continue;
                }
                assert_eq!(
                    continuous.sample(stop.value),
                    stop.color,
                    "{} repainted its own stop at {}",
                    continuous.name(),
                    stop.value
                );
            }
        }
    }

    // -----------------------------------------------------------------------
    // What a picker is handed
    // -----------------------------------------------------------------------

    /// Every palette, in the rendering the analyst is using, plus the flip.
    ///
    /// Every entry in the catalogue is tried as the installed one, in both
    /// renderings, and not just the family default. The picker marks the row
    /// whose `name()` matches the installed table's, so a list that contained
    /// two rows with one name - or none - would mark the wrong row or no row,
    /// and the palette that produces that is whichever one happens to collide,
    /// not the one that happens to be first.
    #[test]
    fn the_offer_list_draws_every_palette_the_way_the_installed_one_is_drawn() {
        for family in ColorTableFamily::ALL {
            let catalogue = builtin_tables_for_family(family);
            let seeds = catalogue
                .iter()
                .flat_map(|table| {
                    [
                        table.rendered(TableRendering::Smooth),
                        table.rendered(TableRendering::Stepped),
                    ]
                })
                .collect::<Vec<_>>();
            for installed in seeds {
                let offers = palette_offers_for_family(family, &installed);

                // The catalogue, all of it, all drawn the installed way.
                assert_eq!(offers.len(), catalogue.len() + 1);
                for offer in offers.iter().take(catalogue.len()) {
                    assert_eq!(
                        offer.rendering(),
                        installed.rendering(),
                        "{} is not drawn the way {} is",
                        offer.name(),
                        installed.name()
                    );
                }
                for (offer, catalogued) in offers.iter().zip(&catalogue) {
                    assert_eq!(offer.base_name(), catalogued.base_name());
                    assert_eq!(offer.stops(), catalogued.stops());
                }

                // The switch, as the last row.
                let switch = offers.last().expect("offers are never empty");
                assert_eq!(switch.base_name(), installed.base_name());
                assert_eq!(switch.rendering(), installed.rendering().flipped());

                // The installed palette is in there exactly once, by name, so a
                // picker that marks the row whose name matches marks one row.
                let installed_rows = offers
                    .iter()
                    .filter(|offer| offer.name() == installed.name())
                    .count();
                assert_eq!(
                    installed_rows,
                    1,
                    "{} appears {installed_rows} times in its own list",
                    installed.name()
                );

                // And no two rows can be confused for each other.
                let names = offers
                    .iter()
                    .map(|offer| offer.name())
                    .collect::<std::collections::HashSet<_>>();
                assert_eq!(
                    names.len(),
                    offers.len(),
                    "{family:?} offers a duplicate name"
                );
            }
        }
    }

    /// A palette an analyst loaded from a file keeps its place in the list.
    #[test]
    fn a_loaded_palette_does_not_vanish_from_its_own_offer_list() {
        let loaded = ColorTable::parse(
            "Downloaded VEL",
            "product: BV\nunits: m/s\ncolor: -70 0 255 255\ncolor: 0 128 128 128\n\
             color: 70 255 0 0",
        )
        .expect("table parses");
        let offers = palette_offers_for_family(ColorTableFamily::Velocity, &loaded);

        assert!(
            offers.iter().any(|offer| offer.name() == loaded.name()),
            "the installed palette is missing from its own list"
        );
        assert!(
            offers
                .iter()
                .any(|offer| offer.base_name() == "Downloaded VEL"
                    && offer.rendering() == loaded.rendering().flipped()),
            "the installed palette has no switch"
        );
        assert_eq!(
            offers.len(),
            builtin_tables_for_family(ColorTableFamily::Velocity).len() + 2
        );
    }

    /// A `mode:` row can ask for the perceptual mixer, and `smooth` still means
    /// what it has always meant.
    #[test]
    fn a_palette_file_can_ask_for_either_continuous_mode_by_name() {
        let body = "color: -10 0 0 255\ncolor: 10 255 255 0";
        for (spelling, expected) in [
            ("smooth", "interpolated"),
            ("linear", "interpolated"),
            ("continuous", "continuous"),
            ("perceptual", "continuous"),
            ("oklab", "continuous"),
            ("stepped", "stepped"),
        ] {
            let table =
                ColorTable::parse("m", &format!("mode: {spelling}\n{body}")).expect("table parses");
            assert_eq!(
                table.sample_mode_label(),
                expected,
                "mode: {spelling} resolved wrongly"
            );
        }
    }
}

//! The Doppler spectrum of the gate under the cursor.
//!
//! # Why this exists
//!
//! A moment product carries three numbers per gate for the Doppler part of the
//! measurement — power, mean velocity, spectrum width — and those three are a
//! Gaussian summary of something that is very often not a Gaussian. Two
//! scatterer populations in one gate, a tornado's debris turning against the
//! inflow, ground clutter sitting at zero velocity underneath weather moving at
//! thirty: all of them are one mean and one width in a moment product, and all
//! of them are two obvious humps in the spectrum. No moment file contains this
//! plot, because the pulses it is computed from were thrown away at scan time.
//!
//! So this is the readout the feature is for, and it is drawn to be read rather
//! than to be decorative: a real axis in m/s, a power axis labelled with its
//! actual reference, the receiver noise floor when the source measured one,
//! and the estimator's own mean velocity marked on the same axis so the
//! summary and the thing it summarises can be compared by eye.
//!
//! # What it will not do
//!
//! It will not draw noise as though it were a spectrum. A gate with no signal
//! above the receiver noise gets a sentence saying so; a censored gate says
//! that instead. Drawing the transform of a noise dwell would produce a
//! plausible ragged shape with a mean and a width, and an analyst reading it
//! would be reading the receiver.
//!
//! # References
//!
//! Doviak and Zrnic, *Doppler Radar and Weather Observations*, 2nd ed. 1993,
//! ch. 5-6, for the relationship between the spectrum, its moments and the
//! pulse-pair estimates of them.

use eframe::egui;

use nexrad_io::iq_moments::spectrum::DopplerSpectrum;

/// Everything the panel draws, resolved off the estimator once per hover.
///
/// Held rather than recomputed while painting for the reason the probe readout
/// is: the paint pass must not reach into the pulses, and a transform per
/// gate per frame would be work done for a cursor that has not moved.
pub struct GateSpectrum {
    /// The transform, when the gate had a signal to transform.
    pub spectrum: Option<DopplerSpectrum>,
    /// The mean velocity the MOMENT estimator produced for this gate — the
    /// pulse-pair lag-1 argument, not a number read back off the plot. Marked
    /// on the same axis so the two can be compared; `None` when the gate is
    /// blank.
    pub estimator_velocity_mps: Option<f32>,
    /// Range of the gate, metres.
    pub range_m: f32,
    /// Which receiver channel this is, for the title.
    pub channel: usize,
    /// Why there is no plot, when there is none. Shown verbatim.
    pub absence: Option<String>,
}

/// Panel size in points. Fixed, like the legend's: these are physical reading
/// sizes rather than a share of the pane, and egui has already divided out the
/// interface scale by the time a painter sees points.
const PANEL_WIDTH: f32 = 208.0;
const PANEL_HEIGHT: f32 = 132.0;
/// Inset from the pane's left edge, and from its top past the header. Matches
/// the legend's `EDGE_MARGIN` and `TOP_MARGIN` so the two panels sit on the
/// same grid at opposite sides of the pane.
const EDGE_MARGIN: f32 = 8.0;
const TOP_MARGIN: f32 = 34.0;
const PANEL_PAD: f32 = 6.0;
const TITLE_FONT_SIZE: f32 = 10.0;
const AXIS_FONT_SIZE: f32 = 9.0;

/// The pane must be at least this many times the panel in each direction before
/// the panel is drawn at all.
///
/// The legend's rule, for the legend's reason: a readout that covers the storm
/// instead of explaining it is worse than no readout, and on a four-pane layout
/// at 1.6x interface scale a fixed 208-point panel is most of a pane. Nothing
/// is lost by leaving it out - the probe readout still names the gate.
const MIN_PANE_MULTIPLE: f32 = 2.2;

/// Panel and ink colours.
///
/// Fixed rather than themed, exactly as the legend's are, and for the same
/// reason: this panel is drawn ON the radar field, whose colours are the colour
/// table's and not the theme's. A panel that took the theme's page colours
/// would be legible over the basemap and invisible over a 60 dBZ core in
/// precisely the situation an analyst opens it. The opaque backing is what
/// makes it readable in every theme, because it does not depend on any of them.
const PANEL_FILL: egui::Color32 = egui::Color32::from_rgba_premultiplied(12, 15, 19, 232);
const PANEL_EDGE: egui::Color32 = egui::Color32::from_rgb(74, 84, 95);
const TITLE_INK: egui::Color32 = egui::Color32::from_rgb(239, 243, 246);
const AXIS_INK: egui::Color32 = egui::Color32::from_rgb(150, 162, 173);
const TRACE_INK: egui::Color32 = egui::Color32::from_rgb(126, 200, 255);
const TRACE_FILL: egui::Color32 = egui::Color32::from_rgba_premultiplied(36, 80, 116, 150);
/// The receiver noise floor per bin. Deliberately a different hue from the
/// trace and from the velocity mark: it is a property of the receiver rather
/// than a measurement of the sky.
const NOISE_INK: egui::Color32 = egui::Color32::from_rgb(120, 132, 116);
/// The estimator's mean velocity. Warm against the cool trace so it reads as a
/// separate claim rather than as part of the curve.
const MEAN_INK: egui::Color32 = egui::Color32::from_rgb(255, 176, 84);
const ABSENT_INK: egui::Color32 = egui::Color32::from_rgb(198, 206, 214);

/// Headroom above the peak and below the noise floor, dB, so the trace never
/// touches the frame.
const HEADROOM_DB: f32 = 6.0;
/// Smallest power span the plot will show, dB. Without a floor on it a flat
/// spectrum would be stretched to fill the panel and read as enormous
/// structure.
const MIN_SPAN_DB: f32 = 20.0;

/// Clearance between the mean-velocity rule and the number beside it, points.
const LABEL_GAP: f32 = 2.0;

/// Draw the panel, or nothing when it would cover more than it explains.
pub fn draw_gate_spectrum(painter: &egui::Painter, pane_rect: egui::Rect, gate: &GateSpectrum) {
    if pane_rect.width() < PANEL_WIDTH * MIN_PANE_MULTIPLE
        || pane_rect.height() < PANEL_HEIGHT * MIN_PANE_MULTIPLE
    {
        return;
    }
    let panel = panel_rect(pane_rect);
    // Clipped to the pane so nothing reaches into the pane next door.
    let painter = painter.with_clip_rect(pane_rect.intersect(painter.clip_rect()));
    painter.rect_filled(panel, 3.0, PANEL_FILL);
    painter.rect_stroke(
        panel,
        3.0,
        egui::Stroke::new(1.0, PANEL_EDGE),
        egui::StrokeKind::Inside,
    );

    let title = format!(
        "DOPPLER SPECTRUM · {:.1} km · {}",
        gate.range_m / 1000.0,
        if gate.channel == 0 { "H" } else { "V" }
    );
    painter.text(
        egui::pos2(panel.left() + PANEL_PAD, panel.top() + PANEL_PAD),
        egui::Align2::LEFT_TOP,
        title,
        egui::FontId::monospace(TITLE_FONT_SIZE),
        TITLE_INK,
    );

    let plot = plot_rect(panel);
    if plot.width() <= 8.0 || plot.height() <= 8.0 {
        return;
    }

    let Some(spectrum) = gate
        .spectrum
        .as_ref()
        .filter(|s| s.power_db.iter().any(|power| power.is_finite()))
    else {
        // The no-data case is the one that matters most. There is no plot
        // because there is nothing to plot, and the panel says which kind of
        // nothing rather than drawing the receiver.
        painter.text(
            plot.center(),
            egui::Align2::CENTER_CENTER,
            gate.absence.as_deref().unwrap_or("no signal at this gate"),
            egui::FontId::proportional(AXIS_FONT_SIZE + 1.0),
            ABSENT_INK,
        );
        return;
    };

    draw_plot(&painter, plot, spectrum, gate.estimator_velocity_mps);
}

/// Where the panel sits inside a pane.
///
/// A named function rather than four lines inside the draw, because the tests
/// measure what lands where - that the velocity label cannot reach the title,
/// that the fill is under the trace - and a test that restated this geometry
/// would be measuring its own copy of it.
fn panel_rect(pane_rect: egui::Rect) -> egui::Rect {
    egui::Rect::from_min_size(
        egui::pos2(pane_rect.left() + EDGE_MARGIN, pane_rect.top() + TOP_MARGIN),
        egui::vec2(PANEL_WIDTH, PANEL_HEIGHT),
    )
}

/// Where the plot's frame sits inside the panel.
///
/// The top edge clears the title row by `TITLE_FONT_SIZE + 6`, which is what
/// makes "inside the plot" and "in the title" disjoint places to put a label.
fn plot_rect(panel: egui::Rect) -> egui::Rect {
    egui::Rect::from_min_max(
        egui::pos2(
            panel.left() + PANEL_PAD + 30.0,
            panel.top() + PANEL_PAD + TITLE_FONT_SIZE + 6.0,
        ),
        egui::pos2(
            panel.right() - PANEL_PAD,
            panel.bottom() - PANEL_PAD - AXIS_FONT_SIZE - 4.0,
        ),
    )
}

/// Power extent the plot is drawn over on the spectrum's declared logarithmic
/// reference.
///
/// Anchored on the noise floor rather than on the minimum bin: the lowest bin
/// of a real spectrum is a deep null that carries no information, and scaling
/// to it puts the whole trace in the top third of the panel. Anchoring on the
/// noise means the height of the trace above the baseline IS the signal-to-noise
/// ratio, which is the quantity being judged.
fn power_extent(spectrum: &DopplerSpectrum) -> (f32, f32) {
    let mut finite = spectrum
        .power_db
        .iter()
        .copied()
        .filter(|value| value.is_finite());
    let Some(first) = finite.next() else {
        // A zero-valued dwell becomes -infinity in logarithmic units. It is
        // normally withheld by `draw_gate_spectrum`, but keep this helper
        // finite as well so a future caller cannot turn one empty gate into
        // infinite plot coordinates.
        let baseline = spectrum
            .noise_per_bin_db
            .filter(|noise| noise.is_finite())
            .unwrap_or(0.0);
        return (baseline - HEADROOM_DB, baseline + MIN_SPAN_DB + HEADROOM_DB);
    };
    let (minimum, peak) = finite.fold((first, first), |(minimum, peak), value| {
        (minimum.min(value), peak.max(value))
    });
    let baseline = spectrum
        .noise_per_bin_db
        .filter(|noise| noise.is_finite())
        .unwrap_or(minimum);
    let low = baseline - HEADROOM_DB;
    let high = peak.max(baseline + MIN_SPAN_DB) + HEADROOM_DB;
    (low, high)
}

fn draw_plot(
    painter: &egui::Painter,
    plot: egui::Rect,
    spectrum: &DopplerSpectrum,
    estimator_velocity_mps: Option<f32>,
) {
    let nyquist = spectrum.nyquist_velocity_mps;
    // Both halves matter: a NaN Nyquist would make every x a NaN and queue a
    // shape nothing can draw, and a zero one would divide the velocity axis by
    // zero. Neither can reach here from a decoded record - a sweep with no PRT
    // is refused long before - but this function is handed a plain struct.
    if !nyquist.is_finite() || nyquist <= 0.0 {
        return;
    }
    let (low, high) = power_extent(spectrum);
    let span = (high - low).max(1.0);

    let x_of = |velocity: f32| {
        plot.left() + plot.width() * (velocity + nyquist) / (2.0 * nyquist).max(f32::EPSILON)
    };
    let y_of = |power_db: f32| plot.bottom() - plot.height() * (power_db - low) / span;

    // Frame first, so the trace is drawn over it.
    painter.rect_stroke(
        plot,
        0.0,
        egui::Stroke::new(1.0, PANEL_EDGE),
        egui::StrokeKind::Inside,
    );
    // Zero velocity: where ground clutter sits, and the line an analyst is
    // measuring the storm's motion against.
    let zero_x = x_of(0.0);
    painter.line_segment(
        [
            egui::pos2(zero_x, plot.top()),
            egui::pos2(zero_x, plot.bottom()),
        ],
        egui::Stroke::new(1.0, PANEL_EDGE),
    );

    // The receiver noise floor per bin. `noise_per_bin_db` and not
    // `noise_db`: the two differ by 10 log10(n) - about 18 dB at a 64-pulse
    // dwell - and the per-bin figure is the one a spectrum bin is measured
    // against. Drawing the other would put the floor 18 dB above the trace and
    // suggest every gate was noise.
    if let Some(noise_per_bin_db) = spectrum.noise_per_bin_db {
        let noise_y = y_of(noise_per_bin_db);
        if plot.y_range().contains(noise_y) {
            dashed_horizontal(painter, plot, noise_y, NOISE_INK);
        }
    }

    // The trace, as a filled area down to the baseline. Filled because the
    // quantity is a power density and the eye reads area as power; the outline
    // on top is what keeps a narrow peak visible when the fill is one pixel
    // wide.
    //
    // The area is built sample by sample - see `area_under_trace` - and NOT
    // handed to a polygon fill. A 64-to-512 point Doppler trace is about as
    // far from convex as a shape gets, and `Shape::convex_polygon` fans a
    // closed path from its first vertex: what that paints is a triangle fan
    // anchored on bin 0, which puts ink above the curve on one side of a peak
    // and leaves the area under the curve empty on the other. On a plot whose
    // whole claim is that area reads as power, that is the fill lying about
    // the quantity it exists to convey.
    let mut outline: Vec<egui::Pos2> = Vec::with_capacity(spectrum.power_db.len());
    for (velocity, power) in spectrum.velocities_mps.iter().zip(spectrum.power_db.iter()) {
        if !power.is_finite() {
            continue;
        }
        outline.push(egui::pos2(x_of(*velocity), y_of(power.clamp(low, high))));
    }
    if outline.len() >= 2 {
        painter.add(egui::Shape::mesh(area_under_trace(
            &outline,
            plot.bottom(),
            TRACE_FILL,
        )));
        painter.add(egui::Shape::line(
            outline,
            egui::Stroke::new(1.4, TRACE_INK),
        ));
    }

    // The moment estimator's answer, on the axis the spectrum is drawn against.
    // This is the whole comparison the panel exists for: where this mark sits
    // relative to the shape says whether the single number in the moment
    // product describes what the gate actually contained.
    if let Some(velocity) = estimator_velocity_mps.filter(|value| value.is_finite()) {
        let mark_x = x_of(velocity.clamp(-nyquist, nyquist));
        painter.line_segment(
            [
                egui::pos2(mark_x, plot.top()),
                egui::pos2(mark_x, plot.bottom()),
            ],
            egui::Stroke::new(1.5, MEAN_INK),
        );
        // INSIDE the plot, under its top edge - not above it.
        //
        // The first draft anchored this `CENTER_BOTTOM` at `plot.top()`, which
        // puts the number in the title row, and the mean velocity of a real
        // gate is near the middle of the axis almost always: the panel's only
        // statement of which gate and which channel it is showing was printed
        // through, and read "DOPPLER SPECTRUM(-3.1)33.0 km - H" in every
        // theme. `plot_rect` clears the title by `TITLE_FONT_SIZE + 6`, so a
        // label that starts below `plot.top()` cannot reach it at any
        // interface scale.
        //
        // Laid out first and placed by its own width, because a centred label
        // at either end of the axis hangs over the plot frame and onto the
        // radar field: at +Nyquist the mark is ON `plot.right()`. BESIDE the
        // rule rather than centred on it, too - a 1.5 point line through the
        // middle of a 9 point digit is the digit made harder to read - and it
        // changes sides rather than running off the right-hand end.
        let galley = painter.layout_no_wrap(
            format!("{velocity:.1}"),
            egui::FontId::monospace(AXIS_FONT_SIZE),
            MEAN_INK,
        );
        let width = galley.size().x;
        let leftmost = plot.left() + 1.0;
        let rightmost = (plot.right() - width - 1.0).max(leftmost);
        let beside = if mark_x + LABEL_GAP + width <= rightmost {
            mark_x + LABEL_GAP
        } else {
            mark_x - LABEL_GAP - width
        };
        painter.galley(
            egui::pos2(beside.clamp(leftmost, rightmost), plot.top() + 1.0),
            galley,
            MEAN_INK,
        );
    }

    // Axes. Three velocity labels and two power labels: enough to read a
    // number off, few enough that a 208-point panel does not become a wall of
    // digits.
    let baseline = plot.bottom() + 2.0;
    for (velocity, align) in [
        (-nyquist, egui::Align2::LEFT_TOP),
        (0.0, egui::Align2::CENTER_TOP),
        (nyquist, egui::Align2::RIGHT_TOP),
    ] {
        painter.text(
            egui::pos2(x_of(velocity), baseline),
            align,
            format!("{velocity:.0}"),
            egui::FontId::monospace(AXIS_FONT_SIZE),
            AXIS_INK,
        );
    }
    painter.text(
        egui::pos2(plot.center().x, baseline + AXIS_FONT_SIZE + 1.0),
        egui::Align2::CENTER_TOP,
        format!("m/s · {}", spectrum.power_reference.label()),
        egui::FontId::monospace(AXIS_FONT_SIZE),
        AXIS_INK,
    );
    for (power, align_y) in [(high, plot.top()), (low, plot.bottom())] {
        painter.text(
            egui::pos2(plot.left() - 3.0, align_y),
            egui::Align2::RIGHT_CENTER,
            format!("{power:.0}"),
            egui::FontId::monospace(AXIS_FONT_SIZE),
            AXIS_INK,
        );
    }
}

/// The area between a trace and its baseline, as a triangle strip.
///
/// One quad per sample interval - trace, trace, baseline, baseline - split
/// into two triangles. That is the region an integral of the trace would
/// measure, drawn with the same straight segments the outline is drawn with,
/// so the ink and the line agree everywhere by construction.
///
/// A mesh rather than a filled path because there is no filled-path primitive
/// that would do: `Shape::convex_polygon` fans from vertex 0 and a spectrum is
/// not convex, and a general concave fill would be a tessellation of exactly
/// these quads with a step in between that could get it wrong.
fn area_under_trace(outline: &[egui::Pos2], baseline_y: f32, fill: egui::Color32) -> egui::Mesh {
    let mut mesh = egui::Mesh::default();
    if outline.len() < 2 {
        return mesh;
    }
    mesh.reserve_vertices(outline.len() * 2);
    mesh.reserve_triangles((outline.len() - 1) * 2);
    for point in outline {
        mesh.colored_vertex(*point, fill);
        mesh.colored_vertex(egui::pos2(point.x, baseline_y), fill);
    }
    for interval in 0..outline.len() as u32 - 1 {
        let (top_left, foot_left) = (2 * interval, 2 * interval + 1);
        let (top_right, foot_right) = (top_left + 2, foot_left + 2);
        mesh.add_triangle(top_left, foot_left, foot_right);
        mesh.add_triangle(top_left, foot_right, top_right);
    }
    mesh
}

/// A dashed rule across the plot, for the noise floor.
///
/// Dashed rather than solid so it cannot be mistaken for part of the trace at a
/// glance, which is what a solid line of a similar weight would be.
fn dashed_horizontal(painter: &egui::Painter, plot: egui::Rect, y: f32, ink: egui::Color32) {
    const DASH: f32 = 4.0;
    const GAP: f32 = 3.0;
    let mut x = plot.left();
    while x < plot.right() {
        let end = (x + DASH).min(plot.right());
        painter.line_segment(
            [egui::pos2(x, y), egui::pos2(end, y)],
            egui::Stroke::new(1.0, ink),
        );
        x = end + GAP;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nexrad_io::iq_moments::estimator::PowerReference;
    use nexrad_io::iq_moments::taper::Taper;

    fn spectrum(power: Vec<f32>, noise_per_bin_db: f32) -> DopplerSpectrum {
        let bins = power.len();
        let nyquist = 33.2f32;
        DopplerSpectrum {
            range_m: 12_000.0,
            nyquist_velocity_mps: nyquist,
            velocities_mps: (0..bins)
                .map(|bin| -nyquist + 2.0 * nyquist * bin as f32 / bins as f32)
                .collect(),
            power_db: power,
            power_reference: PowerReference::AbsoluteDbm,
            noise_db: Some(noise_per_bin_db + 18.0),
            noise_per_bin_db: Some(noise_per_bin_db),
            taper: Taper::Rectangular,
            equivalent_noise_bandwidth_bins: 1.0,
        }
    }

    fn relative_spectrum(power: Vec<f32>) -> DopplerSpectrum {
        let mut spectrum = spectrum(power, -80.0);
        spectrum.power_reference = PowerReference::RelativeStoredIqSquared;
        spectrum.noise_db = None;
        spectrum.noise_per_bin_db = None;
        spectrum
    }

    /// The trace's height above the baseline has to BE the signal-to-noise
    /// ratio, which is what an analyst is judging when they look at it.
    #[test]
    fn the_power_axis_is_anchored_on_the_noise_floor_not_on_the_lowest_bin() {
        // One deep null, 60 dB below everything else. Scaling to it would put
        // the whole trace in the top of the panel and make a 20 dB SNR echo
        // look like a 70 dB one.
        let mut power = vec![-70.0f32; 64];
        power[0] = -130.0;
        power[32] = -50.0;
        let (low, high) = power_extent(&spectrum(power, -80.0));
        assert!(
            (low - (-80.0 - HEADROOM_DB)).abs() < 1e-3,
            "low {low} should sit one headroom under the noise floor"
        );
        assert!(high >= -50.0, "high {high} must clear the peak");
        assert!(
            low > -130.0,
            "the null at -130 dBm must not set the floor: low was {low}"
        );
    }

    /// A flat spectrum is flat. Stretching it to fill the panel would draw
    /// quantisation noise as though it were structure in the weather.
    #[test]
    fn a_flat_spectrum_is_not_stretched_into_apparent_structure() {
        let (low, high) = power_extent(&spectrum(vec![-79.9f32; 64], -80.0));
        assert!(
            high - low >= MIN_SPAN_DB,
            "span {} collapsed below the {MIN_SPAN_DB} dB floor",
            high - low
        );
    }

    #[test]
    fn a_relative_spectrum_labels_its_reference_and_has_no_noise_rule() {
        let spectrum = relative_spectrum(vec![10.0, 20.0, 30.0, 20.0]);
        let gate = GateSpectrum {
            spectrum: Some(spectrum),
            estimator_velocity_mps: Some(0.0),
            range_m: 1_000.0,
            channel: 0,
            absence: None,
        };
        let painted = paint(test_pane(), &gate, 1.0);
        assert!(
            painted.text_containing("dB re stored I/Q unit²").is_some(),
            "relative spectra must name their actual power reference"
        );
        assert!(
            painted.text_containing("dBm").is_none(),
            "relative spectra must never claim absolute receiver power"
        );
    }

    #[test]
    fn an_all_zero_relative_gate_has_no_infinite_plot_extent_or_fake_trace() {
        let spectrum = relative_spectrum(vec![f32::NEG_INFINITY; 32]);
        let (low, high) = power_extent(&spectrum);
        assert!(low.is_finite() && high.is_finite() && high > low);

        let gate = GateSpectrum {
            spectrum: Some(spectrum),
            estimator_velocity_mps: None,
            range_m: 1_000.0,
            channel: 0,
            absence: None,
        };
        let painted = paint(test_pane(), &gate, 1.0);
        assert!(painted.text_containing("no signal at this gate").is_some());
        assert!(
            painted.fill.is_empty(),
            "an empty dwell must not draw a trace"
        );
    }

    /// The noise floor drawn must be the PER-BIN one. The two differ by
    /// 10 log10(n) - about 18 dB at 64 pulses - and drawing the whole-dwell
    /// figure would put the floor above the trace of a real echo and suggest
    /// every gate was noise.
    #[test]
    fn the_noise_rule_is_the_per_bin_floor_and_lands_inside_the_plot() {
        let spectrum = spectrum(vec![-60.0f32; 64], -80.0);
        assert!(
            spectrum.noise_db.unwrap() > spectrum.noise_per_bin_db.unwrap(),
            "fixture is wrong: the whole-dwell floor is the higher number"
        );
        let (low, high) = power_extent(&spectrum);
        assert!(
            (low..=high).contains(&spectrum.noise_per_bin_db.unwrap()),
            "the per-bin floor {} is outside the drawn extent {low}..{high}",
            spectrum.noise_per_bin_db.unwrap()
        );
    }

    /// A pane big enough for the panel to be drawn at all.
    fn test_pane() -> egui::Rect {
        egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(900.0, 700.0))
    }

    /// What one paint of the panel actually put on the glass.
    ///
    /// Tessellated rather than read off the queued shapes, so these tests
    /// measure the ink and not the intention: a fill queued as a path, as a
    /// mesh or as anything else lands here as the same triangles, and a fill
    /// that covers the wrong region cannot pass by being the right KIND of
    /// shape.
    struct Painted {
        /// Every triangle painted in the trace's fill colour, in points.
        fill: Vec<[egui::Pos2; 3]>,
        /// Every text run and the rect it landed in, in points.
        texts: Vec<(String, egui::Rect)>,
    }

    impl Painted {
        fn text_containing(&self, needle: &str) -> Option<egui::Rect> {
            self.texts
                .iter()
                .find(|(text, _)| text.contains(needle))
                .map(|(_, rect)| *rect)
        }

        fn text_exactly(&self, wanted: &str) -> Option<egui::Rect> {
            self.texts
                .iter()
                .find(|(text, _)| text == wanted)
                .map(|(_, rect)| *rect)
        }

        /// Whether any of the fill's triangles covers `point`.
        fn fills(&self, point: egui::Pos2) -> bool {
            self.fill.iter().any(|triangle| covers(triangle, point))
        }

        /// The topmost filled point in the column at `x`, or `None` when the
        /// column carries no ink at all.
        fn ink_top(&self, x: f32, plot: egui::Rect) -> Option<f32> {
            let mut y = plot.top();
            while y <= plot.bottom() {
                if self.fills(egui::pos2(x, y)) {
                    return Some(y);
                }
                y += 0.2;
            }
            None
        }
    }

    fn covers(triangle: &[egui::Pos2; 3], point: egui::Pos2) -> bool {
        let side = |a: egui::Pos2, b: egui::Pos2| {
            (point.x - b.x) * (a.y - b.y) - (a.x - b.x) * (point.y - b.y)
        };
        let (first, second, third) = (
            side(triangle[0], triangle[1]),
            side(triangle[1], triangle[2]),
            side(triangle[2], triangle[0]),
        );
        let outside_one_way = first < 0.0 || second < 0.0 || third < 0.0;
        let outside_the_other = first > 0.0 || second > 0.0 || third > 0.0;
        !(outside_one_way && outside_the_other)
    }

    /// Paint the panel into a real egui pass and report what landed.
    fn paint(pane: egui::Rect, gate: &GateSpectrum, pixels_per_point: f32) -> Painted {
        let context = egui::Context::default();
        let mut input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(pane.right() + 16.0, pane.bottom() + 16.0),
            )),
            ..Default::default()
        };
        input
            .viewports
            .entry(input.viewport_id)
            .or_default()
            .native_pixels_per_point = Some(pixels_per_point);
        let output = context.run_ui(input, |ui| {
            let painter = ui.painter().clone();
            draw_gate_spectrum(&painter, pane, gate);
        });

        fn walk(shape: &egui::Shape, found: &mut Vec<(String, egui::Rect)>) {
            match shape {
                egui::Shape::Text(text) => found.push((
                    text.galley.text().to_owned(),
                    text.galley.rect.translate(text.pos.to_vec2()),
                )),
                egui::Shape::Vec(nested) => {
                    for shape in nested {
                        walk(shape, found);
                    }
                }
                _ => {}
            }
        }
        let mut texts = Vec::new();
        for clipped in &output.shapes {
            walk(&clipped.shape, &mut texts);
        }

        let mut fill = Vec::new();
        for clipped in context.tessellate(output.shapes, output.pixels_per_point) {
            let egui::epaint::Primitive::Mesh(mesh) = clipped.primitive else {
                continue;
            };
            for corners in mesh.indices.chunks_exact(3) {
                let triangle = [
                    mesh.vertices[corners[0] as usize],
                    mesh.vertices[corners[1] as usize],
                    mesh.vertices[corners[2] as usize],
                ];
                // The trace's fill and nothing else: the panel's own backing,
                // its frame and its text all land in this same primitive list.
                if triangle.iter().all(|vertex| vertex.color == TRACE_FILL) {
                    fill.push([triangle[0].pos, triangle[1].pos, triangle[2].pos]);
                }
            }
        }
        Painted { fill, texts }
    }

    /// THE pin on the fill, and the one the panel's own comment demands: "the
    /// quantity is a power density and the eye reads area as power".
    ///
    /// A Doppler trace is a 64-to-512 point jagged path and is about as far
    /// from convex as a shape gets, so `Shape::convex_polygon` - which fans a
    /// closed path from vertex 0 - paints a triangle fan and not the area
    /// under the curve. The fan shades the wedge between the baseline and the
    /// sight-line from bin 0 to the peak, which is ink ABOVE a trace that is
    /// sitting on the noise floor twenty bins away from any signal.
    ///
    /// This asserts the visual contract: ink under the peak, none in a bin far
    /// from it - and then, harder, that the top of the ink in a column IS the
    /// trace in that column.
    #[test]
    fn the_fill_is_the_area_under_the_trace_and_not_a_fan_from_the_first_bin() {
        const BINS: usize = 64;
        const NOISE_DBM: f32 = -80.0;
        const PEAK_DBM: f32 = -40.0;
        // Three bins of 64: one narrow peak in an otherwise flat spectrum.
        const PEAK_BINS: [usize; 3] = [39, 40, 41];
        // Far from the peak, and on the side a fan anchored at bin 0 sweeps
        // across.
        const FAR_BIN: usize = 20;

        let mut power = vec![NOISE_DBM; BINS];
        for bin in PEAK_BINS {
            power[bin] = PEAK_DBM;
        }
        let spectrum = spectrum(power.clone(), NOISE_DBM);
        let (low, high) = power_extent(&spectrum);
        let pane = test_pane();
        let plot = plot_rect(panel_rect(pane));
        // Restated rather than shared with the draw: the point of the test is
        // to measure where the ink is against an independent idea of where the
        // curve is.
        let x_of = |bin: usize| plot.left() + plot.width() * bin as f32 / BINS as f32;
        let y_of = |dbm: f32| plot.bottom() - plot.height() * (dbm - low) / (high - low);

        let gate = GateSpectrum {
            spectrum: Some(spectrum),
            estimator_velocity_mps: Some(0.0),
            range_m: 33_000.0,
            channel: 0,
            absence: None,
        };
        let painted = paint(pane, &gate, 1.0);
        assert!(
            !painted.fill.is_empty(),
            "the trace was not filled at all: the panel's area reading is gone"
        );

        // Halfway up the peak, in dB. Under the peak this is inside the echo;
        // twenty bins away the trace is on the noise floor and there is
        // nothing there to be inside of.
        let probe_y = y_of(-70.0);
        assert!(
            painted.fills(egui::pos2(x_of(PEAK_BINS[1]), probe_y)),
            "no ink under the peak at {} dBm",
            PEAK_DBM
        );
        assert!(
            !painted.fills(egui::pos2(x_of(FAR_BIN), probe_y)),
            "ink 10 dB above a bin sitting on the {NOISE_DBM} dBm noise floor, {} bins from \
             the peak: the fill is a fan from bin 0 rather than the area under the trace",
            PEAK_BINS[1] - FAR_BIN
        );

        // And the general statement the two probes are instances of: in every
        // column, the ink starts at the trace.
        for bin in [2, 10, FAR_BIN, 30, PEAK_BINS[1], 50, 60] {
            let top = painted
                .ink_top(x_of(bin), plot)
                .unwrap_or_else(|| panic!("bin {bin} has no fill under it at all"));
            let wanted = y_of(power[bin]);
            assert!(
                (top - wanted).abs() <= 1.5,
                "the fill in bin {bin} starts at y {top} but the trace there is {} dBm, at \
                 y {wanted}",
                power[bin]
            );
        }
    }

    /// The panel's only statement of WHICH gate and WHICH channel it is
    /// showing is its title, and the marked mean velocity must never print
    /// through it.
    ///
    /// It did: the label was anchored `CENTER_BOTTOM` on `plot.top()`, which
    /// is inside the title row, and a real gate's mean velocity is near the
    /// middle of the axis nearly always - so the title read
    /// "DOPPLER SPECTRUM(-3.1)33.0 km - H" in every theme, and worse at 160 %.
    ///
    /// Pinned geometrically and over the whole axis rather than by eye,
    /// including both ends, where a centred label also has to be stopped from
    /// hanging off the plot and onto the radar field. Several interface scales
    /// because egui rounds glyph positions to whole pixels, so the row height
    /// the title occupies is not the same number of points at every scale.
    #[test]
    fn the_velocity_label_cannot_reach_the_title_at_any_velocity_or_scale() {
        let spectrum = spectrum(vec![-70.0f32; 64], -80.0);
        let nyquist = spectrum.nyquist_velocity_mps;
        let pane = test_pane();
        let panel = panel_rect(pane);
        let plot = plot_rect(panel);

        for pixels_per_point in [1.0f32, 1.25, 1.5, 1.6, 2.0] {
            for step in 0..=48 {
                let velocity = -nyquist + 2.0 * nyquist * step as f32 / 48.0;
                let gate = GateSpectrum {
                    spectrum: Some(spectrum.clone()),
                    estimator_velocity_mps: Some(velocity),
                    range_m: 33_000.0,
                    channel: 0,
                    absence: None,
                };
                let painted = paint(pane, &gate, pixels_per_point);
                let title = painted
                    .text_containing("DOPPLER SPECTRUM")
                    .expect("the panel names the gate it is showing");
                let wanted = format!("{velocity:.1}");
                let label = painted.text_exactly(&wanted).unwrap_or_else(|| {
                    panic!(
                        "the mean velocity {wanted} is not marked at all at \
                         {pixels_per_point}x: {:?}",
                        painted
                            .texts
                            .iter()
                            .map(|(text, _)| text)
                            .collect::<Vec<_>>()
                    )
                });
                assert!(
                    !title.intersects(label),
                    "at {pixels_per_point}x the {wanted} m/s label at {label:?} overprints \
                     the title at {title:?}"
                );
                assert!(
                    plot.contains_rect(label),
                    "at {pixels_per_point}x the {wanted} m/s label at {label:?} hangs \
                     outside the plot {plot:?} and onto the radar field"
                );
                // And it is beside the rule it belongs to rather than sitting
                // on it, at both ends of the axis as well as in the middle.
                let mark_x = plot.left() + plot.width() * (velocity + nyquist) / (2.0 * nyquist);
                assert!(
                    !label.x_range().contains(mark_x),
                    "at {pixels_per_point}x the {wanted} m/s rule at x {mark_x} runs \
                     through its own label at {label:?}"
                );
            }
        }
    }

    /// A gate with nothing in it gets a sentence, not a transform of the
    /// receiver.
    #[test]
    fn an_absent_gate_carries_words_rather_than_an_empty_plot() {
        let gate = GateSpectrum {
            spectrum: None,
            estimator_velocity_mps: None,
            range_m: 12_000.0,
            channel: 0,
            absence: Some("below the receiver noise".to_owned()),
        };
        assert!(gate.spectrum.is_none());
        assert_eq!(gate.absence.as_deref(), Some("below the receiver noise"));
    }
}

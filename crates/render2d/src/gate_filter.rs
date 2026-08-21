//! Censoring gates before they become pixels.
//!
//! A gate filter is a set of criteria that decide, per gate, whether the gate
//! is drawn at all. The concept and the criteria implemented here are the ones
//! the Python ARM Radar Toolkit's `GateFilter` established as the standard
//! vocabulary for this operation - `exclude_below`, `exclude_above`, and the
//! cross-field `exclude_below(field, value)` that gates one moment on another
//! (Helmus, J. J. and Collis, S. M., 2016: "The Python ARM Radar Toolkit
//! (Py-ART), a Library for Working with Weather Radar Data in the Python
//! Programming Language", Journal of Open Research Software, 4(1), e25,
//! doi:10.5334/jors.119).
//!
//! The correlation-coefficient criterion is the dual-polarisation censor for
//! non-meteorological echo. Ground clutter, anomalous propagation, insects,
//! birds and chaff all scatter incoherently across the horizontal and vertical
//! channels, so their co-polar correlation coefficient rho_HV collapses well
//! below the 0.97-0.99 that rain holds, and a threshold on rho_HV separates
//! them from weather with very little else to go on (Zrnic, D. S. and Ryzhkov,
//! A. V., 1999: "Polarimetry for Weather Surveillance Radars", Bulletin of the
//! American Meteorological Society, 80(3), 389-406; Ryzhkov, A. V. and Zrnic,
//! D. S., 2019: "Radar Polarimetry for Weather Observations", Springer
//! Atmospheric Sciences, doi:10.1007/978-3-030-05093-1, chapter 6). The two
//! thresholds this module ships against in its proof runs, 0.80 and 0.95, are
//! the two ends of the usual operational range: 0.80 removes only what is
//! plainly not weather, 0.95 removes the melting layer and most of the
//! non-uniform-beam-filling edges with it.
//!
//! # The safety rule
//!
//! Every criterion here can hide real weather. The contract this module holds
//! up its half of is that a filtered gate is ABSENT, never recoloured and
//! never zeroed, so nothing downstream can mistake a censored gate for a
//! measured zero; and that the caller is handed a [`GateFilterReport`] saying
//! exactly what was hidden and how much of it, so a pane can say so where an
//! analyst cannot miss it. Absence of echo must never be the only evidence
//! that a filter is on.
//!
//! # Absence has to survive the rasteriser too
//!
//! Blanking the gate is not by itself enough, and finding out why is worth
//! writing down. The rasteriser maps each screen pixel to a 0.1 degree azimuth
//! bin, and a bin can hold more than one radial: `AzimuthLookup` gives each
//! radial group a half-width out to its neighbours and the floor/ceil bin
//! bounds overlap, so on a real KDVN super-resolution sweep about a third of
//! the 3,600 bins list TWO radials. The raster walks that candidate list and
//! paints the first one with an opaque colour, stepping past candidates that
//! hold nothing - which is right for a gate the radar never filled, and wrong
//! for a gate a filter removed. Measured on one real KDVN volume, blanking the
//! gate and no more: a rho_HV > 0.80 censor on cut 1 velocity emptied 21,253
//! pixels and RECOLOURED 2,924 others, each of them then showing a value from a
//! beam up to half a degree away - purple range-folded pixels replaced by a
//! neighbour's velocity among them. Every criterion did it: 1,513 pixels under
//! REF > 5, 1,669 under REF > 20, 3,390 under rho_HV > 0.95, 2,875 under
//! hide-range-folded. Only `min_range_km` escaped, because it censors whole
//! contiguous rings and every candidate in a bin dies together.
//!
//! So the censor is carried into the raster as a [`GateFilterMask`] rather than
//! only as blanked words: `AzimuthLookup` holds the mask, and the candidate
//! walk STOPS at the first candidate whose own gate was censored instead of
//! falling through to the next beam. The candidate RANKING is built from the
//! sweep as it arrived rather than from the censored copy, too - censoring
//! shortens rows, row length ranks the candidates in a bin, and a re-ranked bin
//! repaints pixels whose own gate the filter never touched. With both of those
//! in place, on the same real volume, every one of those counts is zero: every
//! pixel an active filter changes goes from opaque to fully transparent, none
//! is recoloured, and none appears out of nothing.
//!
//! One honest exception, stated here rather than discovered later: with a
//! display-quality upgrade in play the softening and interpolation passes run
//! over the censored sweep, so a gate that SURVIVES the filter next to one that
//! did not can legitimately change colour - its interpolated value no longer has
//! the removed gate in it. That is the point of censoring before interpolating
//! rather than after, and it is a change of value, not a change of beam.
//!
//! Three more consequences of the safety rule are visible in the code below:
//!
//! * When a criterion cannot be evaluated at a gate - the companion sweep has
//!   no radial within three degrees, or the companion gate holds no number -
//!   the gate is KEPT. Unknown is not a reason to hide.
//! * When a cross-moment criterion has no companion sweep at all, it no-ops
//!   entirely and says so in the report ([`CompanionSweep::Unavailable`]),
//!   rather than hiding every gate because it could not check any of them.
//! * When a grid's encoding has no way to say "absent", [`masked_grid`]
//!   refuses rather than borrowing a raw code that already means something.
//!   Blanking gates the filter never selected would be the same failure as
//!   hiding weather, arriving by a different door.
//!
//! # Split cuts, and why a companion sweep is needed
//!
//! On a WSR-88D the moments an analyst wants to cross-reference are frequently
//! not on the same sweep. The low tilts of every modern VCP are *split cuts*:
//! a long-PRT surveillance sweep (CS/CD) that carries reflectivity and the
//! dual-polarisation moments out to about 460 km, followed by a short-PRT
//! Doppler sweep (CDW) at the same commanded elevation that carries velocity
//! and spectrum width with a usable Nyquist velocity (Crum, T. D. and Alberty,
//! R. L., 1993: "The WSR-88D and the WSR-88D Operational Support Facility",
//! Bulletin of the American Meteorological Society, 74(9), 1669-1687; OFCM,
//! 2006: Federal Meteorological Handbook No. 11, Part C, "WSR-88D Products and
//! Algorithms"). "Hide velocity where rho_HV is low" therefore has to reach
//! across two sweeps that were flown seconds apart, with azimuths that do not
//! line up and gate ladders that may differ.
//!
//! [`resolve_companion_sweep`] states the rule; [`CompanionSampler`] does the
//! geometry. Neither maps gate to gate by index, because index agreement
//! between two sweeps is a coincidence and not a contract.

use radar_core::{ElevationCut, GateRange, MomentGrid, MomentStorage, MomentType, RadarVolume};
use rayon::prelude::*;

#[cfg(test)]
mod tests;

/// Which gates a pane is allowed to draw.
///
/// Every field is off by default, and [`GateFilter::OFF`] is what the renderer
/// takes when a caller does not ask for anything else. An inactive filter is
/// not merely a no-op in effect: [`apply_gate_filter`] returns before it looks
/// at a single gate, so the unfiltered path costs exactly what it cost before
/// this module existed.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GateFilter {
    /// Hide reflectivity gates weaker than this, in dBZ. None = show everything.
    pub min_reflectivity_dbz: Option<f32>,
    /// Hide velocity gates whose companion reflectivity is weaker than this, in dBZ.
    pub velocity_requires_reflectivity_dbz: Option<f32>,
    /// Hide gates whose correlation coefficient (RhoHV) is below this, 0.0..=1.0.
    pub min_correlation: Option<f32>,
    /// Hide gates flagged range-folded (purple) rather than painting the RF colour.
    pub hide_range_folded: bool,
    /// Hide everything closer to the radar than this, in km (near-field clutter).
    pub min_range_km: Option<f32>,
}

impl GateFilter {
    /// Nothing hidden. The renderer's default, and the only state in which the
    /// output is guaranteed byte-identical to a build without this module.
    pub const OFF: Self = Self {
        min_reflectivity_dbz: None,
        velocity_requires_reflectivity_dbz: None,
        min_correlation: None,
        hide_range_folded: false,
        min_range_km: None,
    };

    /// The correlation threshold this filter actually censors with, held to
    /// the physical range of the moment it reads.
    ///
    /// rho_HV is a correlation coefficient. It is defined on 0..=1, which is
    /// what the field's own documentation on [`GateFilter`] promises, and a
    /// threshold outside that interval names no measurable quantity: a stored
    /// 2.0 would hide every gate whose rho_HV is known - better than 99% of a
    /// sweep - and a stored -1.0 would put a pane into a filtered state that
    /// hides nothing at all. [`GateFilter::is_active`] already refuses a NaN
    /// threshold on exactly that reasoning, and this is the same reasoning
    /// applied to a number that is finite but cannot mean anything.
    ///
    /// So the documented range is enforced here, once, in the one place every
    /// reader goes through: [`GateFilter::is_active`],
    /// [`GateFilter::hidden_summary`] and the censor itself all ask this rather
    /// than the field, which makes the contract self-enforcing instead of
    /// dependent on a caller validating it first. A value above 1.0 is clamped
    /// to 1.0, so the badge names the threshold that ran; a value at or below
    /// 0.0 cannot hide a gate and so is not active, the same rule
    /// `min_range_km` already follows.
    pub fn correlation_threshold(&self) -> Option<f32> {
        self.min_correlation
            .filter(|value| value.is_finite() && *value > 0.0)
            .map(|value| value.min(1.0))
    }

    /// The near-field cutoff this filter actually censors with, in km. `None`
    /// when it cannot hide a gate.
    pub fn range_threshold_km(&self) -> Option<f32> {
        self.min_range_km
            .filter(|value| value.is_finite() && *value > 0.0)
    }

    /// True when anything would be hidden.
    ///
    /// A threshold that is present but not a finite number cannot hide
    /// anything, so it does not count as active: a stored setting that arrived
    /// as a NaN must not put a pane into a filtered state it cannot explain.
    /// The same goes for a threshold that is finite but outside the range its
    /// moment lives in - see [`GateFilter::correlation_threshold`].
    pub fn is_active(&self) -> bool {
        self.min_reflectivity_dbz.is_some_and(f32::is_finite)
            || self
                .velocity_requires_reflectivity_dbz
                .is_some_and(f32::is_finite)
            || self.correlation_threshold().is_some()
            || self.hide_range_folded
            || self.range_threshold_km().is_some()
    }

    /// A short human phrase naming what this filter HIDES, e.g.
    /// `REF below 5 dBZ, RhoHV below 0.80`.
    ///
    /// Empty when [`GateFilter::is_active`] is false. This is the text every
    /// indicator in the application is built from - the pane header, the
    /// toolbar chip's hover, the control panel's own live line, and the
    /// engine's [`GateFilterReport::badge`] - so it names moments the way the
    /// product picker does and never abbreviates past recognition.
    ///
    /// # Why every phrase is written from the HIDDEN side
    ///
    /// This function used to describe what SURVIVED - `REF > 5 dBZ`,
    /// `RhoHV > 0.80`, `beyond 5 km` - for four of the five criteria, and
    /// what was removed for the fifth (`range-folded hidden`). Nothing could
    /// read it consistently, because the phrase carries no verb: each caller
    /// supplied its own and half of them supplied the wrong one. What shipped
    /// was a pane censoring everything weaker than 20 dBZ announcing
    /// `FILTERED - hiding REF > 20 dBZ` - the exact inverse of what it had
    /// done, printed in the one place an analyst goes to find out what is
    /// missing, three inches from a panel whose own slider read
    /// `Hide REF below 20.0 dBZ`.
    ///
    /// So there is ONE phrase table and it is written from the hidden side,
    /// rather than a second inverse-phrased builder for the pane. Two tables
    /// describing five criteria from opposite sides is the same drift that
    /// produced the bug, with twice the surface to drift on: with one table
    /// any caller may put any verb of hiding in front of this and be telling
    /// the truth, and a caller that wants the keep side has to say so in its
    /// own words rather than getting it by accident. The phrases are
    /// deliberately the ones the control panel already prints on its own
    /// sliders, so the pane and the control that set it read as one sentence.
    ///
    /// `hidden_summary` in the name, not `summary`: the sense is the thing a
    /// future edit must not quietly flip, so it is stated at every call site.
    pub fn hidden_summary(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        if let Some(dbz) = self.min_reflectivity_dbz.filter(|value| value.is_finite()) {
            parts.push(format!("REF below {} dBZ", trim_decimal(dbz, 1)));
        }
        if let Some(dbz) = self
            .velocity_requires_reflectivity_dbz
            .filter(|value| value.is_finite())
        {
            parts.push(format!("VEL where REF below {} dBZ", trim_decimal(dbz, 1)));
        }
        if let Some(rho) = self.correlation_threshold() {
            parts.push(format!("RhoHV below {rho:.2}"));
        }
        if self.hide_range_folded {
            parts.push("range-folded gates".to_owned());
        }
        if let Some(km) = self.range_threshold_km() {
            parts.push(format!("everything inside {} km", trim_decimal(km, 1)));
        }
        parts.join(", ")
    }
}

impl Default for GateFilter {
    fn default() -> Self {
        Self::OFF
    }
}

/// Where a cross-moment criterion read its gating moment from.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum CompanionSweep {
    /// The criterion was not asked for.
    NotRequested,
    /// The sweep being drawn carries the gating moment itself. This is the
    /// best case available: same radials, same beam, same instant.
    SameSweep { cut_index: usize },
    /// A different sweep in the same volume, resolved by
    /// [`resolve_companion_sweep`].
    Companion {
        cut_index: usize,
        elevation_deg: f32,
        /// Signed offset from the drawn sweep's midpoint to the companion's,
        /// in seconds. Negative means the companion was flown first, which is
        /// the ordinary case for a surveillance/Doppler split cut.
        seconds_from_target: f32,
    },
    /// No sweep in this volume carries the gating moment near this elevation.
    /// The criterion no-ops; it does not hide anything.
    Unavailable,
}

impl CompanionSweep {
    pub fn is_usable(&self) -> bool {
        matches!(self, Self::SameSweep { .. } | Self::Companion { .. })
    }

    /// One line describing where the gating moment came from, or why it was
    /// not read. `moment` names the gating moment, e.g. `"RhoHV"`.
    pub fn describe(&self, moment: &str) -> Option<String> {
        match self {
            Self::NotRequested => None,
            Self::SameSweep { cut_index } => {
                Some(format!("{moment} read from this sweep (cut {cut_index})"))
            }
            Self::Companion {
                cut_index,
                elevation_deg,
                seconds_from_target,
            } => Some(format!(
                "{moment} read from companion cut {cut_index} at {elevation_deg:.2} deg, {seconds_from_target:+.1} s away"
            )),
            Self::Unavailable => Some(format!("{moment} filter idle: no companion sweep")),
        }
    }
}

/// What one application of a [`GateFilter`] did.
///
/// The per-criterion counts are each measured INDEPENDENTLY: every one of them
/// is the number of visible gates that criterion alone would hide, whether or
/// not another criterion also hides it. `gates_hidden` is the size of the union
/// and is therefore never larger than their sum and never smaller than the
/// largest of them - which is what makes "filters compose" a checkable claim
/// rather than an assertion.
#[derive(Clone, Debug, PartialEq)]
pub struct GateFilterReport {
    /// The filter that produced this report, so a pane that has only the
    /// report can still name what is hidden without having to keep the filter
    /// alongside it.
    pub filter: GateFilter,
    /// Gates the sweep would have drawn something for before filtering: a
    /// value, or the range-folded colour.
    pub gates_visible: usize,
    /// Gates hidden by at least one criterion.
    pub gates_hidden: usize,
    pub hidden_by_min_reflectivity: usize,
    pub hidden_by_velocity_reflectivity: usize,
    pub hidden_by_min_correlation: usize,
    pub hidden_by_range_folded: usize,
    pub hidden_by_min_range: usize,
    /// Visible gates a cross-moment criterion was asked about and could not
    /// answer for - the companion sweep had no radial within three degrees, or
    /// its gate at that range held nothing. Those gates were KEPT. This is the
    /// number that explains why a censor did not clear as much as an analyst
    /// expected, and it belongs in the open rather than in a comment.
    pub kept_unknown_reflectivity: usize,
    pub kept_unknown_correlation: usize,
    pub reflectivity_companion: CompanionSweep,
    pub correlation_companion: CompanionSweep,
    /// Set when a filter was asked for and this product had no gates for it to
    /// run against, naming why in words a pane can show.
    ///
    /// `None` means the filter ran (or was never asked for). Some products are
    /// not rastered from one sweep's gates at all - a vertically integrated
    /// field is computed out of the whole volume by the product engine - so a
    /// per-gate display filter has nothing to attach to there. Reporting that
    /// as INACTIVE would make the pane's badge vanish the moment an analyst
    /// switched product, and the analyst would have no way to learn that this
    /// one pane is not obeying a setting they can see switched on everywhere
    /// else. The direction is safe - such a pane shows MORE than the filter
    /// asked for, never less - but silence about it is not.
    pub inapplicable_reason: Option<&'static str>,
}

impl GateFilterReport {
    /// The report for a filter that was never applied.
    pub const INACTIVE: Self = Self {
        filter: GateFilter::OFF,
        gates_visible: 0,
        gates_hidden: 0,
        hidden_by_min_reflectivity: 0,
        hidden_by_velocity_reflectivity: 0,
        hidden_by_min_correlation: 0,
        hidden_by_range_folded: 0,
        hidden_by_min_range: 0,
        kept_unknown_reflectivity: 0,
        kept_unknown_correlation: 0,
        reflectivity_companion: CompanionSweep::NotRequested,
        correlation_companion: CompanionSweep::NotRequested,
        inapplicable_reason: None,
    };

    /// The report for a product a filter cannot be applied to.
    ///
    /// `reason` is shown to the analyst verbatim, so it says what this product
    /// is rather than what the filter is: "integrated from the whole volume,
    /// not rastered from one sweep" tells them why the pane beside this one
    /// looks censored and this one does not.
    ///
    /// A filter that is off is not a filter that failed to apply, so an
    /// inactive filter still reports [`GateFilterReport::INACTIVE`] and the
    /// pane stays clean.
    pub fn not_applicable(filter: GateFilter, reason: &'static str) -> Self {
        if !filter.is_active() {
            return Self::INACTIVE;
        }
        Self {
            filter,
            inapplicable_reason: Some(reason),
            ..Self::INACTIVE
        }
    }

    /// The report for an ACTIVE filter that had no gates to run against: an
    /// empty sweep, a zero-length gate ladder, a cut index that is not in this
    /// volume.
    ///
    /// Not [`GateFilterReport::INACTIVE`], and the distinction is the whole
    /// point. INACTIVE carries `GateFilter::OFF`, so [`Self::is_inactive`] is
    /// true and [`Self::badge`] returns `None` - and the pane header then
    /// silently drops its filter statement for that frame while the analyst
    /// has a censor switched on. An empty frame is exactly when an analyst
    /// most needs to know a filter is on, because the emptiness is the only
    /// other thing on the pane to read and a filter is one of the two possible
    /// explanations for it. This reports the filter with every count at zero,
    /// so the badge reads `FILTERED: <criteria> - 0 of 0 gates hidden (0.0%)`,
    /// which is true and answers the question the empty pane asks.
    ///
    /// Not [`Self::not_applicable`] either, though its shape is close. That
    /// one means "this product cannot obey the filter" and prints FILTER NOT
    /// APPLIED, which would be a different falsehood here: the filter IS in
    /// force on this pane and would have censored gates had there been any.
    ///
    /// An inactive filter had nothing to do for the ordinary reason, and still
    /// reports INACTIVE so an unfiltered pane stays clean.
    pub fn nothing_to_filter(filter: GateFilter) -> Self {
        if !filter.is_active() {
            return Self::INACTIVE;
        }
        Self {
            filter,
            ..Self::INACTIVE
        }
    }

    /// True when nothing was hidden and no cross-moment criterion had to be
    /// abandoned - the state in which a pane needs no badge.
    pub fn is_inactive(&self) -> bool {
        *self == Self::INACTIVE
    }

    /// True when the filter ran against this product's gates at all.
    pub fn is_applicable(&self) -> bool {
        self.inapplicable_reason.is_none()
    }

    pub fn hid_anything(&self) -> bool {
        self.gates_hidden > 0
    }

    /// Fraction of the visible gates that were hidden, 0.0..=1.0.
    pub fn hidden_fraction(&self) -> f32 {
        if self.gates_visible == 0 {
            return 0.0;
        }
        self.gates_hidden as f32 / self.gates_visible as f32
    }

    /// The one line a pane must show whenever a filter ran.
    ///
    /// `None` only when nothing was filtered and nothing was abandoned, which
    /// is the state in which the pane is showing everything the radar sent.
    /// Any other state is a state an analyst has to be told about, including
    /// the state where a filter was asked for and could not run: an idle
    /// criterion that the analyst believes is working is its own hazard.
    ///
    /// e.g. `FILTERED: RhoHV below 0.80 - 99,403 of 239,189 gates hidden (41.6%)`
    ///
    /// A product the filter could not run against says so instead, e.g.
    /// `FILTER NOT APPLIED: RhoHV below 0.80 - this product is integrated from the
    /// whole volume, not rastered from one sweep`. That line is not decoration:
    /// it is the only way an analyst can tell a pane that is obeying the filter
    /// from a pane that is quietly ignoring it.
    pub fn badge(&self) -> Option<String> {
        if self.is_inactive() {
            return None;
        }
        let summary = self.filter.hidden_summary();
        let summary = if summary.is_empty() {
            "gate filter".to_owned()
        } else {
            summary
        };
        if let Some(reason) = self.inapplicable_reason {
            return Some(format!("FILTER NOT APPLIED: {summary} - {reason}"));
        }
        Some(format!(
            "FILTERED: {summary} - {} of {} gates hidden ({:.1}%)",
            thousands(self.gates_hidden),
            thousands(self.gates_visible),
            self.hidden_fraction() * 100.0
        ))
    }

    /// Lines a pane can show under its filter badge: where each cross-moment
    /// criterion read from, and which of them could not run.
    pub fn notes(&self) -> Vec<String> {
        [
            self.reflectivity_companion.describe("REF"),
            self.correlation_companion.describe("RhoHV"),
        ]
        .into_iter()
        .flatten()
        .collect()
    }
}

/// Which gates of one moment grid a filter hides.
///
/// One bit per gate, laid out row-major over the grid it was built for. Kept
/// separate from the masked grid so a readout can answer "this gate is hidden
/// by the filter" rather than reporting the censored gate as an absence, which
/// would make the filter invisible at exactly the moment an analyst is asking
/// about it.
#[derive(Clone, Debug, PartialEq)]
pub struct GateFilterMask {
    rows: usize,
    gate_count: usize,
    words_per_row: usize,
    bits: Vec<u64>,
    hidden_count: usize,
}

impl GateFilterMask {
    pub fn rows(&self) -> usize {
        self.rows
    }

    pub fn gate_count(&self) -> usize {
        self.gate_count
    }

    pub fn hidden_count(&self) -> usize {
        self.hidden_count
    }

    pub fn hides(&self, row: usize, gate: usize) -> bool {
        if row >= self.rows || gate >= self.gate_count {
            return false;
        }
        let word = row * self.words_per_row + gate / 64;
        self.bits[word] & (1 << (gate % 64)) != 0
    }
}

/// Both halves of one filter evaluation.
#[derive(Clone, Debug)]
pub struct GateFilterOutcome {
    /// `None` when the filter is off, or when it was on and hid nothing.
    pub mask: Option<GateFilterMask>,
    pub report: GateFilterReport,
}

/// Which sweep supplies a gating moment for the sweep being drawn.
///
/// The rule, in order:
///
/// 1. **The sweep itself, if it carries the moment.** Same radials, same beam,
///    same instant - no mapping error is possible. On VCP 212 the Doppler leg
///    of a split cut carries its own reflectivity, so gating velocity on
///    reflectivity takes this path and never crosses a sweep boundary.
/// 2. **Otherwise, the nearest sweep in SCAN ORDER that carries the moment and
///    is within 0.5 degrees in elevation**, ties broken by the smaller
///    elevation difference and then by the lower cut index.
/// 3. **Otherwise nothing**, and the criterion no-ops.
///
/// Scan order rather than elevation distance is the load-bearing choice, and
/// the reason is measurable in real data. In one KDVN VCP 212 volume the two
/// halves of the 0.5 degree split cut are recorded at 0.69 and 0.44 degrees -
/// a quarter of a degree apart, because `elevation_deg` is a measured angle and
/// not the commanded one - while a SAILS repeat of the SAME commanded 0.5
/// degrees, two minutes later in the volume, sits at 0.29 degrees and is
/// therefore CLOSER in elevation to the Doppler leg than its own surveillance
/// partner is. Nearest-in-elevation would pair the Doppler sweep with a
/// surveillance sweep flown 127 seconds away; nearest-in-scan-order pairs it
/// with the one flown 20 seconds away, which is the split cut it belongs to.
/// The elevation window is a guard against pairing across tilts, not the
/// ranking key.
pub fn resolve_companion_sweep(
    volume: &RadarVolume,
    cut_index: usize,
    moment: &MomentType,
) -> CompanionSweep {
    let Some(target) = volume.cuts.get(cut_index) else {
        return CompanionSweep::Unavailable;
    };
    if carries_moment(target, moment) {
        return CompanionSweep::SameSweep { cut_index };
    }

    let target_elevation = target.elevation_deg;
    let target_midpoint_ms = sweep_midpoint_ms(target);
    let mut best: Option<(usize, f32, usize)> = None;
    for (index, cut) in volume.cuts.iter().enumerate() {
        if index == cut_index || !carries_moment(cut, moment) {
            continue;
        }
        let elevation_delta = (cut.elevation_deg - target_elevation).abs();
        // A cut whose elevation is not a number cannot be shown to be near
        // this one, and "cannot be shown to be near" is a refusal, not a pass.
        if !elevation_delta.is_finite() || elevation_delta > COMPANION_MAX_ELEVATION_DELTA_DEG {
            continue;
        }
        let candidate = (index.abs_diff(cut_index), elevation_delta, index);
        let better = best.is_none_or(|current| {
            candidate
                .0
                .cmp(&current.0)
                .then_with(|| candidate.1.total_cmp(&current.1))
                .then_with(|| candidate.2.cmp(&current.2))
                .is_lt()
        });
        if better {
            best = Some(candidate);
        }
    }

    match best {
        None => CompanionSweep::Unavailable,
        Some((_, _, index)) => {
            let cut = &volume.cuts[index];
            CompanionSweep::Companion {
                cut_index: index,
                elevation_deg: cut.elevation_deg,
                seconds_from_target: (sweep_midpoint_ms(cut) - target_midpoint_ms) as f32 / 1_000.0,
            }
        }
    }
}

/// Evaluate a filter against one moment grid without building a masked grid.
///
/// This is the whole of the filter's work; [`apply_gate_filter`] is a thin
/// wrapper that also writes the mask into a copy of the grid.
pub fn evaluate_gate_filter(
    volume: &RadarVolume,
    cut_index: usize,
    grid: &MomentGrid,
    filter: &GateFilter,
) -> GateFilterOutcome {
    if !filter.is_active() {
        return GateFilterOutcome {
            mask: None,
            report: GateFilterReport::INACTIVE,
        };
    }
    // From here on the filter IS active, so every way out short of running it
    // reports `nothing_to_filter` rather than INACTIVE. The difference is what
    // the pane header says on an empty frame: INACTIVE carries `GateFilter::OFF`
    // and silences the badge, which would drop the filter statement at the one
    // moment an analyst most needs it - an empty pane with a censor switched on
    // has two possible explanations and only one of them is the weather.
    let Some(cut) = volume.cuts.get(cut_index) else {
        return GateFilterOutcome {
            mask: None,
            report: GateFilterReport::nothing_to_filter(*filter),
        };
    };

    let rows = grid.radial_count();
    let gate_count = grid.gate_range.gate_count;
    if rows == 0 || gate_count == 0 {
        return GateFilterOutcome {
            mask: None,
            report: GateFilterReport::nothing_to_filter(*filter),
        };
    }

    // Reflectivity gating applies only to a velocity grid, and correlation
    // gating to any grid. Resolving a companion for a criterion nobody asked
    // for would cost a volume scan and report a sweep that was never read.
    let wants_reflectivity = filter
        .velocity_requires_reflectivity_dbz
        .is_some_and(f32::is_finite)
        && grid.moment == MomentType::Velocity;
    let wants_correlation = filter.correlation_threshold().is_some();

    let reflectivity_companion = if wants_reflectivity {
        resolve_companion_sweep(volume, cut_index, &MomentType::Reflectivity)
    } else {
        CompanionSweep::NotRequested
    };
    let correlation_companion = if wants_correlation {
        resolve_companion_sweep(volume, cut_index, &MomentType::CorrelationCoefficient)
    } else {
        CompanionSweep::NotRequested
    };

    let reflectivity_sampler =
        companion_sampler(volume, reflectivity_companion, &MomentType::Reflectivity);
    let correlation_sampler = companion_sampler(
        volume,
        correlation_companion,
        &MomentType::CorrelationCoefficient,
    );

    // A companion that resolved but whose sampler could not be built - an
    // empty sweep, a zero gate ladder - is reported as unavailable rather than
    // as a companion that was read. A report must never name a sweep the
    // filter did not actually consult.
    let reflectivity_companion = downgrade_unread(reflectivity_companion, &reflectivity_sampler);
    let correlation_companion = downgrade_unread(correlation_companion, &correlation_sampler);

    let self_threshold = filter
        .min_reflectivity_dbz
        .filter(|value| value.is_finite())
        .filter(|_| grid.moment == MomentType::Reflectivity);
    let reflectivity_threshold = filter
        .velocity_requires_reflectivity_dbz
        .filter(|value| value.is_finite())
        .filter(|_| reflectivity_sampler.is_some());
    let correlation_threshold = filter
        .correlation_threshold()
        .filter(|_| correlation_sampler.is_some());
    let min_range_m = filter.range_threshold_km().map(|km| km * 1_000.0);

    let words_per_row = gate_count.div_ceil(64);
    let mut bits = vec![0_u64; words_per_row * rows];
    let values = GridValues::new(grid);
    let azimuths = row_azimuths(cut, grid);
    let first_gate_m = grid.gate_range.first_gate_m as f32;
    let gate_spacing_m = grid.gate_range.gate_spacing_m as f32;

    let counts = bits
        .par_chunks_exact_mut(words_per_row)
        .enumerate()
        .map(|(row, row_bits)| {
            let azimuth_deg = azimuths[row];
            let base = row * gate_count;
            let mut counts = Counts::default();
            for gate in 0..gate_count {
                let reading = values.read(grid, base + gate);
                let (value, folded) = match reading {
                    GateReading::Absent => continue,
                    GateReading::RangeFolded => (None, true),
                    GateReading::Value(value) => (Some(value), false),
                };
                counts.visible += 1;

                let range_m = first_gate_m + gate as f32 * gate_spacing_m;
                let mut hide = false;

                if let Some(threshold) = self_threshold
                    && value.is_some_and(|value| value < threshold)
                {
                    counts.min_reflectivity += 1;
                    hide = true;
                }
                if let (Some(threshold), Some(sampler)) =
                    (reflectivity_threshold, reflectivity_sampler.as_ref())
                {
                    match sampler.value_at(azimuth_deg, range_m) {
                        Some(dbz) if dbz < threshold => {
                            counts.velocity_reflectivity += 1;
                            hide = true;
                        }
                        Some(_) => {}
                        None => counts.unknown_reflectivity += 1,
                    }
                }
                if let (Some(threshold), Some(sampler)) =
                    (correlation_threshold, correlation_sampler.as_ref())
                {
                    match sampler.value_at(azimuth_deg, range_m) {
                        Some(rho) if rho < threshold => {
                            counts.min_correlation += 1;
                            hide = true;
                        }
                        Some(_) => {}
                        None => counts.unknown_correlation += 1,
                    }
                }
                if filter.hide_range_folded && folded {
                    counts.range_folded += 1;
                    hide = true;
                }
                if let Some(min_range_m) = min_range_m
                    && range_m < min_range_m
                {
                    counts.min_range += 1;
                    hide = true;
                }

                if hide {
                    counts.hidden += 1;
                    row_bits[gate / 64] |= 1 << (gate % 64);
                }
            }
            counts
        })
        .reduce(Counts::default, Counts::merge);

    let report = GateFilterReport {
        filter: *filter,
        gates_visible: counts.visible,
        gates_hidden: counts.hidden,
        hidden_by_min_reflectivity: counts.min_reflectivity,
        hidden_by_velocity_reflectivity: counts.velocity_reflectivity,
        hidden_by_min_correlation: counts.min_correlation,
        hidden_by_range_folded: counts.range_folded,
        hidden_by_min_range: counts.min_range,
        kept_unknown_reflectivity: counts.unknown_reflectivity,
        kept_unknown_correlation: counts.unknown_correlation,
        reflectivity_companion,
        correlation_companion,
        inapplicable_reason: None,
    };

    if counts.hidden == 0 {
        return GateFilterOutcome { mask: None, report };
    }

    GateFilterOutcome {
        mask: Some(GateFilterMask {
            rows,
            gate_count,
            words_per_row,
            bits,
            hidden_count: counts.hidden,
        }),
        report,
    }
}

/// Apply a filter, returning a copy of the grid with the hidden gates removed.
///
/// `None` for the grid means "draw the one you already have": the filter is
/// off, or it is on and hid nothing, or the grid's encoding cannot express
/// absence at all (see [`masked_grid`]). In the first two cases the caller
/// allocates nothing and the render path is unchanged.
///
/// A hidden gate is written as a word the grid's own encoding reads as ABSENT:
/// `MomentGrid::scaled_value` answers `None` for it, the palette gives it a
/// zero alpha, and the raster leaves the pixel transparent. It is never
/// recoloured and never set to a numeric zero, so a censored gate and a
/// measured 0 dBZ stay as far apart downstream as they are on screen.
///
/// The report is returned whether or not a grid comes back with it, because a
/// caller that renders through the mask instead of through a censored grid -
/// which is what this crate's own raster paths do - still owes the analyst the
/// badge.
pub fn apply_gate_filter(
    volume: &RadarVolume,
    cut_index: usize,
    grid: &MomentGrid,
    filter: &GateFilter,
) -> (Option<MomentGrid>, GateFilterReport) {
    let outcome = evaluate_gate_filter(volume, cut_index, grid, filter);
    let Some(mask) = outcome.mask else {
        return (None, outcome.report);
    };
    (masked_grid(grid, &mask), outcome.report)
}

/// A copy of `grid` with every gate the mask names written as absent, or
/// `None` when this grid has no way to say "absent" that does not also change
/// the meaning of a gate the filter never selected.
///
/// # Why this can refuse
///
/// A packed grid says "absent" with a reserved raw word, `MomentGrid::nodata`.
/// Every NEXRAD moment this repository decodes carries one. A decoder for
/// another format need not - ODIM, CFRadial and DORADE all have encodings
/// where absence is expressed some other way - and for such a grid there is no
/// word already meaning nothing.
///
/// The tempting fallback is to pick 0 and declare it nodata. That is not a
/// fallback, it is a bug with a comment on it: on a grid whose scale and offset
/// put real data at raw 0, declaring `nodata = 0` retroactively blanks every
/// pre-existing raw-0 gate in the sweep - gates the filter did not select, at
/// ranges and azimuths nobody asked about. A censor that removes data nobody
/// asked it to remove is the exact failure this module exists to prevent, so it
/// is not done here.
///
/// Instead: if the grid already has a nodata word that its storage can hold,
/// that word is used and `nodata` is left exactly as it was. Otherwise the grid
/// is scanned for a raw code it does not use anywhere, and that code becomes
/// the nodata word - safe precisely because no gate holds it, so no existing
/// gate changes meaning. Only when every code in the storage's range is already
/// in use does this return `None`, and then the caller must render through the
/// mask rather than through a censored grid.
///
/// The mask must have been built for this grid; a mask of a different shape
/// refuses rather than blanking the wrong gates.
pub fn masked_grid(grid: &MomentGrid, mask: &GateFilterMask) -> Option<MomentGrid> {
    let gate_count = grid.gate_range.gate_count;
    if mask.gate_count != gate_count || mask.rows != grid.radial_count() {
        debug_assert!(false, "gate filter mask does not match its grid");
        return None;
    }

    let mut filtered = grid.clone();
    let words_per_row = mask.words_per_row;
    let bits = &mask.bits;
    match &mut filtered.storage {
        MomentStorage::U8(values) => {
            let absent = absent_u8(grid)?;
            filtered.nodata = Some(u16::from(absent));
            blank_rows(values, gate_count, words_per_row, bits, absent);
        }
        MomentStorage::U16(values) => {
            let absent = absent_u16(grid)?;
            filtered.nodata = Some(absent);
            blank_rows(values, gate_count, words_per_row, bits, absent);
        }
        MomentStorage::F32(values) => {
            blank_rows(values, gate_count, words_per_row, bits, f32::NAN);
        }
    }
    Some(filtered)
}

/// Gates `before` would have drawn something for and `after` will not.
///
/// This is how a censor is carried across a transform that changes the shape of
/// the grid it is applied to. The display-quality passes soften and upsample
/// the polar lattice, so a mask built against the sweep as it sits in the cut
/// does not index the grid that finally reaches the raster. Running the same
/// transform over the censored sweep and over the clean one, and taking the
/// gates that went absent between them, gives a mask that indexes the grid the
/// raster actually walks - without having to model what the interpolator did.
///
/// `None` when the two grids are not the same shape, or when nothing went
/// absent.
pub fn absence_delta_mask(before: &MomentGrid, after: &MomentGrid) -> Option<GateFilterMask> {
    let rows = before.radial_count();
    let gate_count = before.gate_range.gate_count;
    if rows == 0
        || gate_count == 0
        || after.radial_count() != rows
        || after.gate_range.gate_count != gate_count
    {
        return None;
    }

    let words_per_row = gate_count.div_ceil(64);
    let mut bits = vec![0_u64; words_per_row * rows];
    let before_values = GridValues::new(before);
    let after_values = GridValues::new(after);
    let hidden_count: usize = bits
        .par_chunks_exact_mut(words_per_row)
        .enumerate()
        .map(|(row, row_bits)| {
            let base = row * gate_count;
            let mut hidden = 0;
            for gate in 0..gate_count {
                let was_drawn = before_values.read(before, base + gate) != GateReading::Absent;
                let is_drawn = after_values.read(after, base + gate) != GateReading::Absent;
                if was_drawn && !is_drawn {
                    hidden += 1;
                    row_bits[gate / 64] |= 1 << (gate % 64);
                }
            }
            hidden
        })
        .sum();

    (hidden_count > 0).then_some(GateFilterMask {
        rows,
        gate_count,
        words_per_row,
        bits,
        hidden_count,
    })
}

/// A companion sweep, indexed by azimuth so a gate can be looked up by
/// geometry.
///
/// The index is the same 0.1 degree azimuth lattice the rasteriser uses, and
/// each bin holds the row whose recorded azimuth is nearest that bin's centre.
/// A bin whose nearest radial is more than three degrees away holds nothing:
/// that is the same half-width the raster refuses to paint past, so the filter
/// never gates a drawn pixel on a radial the renderer would not have drawn.
pub struct CompanionSampler<'a> {
    grid: &'a MomentGrid,
    values: GridValues<'a>,
    bin_rows: Vec<u32>,
}

const COMPANION_BINS: usize = 3600;
const COMPANION_BIN_WIDTH_DEG: f32 = 0.1;
const COMPANION_MAX_AZIMUTH_DELTA_DEG: f32 = 3.0;
const COMPANION_MAX_ELEVATION_DELTA_DEG: f32 = 0.5;
const NO_ROW: u32 = u32::MAX;

impl<'a> CompanionSampler<'a> {
    pub fn new(cut: &ElevationCut, grid: &'a MomentGrid) -> Option<Self> {
        let rows = grid.radial_count();
        if rows == 0 || grid.gate_range.gate_count == 0 {
            return None;
        }

        let mut by_azimuth: Vec<(f32, u32)> = row_azimuths(cut, grid)
            .into_iter()
            .enumerate()
            .filter(|(_, azimuth)| azimuth.is_finite())
            .map(|(row, azimuth)| (azimuth.rem_euclid(360.0), row as u32))
            .collect();
        if by_azimuth.is_empty() {
            return None;
        }
        by_azimuth.sort_by(|left, right| left.0.total_cmp(&right.0));

        let count = by_azimuth.len();
        let mut bin_rows = vec![NO_ROW; COMPANION_BINS];
        for (bin, slot) in bin_rows.iter_mut().enumerate() {
            let centre = (bin as f32 + 0.5) * COMPANION_BIN_WIDTH_DEG;
            // The two radials that straddle this bin's centre in the sorted
            // ring. The modular indexing is what makes the ring a ring: a bin
            // just clockwise of due north still finds the radials sitting just
            // anticlockwise of 360, which a plain sorted search would miss and
            // leave a seam of ungated gates through north.
            let after = by_azimuth.partition_point(|(azimuth, _)| *azimuth < centre);
            let straddling = [
                by_azimuth[after % count],
                by_azimuth[(after + count - 1) % count],
            ];
            let nearest = straddling
                .into_iter()
                .map(|(azimuth, row)| (angular_separation_deg(azimuth, centre), row))
                .min_by(|left, right| left.0.total_cmp(&right.0));
            if let Some((delta, row)) = nearest
                && delta <= COMPANION_MAX_AZIMUTH_DELTA_DEG
            {
                *slot = row;
            }
        }

        Some(Self {
            grid,
            values: GridValues::new(grid),
            bin_rows,
        })
    }

    /// Read the companion moment at one azimuth and slant range.
    ///
    /// `None` means the companion has no usable number there - no radial
    /// within three degrees, no gate at that range, or a gate that holds
    /// nodata or a range-folded flag. Callers must read that as "unknown" and
    /// keep the gate; see the safety rule in the module docs.
    pub fn value_at(&self, azimuth_deg: f32, slant_range_m: f32) -> Option<f32> {
        if !azimuth_deg.is_finite() || !slant_range_m.is_finite() {
            return None;
        }
        let bin = azimuth_bin(azimuth_deg);
        let row = *self.bin_rows.get(bin)?;
        if row == NO_ROW {
            return None;
        }
        let gate = gate_for_range(&self.grid.gate_range, slant_range_m)?;
        match self.values.read(
            self.grid,
            row as usize * self.grid.gate_range.gate_count + gate,
        ) {
            GateReading::Value(value) if value.is_finite() => Some(value),
            _ => None,
        }
    }
}

/// Build the sampler for a resolved companion, or `None` when there is nothing
/// to read.
fn companion_sampler<'a>(
    volume: &'a RadarVolume,
    companion: CompanionSweep,
    moment: &MomentType,
) -> Option<CompanionSampler<'a>> {
    let cut_index = match companion {
        CompanionSweep::NotRequested | CompanionSweep::Unavailable => return None,
        CompanionSweep::SameSweep { cut_index } | CompanionSweep::Companion { cut_index, .. } => {
            cut_index
        }
    };
    let cut = volume.cuts.get(cut_index)?;
    let grid = cut.moments.get(moment)?;
    CompanionSampler::new(cut, grid)
}

fn downgrade_unread(
    companion: CompanionSweep,
    sampler: &Option<CompanionSampler<'_>>,
) -> CompanionSweep {
    if companion.is_usable() && sampler.is_none() {
        return CompanionSweep::Unavailable;
    }
    companion
}

fn carries_moment(cut: &ElevationCut, moment: &MomentType) -> bool {
    cut.moments
        .get(moment)
        .is_some_and(|grid| !grid.radial_indices.is_empty() && grid.gate_range.gate_count > 0)
}

fn sweep_midpoint_ms(cut: &ElevationCut) -> i64 {
    let first = cut
        .radials
        .first()
        .map(|radial| i64::from(radial.time_offset_ms))
        .unwrap_or_default();
    let last = cut
        .radials
        .last()
        .map(|radial| i64::from(radial.time_offset_ms))
        .unwrap_or(first);
    (first + last) / 2
}

/// The recorded azimuth of every row of a moment grid.
///
/// A grid's rows index `radial_indices`, not `radials`, because a moment can be
/// absent from some radials of its own cut. Reading `cut.radials[row]` instead
/// is the classic off-by-a-radial in this data model.
fn row_azimuths(cut: &ElevationCut, grid: &MomentGrid) -> Vec<f32> {
    grid.radial_indices
        .iter()
        .map(|index| {
            cut.radials
                .get(*index)
                .map(|radial| radial.azimuth_deg)
                .unwrap_or(f32::NAN)
        })
        .collect()
}

/// Gate index whose CENTRE is nearest `range_m`.
///
/// Rounding, not flooring: `render2d` centres gate `g` at
/// `first_gate_m + g * gate_spacing_m`, and a floor would shift every companion
/// lookup half a gate - 125 m on a super-resolution sweep - against the pixel
/// it is gating.
fn gate_for_range(gate_range: &GateRange, range_m: f32) -> Option<usize> {
    if gate_range.gate_spacing_m <= 0 || gate_range.gate_count == 0 {
        return None;
    }
    let offset = (range_m - gate_range.first_gate_m as f32) / gate_range.gate_spacing_m as f32;
    let gate = offset.round();
    if gate < 0.0 || gate >= gate_range.gate_count as f32 {
        return None;
    }
    Some(gate as usize)
}

fn azimuth_bin(azimuth_deg: f32) -> usize {
    let normalized = azimuth_deg.rem_euclid(360.0);
    ((normalized / COMPANION_BIN_WIDTH_DEG) as usize).min(COMPANION_BINS - 1)
}

fn angular_separation_deg(left_deg: f32, right_deg: f32) -> f32 {
    let difference = (left_deg - right_deg).abs().rem_euclid(360.0);
    difference.min(360.0 - difference)
}

/// Group a count in threes, because a badge that says 99403 makes an analyst
/// count digits at the moment they are trying to read a storm.
fn thousands(value: usize) -> String {
    let digits = value.to_string();
    let mut text = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            text.push(',');
        }
        text.push(digit);
    }
    text
}

fn trim_decimal(value: f32, decimals: usize) -> String {
    let text = format!("{value:.decimals$}");
    if !text.contains('.') {
        return text;
    }
    let trimmed = text.trim_end_matches('0').trim_end_matches('.');
    if trimmed.is_empty() || trimmed == "-" {
        "0".to_owned()
    } else {
        trimmed.to_owned()
    }
}

/// The raw word this U8 grid can be blanked with, or `None` if it has none.
///
/// The grid's own nodata word when it has one the storage can hold; otherwise
/// a code the grid does not use anywhere, which is safe to declare as nodata
/// precisely because declaring it changes the meaning of no existing gate.
/// `None` when all 256 codes are in use, and then nothing is blanked - see
/// [`masked_grid`].
fn absent_u8(grid: &MomentGrid) -> Option<u8> {
    if let Some(nodata) = grid.nodata
        && let Ok(nodata) = u8::try_from(nodata)
    {
        return Some(nodata);
    }
    let MomentStorage::U8(values) = &grid.storage else {
        return None;
    };
    let mut used = [false; 256];
    for value in values {
        used[usize::from(*value)] = true;
    }
    // The range-folded word is a drawn colour, not an absence, so it may not be
    // borrowed to mean "nothing here".
    if let Some(folded) = grid.range_folded
        && let Ok(folded) = u8::try_from(folded)
    {
        used[usize::from(folded)] = true;
    }
    used.iter().position(|used| !used).map(|code| code as u8)
}

/// As [`absent_u8`], for a 16-bit grid.
fn absent_u16(grid: &MomentGrid) -> Option<u16> {
    if let Some(nodata) = grid.nodata {
        return Some(nodata);
    }
    let MomentStorage::U16(values) = &grid.storage else {
        return None;
    };
    const WORDS: usize = (u16::MAX as usize + 1) / 64;
    let mut used = vec![0_u64; WORDS];
    for value in values {
        let code = usize::from(*value);
        used[code / 64] |= 1 << (code % 64);
    }
    if let Some(folded) = grid.range_folded {
        let code = usize::from(folded);
        used[code / 64] |= 1 << (code % 64);
    }
    used.iter()
        .position(|word| *word != u64::MAX)
        .map(|word| word * 64 + used[word].trailing_ones() as usize)
        .map(|code| code as u16)
}

fn blank_rows<T: Copy + Send + Sync>(
    values: &mut [T],
    gate_count: usize,
    words_per_row: usize,
    bits: &[u64],
    blank: T,
) {
    values
        .par_chunks_mut(gate_count)
        .enumerate()
        .for_each(|(row, row_values)| {
            let start = row * words_per_row;
            let Some(row_bits) = bits.get(start..start + words_per_row) else {
                return;
            };
            for (word_index, word) in row_bits.iter().enumerate() {
                let mut word = *word;
                while word != 0 {
                    let bit = word.trailing_zeros() as usize;
                    word &= word - 1;
                    let gate = word_index * 64 + bit;
                    if let Some(slot) = row_values.get_mut(gate) {
                        *slot = blank;
                    }
                }
            }
        });
}

#[derive(Clone, Copy, Debug, Default)]
struct Counts {
    visible: usize,
    hidden: usize,
    min_reflectivity: usize,
    velocity_reflectivity: usize,
    min_correlation: usize,
    range_folded: usize,
    min_range: usize,
    unknown_reflectivity: usize,
    unknown_correlation: usize,
}

impl Counts {
    fn merge(self, other: Self) -> Self {
        Self {
            visible: self.visible + other.visible,
            hidden: self.hidden + other.hidden,
            min_reflectivity: self.min_reflectivity + other.min_reflectivity,
            velocity_reflectivity: self.velocity_reflectivity + other.velocity_reflectivity,
            min_correlation: self.min_correlation + other.min_correlation,
            range_folded: self.range_folded + other.range_folded,
            min_range: self.min_range + other.min_range,
            unknown_reflectivity: self.unknown_reflectivity + other.unknown_reflectivity,
            unknown_correlation: self.unknown_correlation + other.unknown_correlation,
        }
    }
}

/// What one gate holds, told apart the way the raster tells them apart.
#[derive(Clone, Copy, Debug, PartialEq)]
enum GateReading {
    Value(f32),
    /// Ambiguous range. The raster paints this, so the filter counts it as
    /// visible and `hide_range_folded` is the only criterion that removes it.
    RangeFolded,
    /// Nodata, past the end of a short row, or a non-finite float. Nothing is
    /// drawn here already, so no filter can hide it.
    Absent,
}

#[derive(Clone, Copy)]
enum GridValues<'a> {
    U8(&'a [u8]),
    U16(&'a [u16]),
    F32(&'a [f32]),
}

impl<'a> GridValues<'a> {
    fn new(grid: &'a MomentGrid) -> Self {
        match &grid.storage {
            MomentStorage::U8(values) => Self::U8(values),
            MomentStorage::U16(values) => Self::U16(values),
            MomentStorage::F32(values) => Self::F32(values),
        }
    }

    fn read(&self, grid: &MomentGrid, index: usize) -> GateReading {
        let raw = match self {
            Self::U8(values) => match values.get(index) {
                Some(value) => u16::from(*value),
                None => return GateReading::Absent,
            },
            Self::U16(values) => match values.get(index) {
                Some(value) => *value,
                None => return GateReading::Absent,
            },
            Self::F32(values) => {
                return match values.get(index) {
                    Some(value) if value.is_finite() => GateReading::Value(*value),
                    _ => GateReading::Absent,
                };
            }
        };
        if grid.nodata == Some(raw) {
            return GateReading::Absent;
        }
        if grid.range_folded == Some(raw) {
            return GateReading::RangeFolded;
        }
        if grid.scale == 0.0 || !grid.scale.is_finite() {
            return GateReading::Absent;
        }
        GateReading::Value((raw as f32 - grid.offset) / grid.scale)
    }
}

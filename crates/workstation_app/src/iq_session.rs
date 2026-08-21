//! NEXRAD Level 1 (time series / I/Q): the sweep the analyst opened, and the
//! estimator settings the moments on screen were made with.
//!
//! # What this is, and what it is not
//!
//! Every other file this application reads is a SUMMARY. A Level II volume
//! arrives with its moments already estimated: the signal processor averaged
//! fifty to eighty pulses per radial, wrote six or seven numbers per gate, and
//! threw the pulses away. Level 1 is the pulses — the complex receiver voltage
//! per pulse per gate, before any estimator ran. Nothing about the moments is
//! decided until this module decides it.
//!
//! That is the whole point of the feature and it is also the obligation it
//! creates. A Level II field is the radar's answer; a Level 1 field is OUR
//! answer, and the dwell length, the window and the censor threshold that
//! produced it are as much a part of the number as the samples are. So the
//! settings travel with the sweep, [`IqSession::provenance`] states them in
//! words, and the pane badges the field as computed rather than delivered.
//!
//! **Level 1 is archive material.** The NEXRAD Radar Operations Center states
//! that Level I "is not collected regularly or disseminated in real time":
//! there is no feed, there never was one, and nothing here polls, follows or
//! updates. A session is one file an analyst opened.
//!
//! # References
//!
//! - Doviak, R. J. and D. S. Zrnic, *Doppler Radar and Weather Observations*,
//!   2nd ed., Academic Press 1993, ch. 4 and 6 — the pulse-pair estimators and
//!   the spectral interpretation of them.
//! - Bringi, V. N. and V. Chandrasekar, *Polarimetric Doppler Weather Radar*,
//!   Cambridge University Press 2001, ch. 5-6 — the dual-polarisation moments.
//! - Zrnic, D. S., "Spectral moment estimates from correlated pulse pairs",
//!   *IEEE Trans. Aerospace and Electronic Systems* 13, 344-354, 1977.
//! - Melnikov, V. M. et al., *J. Appl. Meteor. Climatol.* 50, 859-872, 2011.
//! - Ivic, I. R., C. Curtis and S. M. Torres, "Radial-based noise power
//!   estimation for weather radars", *J. Atmos. Oceanic Technol.* 30,
//!   2737-2753, 2013.
//!
//! The estimator itself lives in `nexrad_io::iq_moments`; this module is the
//! application's side of it.

use std::sync::Arc;

use chrono::{DateTime, TimeZone, Utc};
use nexrad_io::iq::{IqCalibration, IqSweep, PulseLayout};
use nexrad_io::iq_moments::estimator::SnrCensor;
use nexrad_io::iq_moments::spectrum::DopplerSpectrum;
use nexrad_io::iq_moments::taper::Taper;
use nexrad_io::iq_moments::{
    DwellPlan, MomentConfig, ProcessedSweep, SnrApplication, process_sweep, sweep_gate_spectrum,
};
use radar_core::{RadarSite, RadarVolume};

/// The three knobs the settings page offers, resolved into numbers.
///
/// Deliberately not [`MomentConfig`] itself. That type carries a dozen fields
/// the settings window does not offer — calibration offsets, the burst count,
/// the range-uniformity tolerance — and every one of them has a right answer
/// that comes from the file rather than from a slider. Keeping the analyst's
/// three choices in their own type means [`Self::moment_config`] is the one
/// place where "what was chosen" becomes "what was run", and the rest cannot
/// drift by being edited somewhere else.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct IqControls {
    /// Pulses per dwell: how many are averaged into one radial.
    ///
    /// This is the trade the feature exists to expose. A long dwell averages
    /// more pulses, so the estimates are steadier and the spectrum is finer in
    /// velocity; a short one resolves the storm's own changes and gives more
    /// radials across the sector. The signal processor made this choice once,
    /// at scan time, and no moment product records what it chose.
    pub dwell_pulses: usize,
    /// Window the dwell is tapered by before the transform.
    pub taper: Taper,
    /// Signal-to-noise floor, dB. Gates below it are left blank rather than
    /// drawn from noise.
    pub censor: SnrCensor,
}

impl Default for IqControls {
    /// The estimator's own defaults, not a second opinion about them.
    ///
    /// Read from [`MomentConfig::default`] and [`DwellPlan::default`] rather
    /// than restated, and pinned by a test, on the settings catalog's rule that
    /// a fresh settings file changes nothing. The rectangular window is the
    /// engine's default because it is the one the published pulse-pair formulas
    /// describe; von Hann is the usual choice for looking at a SPECTRUM, which
    /// is a decision the analyst makes on the page rather than one taken here.
    fn default() -> Self {
        let engine = MomentConfig::default();
        Self {
            dwell_pulses: engine.dwell.pulses,
            taper: engine.taper,
            censor: engine.censor,
        }
    }
}

/// Range the dwell slider offers.
///
/// The floor is the shortest dwell a lag-1 estimator can be formed from with
/// any averaging worth the name; the ceiling is where a dwell stops being a
/// dwell and becomes the whole record. The DEFAULT is not declared here — it
/// is [`DwellPlan::default`]'s, read through [`IqControls::default`], so the
/// page and the engine cannot drift apart.
///
/// `allow`, not `expect`: these are read by the mirror test below and by
/// nothing in the shipped binary, and dead code is judged per compilation
/// unit - so an `expect` would be unfulfilled in the binary's own build. Same
/// reason `WorkstationApp::settings_ui_mut` carries one.
#[allow(dead_code)]
pub const MIN_DWELL_PULSES: i64 = 8;
#[allow(dead_code)]
pub const MAX_DWELL_PULSES: i64 = 512;

impl IqControls {
    /// The estimator configuration these choices mean.
    ///
    /// `burst_samples` is zero and is not a knob: the reader has already taken
    /// the transmit samples out of `h`, `v` and `range_bins`, so asking the
    /// estimator to drop more would throw away real gates and label every
    /// survivor with the range of a gate further in. See
    /// `nexrad_io::iq`'s note on the range mask.
    ///
    /// The dwell is contiguous rather than sliding. A sliding dwell would give
    /// one radial per pulse and a smoother-looking sector, but the radials
    /// would share pulses and so would not be independent estimates, which is
    /// exactly the kind of picture that looks better than the data.
    pub fn moment_config(self) -> MomentConfig {
        MomentConfig {
            dwell: DwellPlan::contiguous(self.dwell_pulses),
            taper: self.taper,
            censor: self.censor,
            burst_samples: 0,
            ..MomentConfig::default()
        }
    }
}

/// One opened time-series record, kept so the knobs can re-run the estimator
/// over it without re-reading the file.
pub struct IqSession {
    sweep: Arc<IqSweep>,
    /// The file this came from, for the pane header and the provenance line.
    source_label: String,
    /// Site id with the signal processor's own suffix taken off: the reference
    /// records name themselves `KOUN_RVP`, which is the RVP8 in the KOUN
    /// equipment room rather than a separate radar, and the site directory
    /// knows it as `KOUN`.
    site_id: String,
    controls: IqControls,
    processed: Arc<ProcessedSweep>,
}

impl IqSession {
    /// Decode a record and run the estimator over it once.
    pub fn open(raw: &[u8], source_label: &str, controls: IqControls) -> Result<Self, String> {
        let sweep = nexrad_io::iq::decode_iq_time_series(raw).map_err(|error| error.to_string())?;
        Self::from_sweep(sweep, source_label, controls)
    }

    pub fn from_sweep(
        sweep: IqSweep,
        source_label: &str,
        controls: IqControls,
    ) -> Result<Self, String> {
        let site_id = site_id_of(&sweep.site);
        let controls = controls_for_sweep(&sweep, controls);
        let processed = run(&sweep, controls)?;
        Ok(Self {
            sweep: Arc::new(sweep),
            source_label: source_label.to_owned(),
            site_id,
            controls,
            processed: Arc::new(processed),
        })
    }

    /// The pulses themselves.
    ///
    /// `allow(dead_code)` for the reason [`MIN_DWELL_PULSES`] carries one: the
    /// tests below read this to prove that a knob change re-estimates the
    /// pulses already in memory rather than re-reading the file, and the
    /// shipped binary reaches the pulses through [`Self::spectrum`] instead.
    #[allow(dead_code)]
    pub fn sweep(&self) -> &IqSweep {
        &self.sweep
    }

    pub fn processed(&self) -> &ProcessedSweep {
        &self.processed
    }

    /// The choices the field on screen was made with. See [`Self::sweep`] for
    /// why this carries an `allow`.
    #[allow(dead_code)]
    pub fn controls(&self) -> IqControls {
        self.controls
    }

    pub fn site_id(&self) -> &str {
        &self.site_id
    }

    pub fn source_label(&self) -> &str {
        &self.source_label
    }

    /// Native pulses per ray when the acquisition carries hard ray
    /// boundaries. Such sessions use exactly one dwell per ray.
    pub fn native_dwell_pulses(&self) -> Option<usize> {
        native_dwell_pulses(&self.sweep)
    }

    /// Whether the source carries a receiver-noise reference from which SNR
    /// can be computed.
    pub fn snr_available(&self) -> bool {
        matches!(self.sweep.calibration, IqCalibration::Absolute { .. })
    }

    /// Time of the first pulse.
    pub fn time_utc(&self) -> DateTime<Utc> {
        Utc.timestamp_opt(self.sweep.time_utc, 0)
            .single()
            .unwrap_or_else(Utc::now)
    }

    /// Re-run the estimator with new choices. Returns `Ok(false)` when the
    /// choices are the ones already on screen, so a settings change that did
    /// not touch this page costs nothing.
    pub fn set_controls(&mut self, controls: IqControls) -> Result<bool, String> {
        let controls = controls_for_sweep(&self.sweep, controls);
        if controls == self.controls {
            return Ok(false);
        }
        let processed = run(&self.sweep, controls)?;
        self.controls = controls;
        self.processed = Arc::new(processed);
        Ok(true)
    }

    /// The sweep as a one-cut volume, ready for the ordinary render path.
    ///
    /// One cut and not seventeen. A time-series record is a single dwell
    /// sequence at one antenna position, so presenting it as a volume with a
    /// tilt ladder would be inventing sixteen cuts that were never scanned.
    /// `metadata.source_path` is deliberately left unset: the load path writes
    /// the file's own path there for every other format and would otherwise
    /// join this onto it, so a Level 1 file would be the one file in the
    /// application whose header read `path::path`.
    pub fn volume(&self, site: RadarSite) -> RadarVolume {
        let mut volume = RadarVolume::new(site, self.time_utc());
        volume.cuts.push(self.processed.cut.clone());
        volume
    }

    /// The Doppler spectrum of one gate of one dwell, on the same indices the
    /// rendered cut uses: `dwell` is the radial row, `gate` the gate column.
    pub fn spectrum(
        &self,
        dwell: usize,
        gate: usize,
        channel: usize,
    ) -> Result<DopplerSpectrum, String> {
        sweep_gate_spectrum(
            &self.sweep,
            &self.controls.moment_config(),
            dwell,
            gate,
            channel,
        )
        .map_err(|error| error.to_string())
    }

    /// One line saying how the field on screen was made.
    ///
    /// Shown wherever the field is, because for Level 1 this IS part of the
    /// measurement: the same pulses under a different dwell are a different
    /// picture, and an analyst comparing two screenshots has no other way to
    /// know which is which.
    pub fn provenance(&self) -> String {
        let report = &self.processed.report;
        let dwell = match self.native_dwell_pulses() {
            Some(native) => format!(
                "{} native {native}-pulse rays (dwell fixed to each measured ray)",
                report.dwells
            ),
            None => format!("{}-pulse dwells", report.pulses_per_dwell),
        };
        let power = match self.sweep.calibration {
            IqCalibration::Absolute { .. } => String::new(),
            IqCalibration::RelativeStoredIq => {
                "; power is relative (dB re stored I/Q unit²); absolute receiver power and \
                 calibrated reflectivity are unavailable"
                    .to_owned()
            }
        };
        format!(
            "moments computed here from {} pulses: {dwell}, {} window, {}{power}",
            report.pulses_available,
            report.taper.label(),
            snr_words(report.snr_application),
        )
    }
}

fn native_dwell_pulses(sweep: &IqSweep) -> Option<usize> {
    let PulseLayout::Rays(spans) = &sweep.pulse_layout else {
        return None;
    };
    spans.first().map(|span| span.len)
}

fn controls_for_sweep(sweep: &IqSweep, mut controls: IqControls) -> IqControls {
    if let Some(native) = native_dwell_pulses(sweep) {
        controls.dwell_pulses = native;
    }
    controls
}

/// Run the estimator, turning a refusal into words an analyst can act on.
fn run(sweep: &IqSweep, controls: IqControls) -> Result<ProcessedSweep, String> {
    let mut config = controls.moment_config();
    // The record's own declared mode, so the two waveforms that cannot be
    // pulse-paired are refused by name rather than producing a plausible wrong
    // velocity field. `iMajorMode` 12 is batch / staggered PRT and 15 is SZ-2
    // phase coding; see `nexrad_io::iq_moments`.
    config.declared_major_mode = sweep.major_mode.and_then(|mode| u32::try_from(mode).ok());
    process_sweep(sweep, &config).map_err(|error| error.to_string())
}

/// How a censor setting is written in a provenance line.
fn censor_words(censor: SnrCensor) -> String {
    match censor {
        SnrCensor::Off => "no SNR threshold".to_owned(),
        SnrCensor::MinDb(db) => format!("gates below {db:.1} dB SNR left blank"),
    }
}

fn snr_words(application: SnrApplication) -> String {
    match application {
        SnrApplication::Applied { threshold_db } => censor_words(SnrCensor::MinDb(threshold_db)),
        SnrApplication::Off => censor_words(SnrCensor::Off),
        SnrApplication::UnavailableNoNoiseCalibration => {
            "SNR unavailable: source has no receiver-noise calibration".to_owned()
        }
    }
}

/// Strip the signal processor's suffix off a record's site name.
///
/// `sSiteName` names the PROCESSOR, not the radar: the reference records read
/// `KOUN_RVP`, meaning the RVP8 at KOUN. The site directory, the map anchor and
/// the pane header all want the radar.
fn site_id_of(site_name: &str) -> String {
    let trimmed = site_name.trim();
    let base = trimmed
        .split(['_', '-'])
        .next()
        .filter(|part| !part.is_empty())
        .unwrap_or(trimmed);
    base.to_ascii_uppercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_processor_suffix_is_not_part_of_the_radar_name() {
        // The reference records call themselves KOUN_RVP. That is the RVP8 in
        // the KOUN equipment room, not a radar called KOUN_RVP, and the site
        // directory has never heard of it - so the map would not anchor and the
        // pane header would name a station that does not exist.
        assert_eq!(site_id_of("KOUN_RVP"), "KOUN");
        assert_eq!(site_id_of("KOUN"), "KOUN");
        assert_eq!(site_id_of("koun_rvp"), "KOUN");
        assert_eq!(site_id_of("NOXP-RVP8"), "NOXP");
        // Nothing to strip, and nothing to invent.
        assert_eq!(site_id_of(""), "");
        assert_eq!(site_id_of("_RVP"), "_RVP");
    }

    #[test]
    fn the_burst_count_is_not_a_knob_and_stays_zero() {
        // The reader has already removed the transmit samples. Asking the
        // estimator to drop more would throw away real gates AND relabel every
        // survivor with the range of a gate further in - the whole-field error
        // that looks plausible on a display.
        for pulses in [MIN_DWELL_PULSES, 64, MAX_DWELL_PULSES] {
            let controls = IqControls {
                dwell_pulses: pulses as usize,
                ..IqControls::default()
            };
            assert_eq!(controls.moment_config().burst_samples, 0);
        }
    }

    #[test]
    fn dwells_are_contiguous_so_radials_do_not_share_pulses() {
        let controls = IqControls {
            dwell_pulses: 64,
            ..IqControls::default()
        };
        let config = controls.moment_config();
        assert_eq!(config.dwell.pulses, 64);
        assert_eq!(
            config.dwell.stride, 64,
            "a stride under the dwell length would make neighbouring radials \
             share pulses, which draws a smoother sector than was measured"
        );
    }

    /// A fresh settings file must change nothing about the picture.
    ///
    /// The settings catalog's rule for every other page, and it matters more
    /// here than anywhere: on Level 1 the window and the dwell are not display
    /// preferences, they are part of the measurement. A page that quietly
    /// defaulted to a different window from the estimator's would mean the
    /// numbers under the cursor stopped matching the formulas the estimator
    /// documents itself by.
    #[test]
    fn the_pages_defaults_are_the_estimators_own() {
        let controls = IqControls::default();
        let engine = MomentConfig::default();
        assert_eq!(controls.dwell_pulses, engine.dwell.pulses);
        assert_eq!(controls.taper, engine.taper);
        assert_eq!(controls.censor, engine.censor);
        // And the config the page builds from its own defaults is the config
        // the engine would have used unasked, knob for knob.
        let built = controls.moment_config();
        assert_eq!(built.dwell, engine.dwell);
        assert_eq!(built.taper, engine.taper);
        assert_eq!(built.censor, engine.censor);
    }

    #[test]
    fn the_default_dwell_is_inside_the_range_the_slider_offers() {
        let pulses = IqControls::default().dwell_pulses as i64;
        assert!(
            (MIN_DWELL_PULSES..=MAX_DWELL_PULSES).contains(&pulses),
            "default dwell {pulses} is outside the {MIN_DWELL_PULSES}..={MAX_DWELL_PULSES} \
             the page offers, so a fresh file would open on a clamped value"
        );
    }

    /// The settings page mirrors these numbers by hand, because `catalog.rs` is
    /// also compiled by the `settings` crate's UI harness, which has neither
    /// this module nor `nexrad_io` on its dependency list. This is the test
    /// that stops the mirror drifting: it is compiled HERE, where both sides
    /// are visible.
    ///
    /// A drift would not look like a bug. The page would offer a range the
    /// estimator does not have, or default to a dwell the estimator would not
    /// have chosen, and the field would simply be estimated differently from
    /// the way the page says it was.
    #[test]
    fn the_settings_page_mirrors_these_numbers_exactly() {
        use crate::settings_ui::catalog::timeseries_limits as limit;

        assert_eq!(limit::MIN_DWELL, MIN_DWELL_PULSES);
        assert_eq!(limit::MAX_DWELL, MAX_DWELL_PULSES);
        assert_eq!(
            limit::DEFAULT_DWELL,
            IqControls::default().dwell_pulses as i64
        );
        assert_eq!(
            SnrCensor::MinDb(limit::DEFAULT_SNR_DB as f32),
            IqControls::default().censor,
            "the page's default threshold is not the estimator's operational one"
        );
        // Every window the page offers has to resolve to a distinct taper, and
        // between them they have to cover every taper the estimator has - a
        // window the estimator supports and the page cannot reach is a window
        // nobody can choose.
        let offered = [
            limit::WINDOW_RECTANGULAR,
            limit::WINDOW_VON_HANN,
            limit::WINDOW_HAMMING,
            limit::WINDOW_BLACKMAN,
        ];
        let resolved: Vec<Taper> = offered
            .iter()
            .map(|id| crate::app::iq_taper_from_id(id))
            .collect();
        assert_eq!(resolved, Taper::ALL.to_vec());
    }

    /// The slider's leftmost stop means "no threshold", and the comparison that
    /// decides it is exact.
    ///
    /// A value that is NEARLY the floor is still a threshold that is on. A
    /// field reporting "no SNR threshold" in its provenance line while quietly
    /// hiding gates is the one failure the whole provenance line exists to
    /// prevent.
    #[test]
    fn the_censor_slider_is_off_only_at_its_declared_floor() {
        use crate::settings_ui::catalog::timeseries_limits as limit;
        assert_eq!(
            crate::app::iq_censor_from_db(limit::OFF_SNR_DB),
            SnrCensor::Off
        );
        assert_eq!(
            crate::app::iq_censor_from_db(limit::OFF_SNR_DB + 0.1),
            SnrCensor::MinDb((limit::OFF_SNR_DB + 0.1) as f32),
            "a hair above the floor is a threshold that is ON"
        );
        assert_eq!(
            crate::app::iq_censor_from_db(limit::DEFAULT_SNR_DB),
            SnrCensor::MinDb(2.0)
        );
    }

    /// Changing a knob re-estimates the sweep already in memory: same pulses,
    /// different field. Proved on a synthetic sweep because it is about the
    /// bookkeeping, not the arithmetic - the estimators are proved on real
    /// pulses in `nexrad_io`.
    #[test]
    fn a_knob_change_re_estimates_the_same_pulses() {
        let sweep = coherent_sweep();
        let pulses = sweep.pulses.len();
        let mut session =
            IqSession::from_sweep(sweep, "fixture", IqControls::default()).expect("processes");
        assert_eq!(session.sweep().pulses.len(), pulses);
        let before = session.processed().report.dwells;
        assert_eq!(session.controls().dwell_pulses, 64);

        // Same controls: nothing to do, and it says so rather than redoing the
        // work and bumping every pane's clock.
        assert!(!session.set_controls(IqControls::default()).expect("no-op"));

        let longer = IqControls {
            dwell_pulses: 128,
            ..IqControls::default()
        };
        assert!(session.set_controls(longer).expect("re-estimates"));
        assert_eq!(session.controls().dwell_pulses, 128);
        // The file was never re-read: the pulses are the ones already held.
        assert_eq!(session.sweep().pulses.len(), pulses);
        // And the field genuinely changed - twice the dwell, half the radials.
        assert_eq!(session.processed().report.dwells, before / 2);
        assert!(session.provenance().contains("128-pulse dwells"));
    }

    #[test]
    fn native_relative_rays_lock_the_dwell_and_never_claim_snr_or_calibration() {
        use nexrad_io::iq::{IqCalibration, PulseLayout, PulseSpan};
        use radar_core::MomentType;

        let mut sweep = coherent_sweep();
        sweep.calibration = IqCalibration::RelativeStoredIq;
        sweep.pulse_layout = PulseLayout::Rays(
            (0..8)
                .map(|ray| PulseSpan {
                    start: ray * 32,
                    len: 32,
                })
                .collect(),
        );
        let mut session = IqSession::from_sweep(sweep, "relative", IqControls::default())
            .expect("native rays process");

        assert_eq!(session.controls().dwell_pulses, 32);
        assert_eq!(session.native_dwell_pulses(), Some(32));
        assert_eq!(session.processed().report.dwells, 8);
        assert!(!session.snr_available());
        assert!(
            session
                .processed()
                .cut
                .moments
                .contains_key(&MomentType::RelativePower)
        );
        assert!(
            session
                .processed()
                .cut
                .moments
                .contains_key(&MomentType::Velocity)
        );
        assert!(
            !session
                .processed()
                .cut
                .moments
                .contains_key(&MomentType::Reflectivity)
        );

        let provenance = session.provenance();
        assert!(provenance.contains("native 32-pulse rays"), "{provenance}");
        assert!(provenance.contains("SNR unavailable"), "{provenance}");
        assert!(provenance.contains("power is relative"), "{provenance}");
        assert!(!provenance.contains("dBm"), "{provenance}");
        assert!(!provenance.contains("dBZ"), "{provenance}");

        let changed = IqControls {
            dwell_pulses: 128,
            taper: Taper::Hamming,
            ..IqControls::default()
        };
        assert!(session.set_controls(changed).expect("taper reprocesses"));
        assert_eq!(session.controls().dwell_pulses, 32);
        assert_eq!(session.controls().taper, Taper::Hamming);
    }

    /// A refusal is reported and does not destroy the session. An analyst who
    /// drags the dwell past the end of the record gets a message and keeps the
    /// field they had.
    #[test]
    fn a_refused_setting_leaves_the_previous_field_intact() {
        let mut session = IqSession::from_sweep(coherent_sweep(), "fixture", IqControls::default())
            .expect("processes");
        let good = session.processed().report.dwells;
        let absurd = IqControls {
            dwell_pulses: 100_000,
            ..IqControls::default()
        };
        assert!(session.set_controls(absurd).is_err());
        assert_eq!(session.controls().dwell_pulses, 64, "controls rolled back");
        assert_eq!(session.processed().report.dwells, good, "field kept");
    }

    /// A sweep of coherent tones, enough pulses for a 128-pulse dwell.
    fn coherent_sweep() -> IqSweep {
        use nexrad_io::iq::{IqCalibration, IqPulse};
        const PRT_S: f32 = 833.375e-6;
        let gates = 12;
        let pulses = (0..256)
            .map(|index| {
                let phase = 0.3 * index as f32;
                IqPulse {
                    azimuth_deg: (90.0 + 0.01 * index as f32).rem_euclid(360.0),
                    elevation_deg: 4.0,
                    prt_seconds: PRT_S,
                    prt_previous_seconds: PRT_S,
                    h: (0..gates)
                        .map(|_| (0.05 * phase.cos(), 0.05 * phase.sin()))
                        .collect(),
                    v: (0..gates)
                        .map(|_| (0.04 * phase.cos(), 0.04 * phase.sin()))
                        .collect(),
                    ..IqPulse::default()
                }
            })
            .collect();
        IqSweep {
            site: "KOUN_RVP".to_owned(),
            time_utc: 1_369_079_161,
            wavelength_m: 0.1108,
            pulse_width_s: Some(1.5e-6),
            gate_spacing_m: Some(500.0),
            first_gate_m: 1_000.0,
            range_bins: (0..gates).map(|bin| 1_000.0 + 500.0 * bin as f32).collect(),
            calibration: IqCalibration::Absolute {
                noise_dbm: [-80.5555, -80.5955],
                dbz_calibration: -35.5,
                saturation_dbm: 6.0,
            },
            pulses,
            ..IqSweep::default()
        }
    }

    #[test]
    fn a_censor_setting_is_written_out_rather_than_left_as_a_number() {
        assert_eq!(censor_words(SnrCensor::Off), "no SNR threshold");
        assert_eq!(
            censor_words(SnrCensor::MinDb(2.0)),
            "gates below 2.0 dB SNR left blank"
        );
    }
}

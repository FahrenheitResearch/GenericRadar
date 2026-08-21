//! NEXRAD Level 1 (time series / I/Q) reader for the Vaisala RVP8 / RVP900
//! TS record format.
//!
//! # What Level 1 is, and what it is not
//!
//! Level II — everything else this crate reads — is a summary. For each radial
//! the signal processor averages fifty to eighty transmitted pulses, writes
//! six or seven estimated moments per gate, and discards the pulses. Level 1
//! is the pulses themselves: the complex receiver voltage per pulse per gate,
//! before any estimator has run. From it a processor computes the moments for
//! itself — choosing the dwell length, the window, the clutter filter and the
//! censoring — and can compute the Doppler spectrum per gate, which no moment
//! product contains.
//!
//! Level 1 is an ARCHIVE format. The NEXRAD Radar Operations Center states
//! that Level I "is not collected regularly or disseminated in real time":
//! there is no live feed, and nothing in this module should be wired to one.
//! It exists for case studies against collections such as the NSSL research
//! archive.
//!
//! # Record structure
//!
//! One ASCII `rvp8PulseInfo` block describes the acquisition. Every
//! transmitted pulse then contributes an ASCII `rvp8PulseHdr` block followed
//! by exactly `2 * 2 * iNumVecs * iVIQPerBin` bytes of packed I/Q — two bytes
//! per [packed float](packed), an I and a Q per sample, `iNumVecs` samples per
//! channel and `iVIQPerBin` channels. There is no index and no trailer; the
//! stride rule is what walks the file, and it holds to the byte at EOF.
//!
//! Three details decide whether a reader is right or merely plausible, and
//! each of them was verified against real KOUN records rather than taken from
//! a document:
//!
//! 1. **The burst leads every channel block, and it is TWO samples long, not
//!    one.** The RVP8 reserves the first two samples of each channel's block
//!    for the transmitter's own burst — sampled through a coupler rather than
//!    received through the antenna — and the gates follow them. A producer
//!    that recorded a different number says so by writing the sentinel `9999`
//!    into `uiqOnce.iLong[0]`, in which case `uiqOnce.iLong[1]` is the count;
//!    otherwise the reserve is two. See [`IqSweep::burst_samples`].
//!
//!    The burst is written OVER the leading recorded bins rather than beside
//!    them, so it consumes that many range-mask positions and the first gate
//!    is `burst_samples` bins out. See [`GateLayout::range_bins`] for the
//!    evidence; getting the count right and the offset wrong puts every gate
//!    a kilometre too close on the reference record.
//!
//!    Nothing else in the record declares it, and in particular `iMaxVecs`
//!    does NOT. Every reference record has `iMaxVecs` exactly one more than
//!    the bits set in `iRangeMask` while carrying a two-sample burst, so an
//!    `iMaxVecs` rule reads as if it worked and does not: it hands a
//!    processor the transmit pulse as gate 0 — a 78 dB return at range zero
//!    with ZDR exactly 0 dB and rho_hv exactly 1.000 — and places every
//!    genuine gate one recorded bin too far out.
//!
//!    The records settle it themselves. Over all 1,830 pulses of the
//!    reference file the first TWO samples of the H block are bit-identical
//!    to the first two of the V block — 1,830 of 1,830 for each — while
//!    exactly one of the 453,840 later comparisons matches, at a pure-noise
//!    gate 77.5 km out where two twelve-bit denormals happened to coincide.
//!    Two independent receiver chains do not produce bit-identical I/Q from
//!    the sky, so neither leading sample is a measured gate. The second of
//!    them has Q exactly zero in every pulse and a magnitude equal to the
//!    header's own `RX[n].fBurstMag` to within half a quantisation step,
//!    which is what makes it the burst reference; the third is the first
//!    sample with independent per-channel structure. Three KOUN records of
//!    the same day — 250 and 598 vectors, alternate and contiguous masks,
//!    9,933 pulses between them — agree on all of it.
//! 2. **`iRangeMask` need not be contiguous.** It is a 512-word bitmap at
//!    `fRangeMaskRes` metre resolution, and a record may have every other bit
//!    set (`0x5555`, i.e. 500 m gates built from a 250 m mask) as readily as a
//!    contiguous run. A reader that assumes contiguous gates places every gate
//!    at the wrong range. The range of each recorded bin is therefore carried
//!    explicitly in [`IqSweep::range_bins`] rather than implied by its index.
//! 3. **The two channels are stored as blocks, not interleaved per gate.**
//!    For `iVIQPerBin = 2` the pulse's samples are all of channel 0 and then
//!    all of channel 1, `iNumVecs` samples each — not `(H, V)` per gate.
//!
//! # Modes this reader refuses
//!
//! `iMajorMode` 12 (batch / staggered PRT) and 15 (SZ-2 phase coding) are
//! refused by name rather than decoded. Both produce a plausible but wrong
//! velocity field under naive pulse-pair processing — staggered PRT because
//! consecutive pulses are not separated by a constant `T`, SZ-2 because the
//! transmit phase is modulated per pulse and must be removed before the
//! samples mean anything. Producing a field that looks right and is not is
//! worse than producing nothing.
//!
//! # Calibration this reader refuses
//!
//! `"NaN"`, `"inf"` and `"-inf"` all parse as `f32`, so a header can carry one
//! and be read without complaint. `fNoiseDBm`, `fSaturationDBM`, `fDBzCalib`
//! and `fRangeMaskRes` are therefore checked and refused by name, on the same
//! principle as the modes above: without that the record decodes, a frame
//! installs, and every moment of every gate comes out NaN — a completely empty
//! display with nothing on it that says the record's calibration is unusable.
//! `fRangeMaskRes` must also be positive, because zero puts every gate at
//! range zero and a NaN one makes a range mask that no uniformity check
//! written with `>` can catch.
//!
//! # What the contract carries
//!
//! [`IqSweep`] is deliberately self-sufficient: a processor must be able to
//! produce calibrated moments from it without reopening the file. That means
//! the range of every recorded bin, both channels' noise floors, the
//! reflectivity calibration and the saturation reference, the wavelength and
//! the pulse width, and per pulse the PRT on both sides so a stagger that the
//! header failed to declare is still detectable.
//!
//! # Moment estimation
//!
//! This module does not estimate moments — it hands a processor the samples.
//! The estimators those samples feed are the standard ones:
//!
//! ```text
//! S = R(0) - N            SNR = S / N          SQI = |R(1)| / R(0)
//! V = (lambda / 4 pi T) arg R(1)                v_a = lambda / 4T
//! W = (lambda / 2 sqrt(2) pi T) sqrt(ln(S / |R(1)|))
//! rho_hv = |C(0)| sqrt((1 + 1/SNR_h)(1 + 1/SNR_v))
//! Phi_DP = arg C(0) + phi_cal
//! ```
//!
//! # References
//!
//! - R. J. Doviak and D. S. Zrnić, *Doppler Radar and Weather Observations*,
//!   2nd ed., Academic Press, 1993 — ch. 4 (the received signal and its
//!   statistics) and ch. 6 (spectral moment estimation, pulse-pair and
//!   spectral estimators).
//! - V. N. Bringi and V. Chandrasekar, *Polarimetric Doppler Weather Radar:
//!   Principles and Applications*, Cambridge University Press, 2001 —
//!   ch. 5-6 (dual-polarisation covariance, ZDR, rho_hv and Phi_DP).
//! - D. S. Zrnić, "Spectral moment estimates from correlated pulse pairs",
//!   *IEEE Trans. Aerosp. Electron. Syst.* AES-13, 344-354, 1977 — the
//!   pulse-pair estimator itself.
//! - V. M. Melnikov, D. S. Zrnić, R. J. Doviak, et al., *J. Appl. Meteor.
//!   Climatol.* 50, 859-872, 2011 — polarimetric measurements at KOUN, the
//!   research WSR-88D these records come from.
//! - I. R. Ivić, C. Curtis and S. M. Torres, "Radial-based noise power
//!   estimation for weather radars", *J. Atmos. Oceanic Technol.* 30,
//!   2737-2753, 2013 — noise estimation, for a processor that would rather
//!   measure the floor than trust `fNoiseDBm`.
//! - Vaisala, *RVP900 Digital Receiver and Signal Processor User's Guide* —
//!   section 8 for the TS record layout, chapter 7 for the processor's own
//!   moment chain. Public at `ftp.sigmet.vaisala.com/files/manuals/`.
//! - Pulse-pair structure cross-read against OU RadarKit
//!   (github.com/OURadar/RadarKit, MIT licence).
//! - Burst-preamble length cross-read against NCAR LROSE `IwrfTsPulse`
//!   (github.com/NCAR/lrose-core, BSD-3-Clause), which reads the same RVP8
//!   records: `IWRF_RVP8_NGATES_BURST` is 2, overridden by `uiqOnce.iLong[1]`
//!   when `uiqOnce.iLong[0]` is the sentinel 9999, and the burst phase
//!   reference is the LAST preamble sample rather than the first.

pub mod packed;
mod text;

use thiserror::Error;

use text::{INFO_TAGS, PULSE_TAGS, read_block};

/// `iMajorMode` for batch / staggered PRT acquisition.
pub const MAJOR_MODE_BATCH_STAGGERED_PRT: i64 = 12;
/// `iMajorMode` for SZ-2 phase-coded acquisition.
pub const MAJOR_MODE_SZ2_PHASE_CODED: i64 = 15;

/// Leading samples per channel that hold the burst when the record does not
/// declare otherwise.
///
/// The RVP8 reserves the first two samples of each channel's block for the
/// transmitter's burst and starts the gates at the third. LROSE calls the
/// same number `IWRF_RVP8_NGATES_BURST`. See the module note on why no
/// arithmetic on `iMaxVecs` substitutes for it.
pub const DEFAULT_BURST_SAMPLES: usize = 2;

/// Sentinel in `uiqOnce.iLong[0]` that makes `uiqOnce.iLong[1]` the burst
/// sample count instead of [`DEFAULT_BURST_SAMPLES`].
const BURST_COUNT_DECLARED: i64 = 9999;

/// Bits per word of `iRangeMask`.
const MASK_BITS_PER_WORD: usize = 16;
/// Full-circle count of the 16-bit binary angles `iAz` and `iEl`.
const BINARY_ANGLE_FULL_CIRCLE: f32 = 65_536.0;

/// One transmitted pulse: the complex receiver voltage at every recorded gate.
///
/// `h` and `v` hold `(I, Q)` in the normalised units the record is written in,
/// where unit magnitude is [`IqSweep::saturation_dbm`]. Both are the same
/// length as [`IqSweep::range_bins`], and `v` is empty when only one channel
/// was recorded.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct IqPulse {
    /// Antenna azimuth at transmission, degrees clockwise from true north,
    /// in `[0, 360)`.
    pub azimuth_deg: f32,
    /// Antenna elevation at transmission, degrees, normalised to
    /// `(-180, 180]` so a below-horizon cut reads negative.
    pub elevation_deg: f32,
    /// Interval to the NEXT pulse, seconds — the `T` that pairs with a lag-1
    /// autocorrelation, and so the one a pulse-pair estimator wants.
    pub prt_seconds: f32,
    /// Interval since the PREVIOUS pulse, seconds. Equal to `prt_seconds` in
    /// a constant-PRF acquisition; carried separately so an undeclared
    /// stagger is visible without reopening the file.
    pub prt_previous_seconds: f32,
    /// Transmission time, whole seconds since the Unix epoch.
    pub time_utc: i64,
    /// Millisecond part of the transmission time.
    pub time_millis: u16,
    /// The burst that led this pulse's samples, when the acquisition
    /// recorded one.
    pub burst: Option<IqBurst>,
    /// `(I, Q)` per recorded range bin, horizontal channel.
    pub h: Vec<(f32, f32)>,
    /// `(I, Q)` per recorded range bin, vertical channel; empty when the
    /// acquisition recorded a single channel.
    pub v: Vec<(f32, f32)>,
}

/// The transmitter burst that precedes a pulse's gates.
///
/// The burst is a sample of the transmitted pulse itself, taken through a
/// coupler rather than through the antenna. Its phase is the transmitter's
/// phase for that pulse, which is what a processor subtracts to make a
/// magnetron-coherent or drift-corrected estimate; its magnitude tracks
/// transmitted power.
///
/// The acquisition reserves [`Self::preamble_samples`] samples at the head of
/// each channel's block for it, normally two. [`Self::h`] and [`Self::v`] are
/// the LAST of them, which is the one the processor phase-references the
/// record to and the one whose magnitude it reports as `RX[n].fBurstMag`;
/// the samples before it are the leading edge of the burst window and are
/// neither a gate nor a usable phase reference. In the reference records
/// taking the first instead reads 11.7% and 13.4% high in magnitude and 119
/// degrees away in phase, so every correction built on it would be wrong.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct IqBurst {
    /// Burst reference sample on channel 0: the last of the preamble.
    pub h: (f32, f32),
    /// Burst reference sample on channel 1, when two channels were recorded.
    pub v: Option<(f32, f32)>,
    /// Samples per channel the acquisition reserved for the burst, which is
    /// how many lead the gates. Normally [`DEFAULT_BURST_SAMPLES`].
    pub preamble_samples: usize,
    /// `RX[n].fBurstMag`: the magnitude the processor measured, per channel.
    pub reported_magnitude: [f32; 2],
    /// `RX[n].iBurstArg` converted to radians in `(-pi, pi]`.
    pub reported_phase_rad: [f32; 2],
}

/// One time-series record: an acquisition description and its pulses.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct IqSweep {
    /// `sSiteName`, e.g. `KOUN_RVP`.
    pub site: String,
    /// `taskID.sTaskName`, the acquisition task the operator ran.
    pub task_name: String,
    /// `sVersionString`, the signal processor's software version.
    pub processor_version: String,
    /// Time of the first pulse, whole seconds since the Unix epoch.
    pub time_utc: i64,
    /// Millisecond part of the first pulse's time.
    pub time_millis: u16,
    /// Radar wavelength in metres, from `fWavelengthCM`.
    pub wavelength_m: f32,
    /// Transmitted pulse width in seconds, from `fPWidthUSec`.
    pub pulse_width_s: f32,
    /// `iPolarization` verbatim. Advisory only: the number of channels
    /// actually recorded is [`Self::channels_recorded`], which comes from
    /// `iVIQPerBin` and is what the byte layout obeys.
    pub polarization_code: i64,
    /// Channels present per pulse, from `iVIQPerBin`: 1 or 2.
    pub channels_recorded: usize,
    /// Samples per channel the acquisition reserved for the burst before its
    /// first gate. Normally [`DEFAULT_BURST_SAMPLES`]; zero when the record
    /// declared no burst. Carried so a processor can see where the gates
    /// were taken to start without reopening the file.
    pub burst_samples: usize,
    /// `iMajorMode` verbatim. Modes 12 and 15 never reach here; see the
    /// module note on refusals.
    pub major_mode: i64,
    /// `iSampleSize`: the dwell length the processor itself would have used.
    /// A processor reading this record is free to choose another.
    pub nominal_sample_size: i64,
    /// `fRangeMaskRes`, the metre resolution of one `iRangeMask` bit.
    pub range_mask_res_m: f32,
    /// Spacing between recorded bins when they are evenly spaced, `None`
    /// when `iRangeMask` selected an irregular set. [`Self::range_bins`] is
    /// always authoritative; this is a convenience for the common case.
    pub gate_spacing_m: Option<f32>,
    /// Range of the first recorded bin, metres. Equal to `range_bins[0]`.
    pub first_gate_m: f32,
    /// Range in metres of every recorded bin, honouring `iRangeMask`.
    ///
    /// Bin `k` of [`IqPulse::h`] and [`IqPulse::v`] is at `range_bins[k]`.
    /// This is explicit rather than implied by the index because the mask may
    /// select alternate bins, or any other subset, and an index-times-spacing
    /// assumption then misplaces every gate.
    pub range_bins: Vec<f32>,
    /// `fNoiseDBm` per channel, in dBm. Convert to the record's normalised
    /// power with `10^((noise_dbm - saturation_dbm) / 10)`; that is the `N`
    /// subtracted in `S = R(0) - N`.
    pub noise_dbm: [f32; 2],
    /// `fDBzCalib`, the reflectivity calibration constant in dB.
    pub dbz_calibration: f32,
    /// `fSaturationDBM`: the power in dBm that unit sample magnitude
    /// represents. A sample's power in dBm is
    /// `10*log10(i*i + q*q) + saturation_dbm`.
    pub saturation_dbm: f32,
    /// Every pulse in the record, in the order it was transmitted.
    pub pulses: Vec<IqPulse>,
}

impl IqSweep {
    /// Number of recorded range bins per pulse per channel.
    #[must_use]
    pub fn gate_count(&self) -> usize {
        self.range_bins.len()
    }

    /// Whether two channels were recorded.
    #[must_use]
    pub fn is_dual_channel(&self) -> bool {
        self.channels_recorded == 2
    }

    /// Nyquist velocity in m/s from the first pulse's PRT, `v_a = lambda / 4T`
    /// (Doviak and Zrnić 1993, eq. 3.17). `None` for an empty record.
    #[must_use]
    pub fn nyquist_velocity_m_s(&self) -> Option<f32> {
        let prt = self.pulses.first()?.prt_seconds;
        (prt > 0.0).then(|| self.wavelength_m / (4.0 * prt))
    }

    /// Unambiguous range in metres from the first pulse's PRT, `r_a = cT/2`.
    /// `None` for an empty record.
    #[must_use]
    pub fn unambiguous_range_m(&self) -> Option<f32> {
        const SPEED_OF_LIGHT_M_S: f32 = 299_792_458.0;
        let prt = self.pulses.first()?.prt_seconds;
        (prt > 0.0).then(|| SPEED_OF_LIGHT_M_S * prt / 2.0)
    }

    /// Noise power for `channel` in the record's normalised units, ready to
    /// subtract from `R(0)`.
    #[must_use]
    pub fn noise_power(&self, channel: usize) -> f32 {
        let dbm = self.noise_dbm[channel.min(1)];
        10f32.powf((dbm - self.saturation_dbm) / 10.0)
    }

    /// Whether every pulse is separated from its neighbours by the same
    /// interval, within `tolerance` seconds.
    ///
    /// A processor should check this before running pulse-pair even on a
    /// record whose `iMajorMode` claims constant PRF, because the mode field
    /// describes intent and these two fields describe what happened.
    #[must_use]
    pub fn has_uniform_prt(&self, tolerance_s: f32) -> bool {
        let Some(first) = self.pulses.first() else {
            return true;
        };
        let reference = first.prt_seconds;
        self.pulses.iter().all(|pulse| {
            (pulse.prt_seconds - reference).abs() <= tolerance_s
                && (pulse.prt_previous_seconds - reference).abs() <= tolerance_s
        })
    }
}

/// What a time-series record says about itself, without decoding its pulses.
///
/// Reading this costs one ASCII block, so a file picker or a router can name
/// the record — site, mode, geometry, how many pulses — without paying for
/// hundreds of megabytes of sample decode.
#[derive(Clone, Debug, Default)]
pub struct IqSummary {
    /// `sSiteName`.
    pub site: String,
    /// `taskID.sTaskName`.
    pub task_name: String,
    /// `iMajorMode` verbatim, including the refused modes.
    pub major_mode: i64,
    /// `iPolarization` verbatim.
    pub polarization_code: i64,
    /// Channels per pulse, from the first pulse's `iVIQPerBin`.
    pub channels_recorded: usize,
    /// Samples per channel reserved for the burst; see
    /// [`IqSweep::burst_samples`].
    pub burst_samples: usize,
    /// Recorded range bins per pulse, from the first pulse header.
    ///
    /// This is the same number [`decode_iq_time_series`] would report, and a
    /// record whose gates this function cannot place is one that function
    /// refuses too — a peek never describes a record the decoder will
    /// decline.
    pub gate_count: usize,
    /// Pulses in the record, counted by walking the stride rule.
    pub pulse_count: usize,
    /// Time of the first pulse, seconds since the Unix epoch.
    pub time_utc: i64,
}

/// Everything that can go wrong reading a time-series record.
#[derive(Debug, Error)]
pub enum IqError {
    /// The bytes do not open with a time-series record.
    #[error(
        "not a time series record: expected a {what} block at offset {offset}, found {found:?}"
    )]
    MissingBlock {
        what: &'static str,
        offset: usize,
        found: String,
    },
    /// A block opened but never closed.
    #[error("unterminated {what} block starting at offset {offset}")]
    UnterminatedBlock { what: &'static str, offset: usize },
    /// A block held a line that is neither `key=value` nor its terminator.
    #[error("malformed line in {what} block: {line:?}")]
    MalformedLine { what: &'static str, line: String },
    /// A header line is not valid text.
    #[error("{what} line at offset {offset} is not valid UTF-8")]
    NotText { what: &'static str, offset: usize },
    /// A field this reader needs was not written.
    #[error("time series record has no {key} field")]
    MissingField { key: &'static str },
    /// A field was written but could not be read as its declared type.
    #[error("time series field {key} is not a number: {value:?}")]
    BadField { key: &'static str, value: String },
    /// The record ended in the middle of something.
    #[error("truncated {what} at offset {offset}: need {needed} bytes, have {available}")]
    Truncated {
        what: &'static str,
        offset: usize,
        needed: usize,
        available: usize,
    },
    /// An acquisition mode whose samples naive pulse-pair processing would
    /// silently misread.
    #[error(
        "iMajorMode {code} ({name}) is not supported: its pulses cannot be \
         processed by pulse-pair without first undoing the modulation, and \
         doing so naively yields a plausible but wrong field"
    )]
    UnsupportedMajorMode { code: i64, name: &'static str },
    /// The record reserved at least as many leading samples for the burst as
    /// it recorded, leaving nothing to be a gate.
    #[error(
        "cannot place gates: the record reserves {burst} burst sample(s) per channel \
         but recorded only {samples}, leaving nothing for a gate"
    )]
    BurstPreambleTooLong { burst: usize, samples: usize },
    /// A pulse declared a geometry the sweep-level range mapping cannot serve.
    #[error(
        "pulse {index} records {found} samples per channel but the record's range mask \
         places {expected}; a gate index would no longer mean the same range for every pulse"
    )]
    InconsistentGeometry {
        index: usize,
        found: usize,
        expected: usize,
    },
    /// A field held a value outside what the format allows.
    #[error("time series field {key} has unusable value {value}")]
    OutOfRange { key: &'static str, value: i64 },
    /// A calibration or range-scale field decoded, and decoded to a number no
    /// arithmetic can use.
    #[error(
        "time series field {key} is {value}, which no moment can be calibrated against: \
         every quantity derived from it would be NaN and the sweep would draw as an \
         entirely empty pane with nothing to say why"
    )]
    UnusableCalibration { key: &'static str, value: f32 },
    /// The record held no pulses at all.
    #[error("time series record holds no pulses")]
    NoPulses,
}

/// Whether `head` opens with a Vaisala time-series record.
///
/// Both the `rvp8` and the newer `rvpts` spelling of the opening block are
/// recognised. This reads only the leading bytes and allocates nothing.
#[must_use]
pub fn looks_like_iq_time_series(head: &[u8]) -> bool {
    text::starts_with_block(head, &INFO_TAGS)
}

/// Read a record's description and pulse count without decoding samples.
///
/// The geometry is worked out by exactly the code
/// [`decode_iq_time_series`] uses, so a router that peeks to describe a file
/// and then decodes it never prints a gate count for a record the decoder is
/// about to refuse. The two differ in one deliberate way only: a peek reports
/// [`IqSummary::major_mode`] verbatim, including the modes a decode declines,
/// because naming the mode is the whole point of being able to describe a
/// record one cannot process.
pub fn peek_iq_time_series(raw: &[u8]) -> Result<IqSummary, IqError> {
    let info = read_block(raw, 0, &INFO_TAGS, "pulse info")?;
    let first = read_block(raw, info.end, &PULSE_TAGS, "pulse header")?;
    let geometry = PulseGeometry::read(&first)?;
    // `iNumVecs` counts the burst samples too, so the gate count is not it.
    let layout = GateLayout::read(&info, &first, geometry)?;

    let mut pulse_count = 0usize;
    let mut cursor = info.end;
    let mut time_utc = 0i64;
    while cursor < raw.len() {
        let header = read_block(raw, cursor, &PULSE_TAGS, "pulse header")?;
        if pulse_count == 0 {
            time_utc = header.int("iTimeUTC")?;
        }
        let step = PulseGeometry::read(&header)?;
        let end = step.sample_bytes_end(header.end, raw.len())?;
        pulse_count += 1;
        cursor = end;
    }

    Ok(IqSummary {
        site: info.text("sSiteName").unwrap_or_default().to_owned(),
        task_name: info.text("taskID.sTaskName").unwrap_or_default().to_owned(),
        major_mode: info.int("iMajorMode")?,
        polarization_code: info.opt_int("iPolarization")?.unwrap_or(0),
        channels_recorded: geometry.channels,
        burst_samples: layout.burst_samples,
        gate_count: layout.gate_count,
        pulse_count,
        time_utc,
    })
}

/// Decode a whole time-series record.
///
/// Every pulse in the record is decoded, which for a large archive file means
/// a large allocation: a 440 MB record of 600-gate dual-channel pulses lands
/// near 1.5 GB once expanded to `f32` pairs. Use
/// [`decode_iq_time_series_limited`] to take a dwell off the front instead.
pub fn decode_iq_time_series(raw: &[u8]) -> Result<IqSweep, IqError> {
    decode_iq_time_series_limited(raw, usize::MAX)
}

/// Decode at most `max_pulses` pulses from the front of a record.
///
/// The header is read in full either way, so the returned sweep is as
/// calibrated and as well placed in range as a full decode; only the pulse
/// list is shorter.
pub fn decode_iq_time_series_limited(raw: &[u8], max_pulses: usize) -> Result<IqSweep, IqError> {
    let info = read_block(raw, 0, &INFO_TAGS, "pulse info")?;

    let major_mode = info.int("iMajorMode")?;
    if let Some(name) = refused_major_mode(major_mode) {
        return Err(IqError::UnsupportedMajorMode {
            code: major_mode,
            name,
        });
    }

    let first = read_block(raw, info.end, &PULSE_TAGS, "pulse header")?;
    let geometry = PulseGeometry::read(&first)?;

    let range_mask_res_m = info.float("fRangeMaskRes")?;
    // The range scale, before it is multiplied through every bin. Zero puts
    // every gate at range zero, and NaN puts every gate nowhere: the
    // uniformity check downstream compares deviations with `>`, and every
    // comparison against NaN is false, so a NaN ladder passes a test written
    // to catch a mask that cannot be drawn.
    if !range_mask_res_m.is_finite() || range_mask_res_m <= 0.0 {
        return Err(IqError::UnusableCalibration {
            key: "fRangeMaskRes",
            value: range_mask_res_m,
        });
    }
    let layout = GateLayout::read(&info, &first, geometry)?;
    let burst_samples = layout.burst_samples;
    let range_bins = layout.range_bins(range_mask_res_m);

    let noise = info.float_list("fNoiseDBm")?;
    let noise_dbm = [
        noise.first().copied().unwrap_or(f32::NEG_INFINITY),
        noise.get(1).copied().unwrap_or_else(|| {
            // A single-channel record states one floor; reusing it keeps the
            // array total without inventing a second measurement.
            noise.first().copied().unwrap_or(f32::NEG_INFINITY)
        }),
    ];
    let dbz_calibration = info.float_or("fDBzCalib", 0.0)?;
    let saturation_dbm = info.float_or("fSaturationDBM", 0.0)?;
    // The calibration, checked here rather than left to produce a silent
    // nothing later. `iMajorMode` 12 and 15 are refused above BY NAME because
    // decoding them yields a plausible wrong field; a NaN noise floor or an
    // infinite saturation reference is the same failure with the plausible
    // part removed - the record decodes, a frame installs, the pane raises its
    // LEVEL 1 and COMPUTED badges and writes the provenance line, and then
    // every gate of every moment is NaN and the pane is empty with nothing on
    // it that says the record's calibration is unusable.
    //
    // `noise_dbm` is checked as decoded, so the reused single-channel floor is
    // checked as the value the estimator will actually read.
    require_usable("fNoiseDBm", noise_dbm[0])?;
    require_usable("fNoiseDBm", noise_dbm[1])?;
    require_usable("fDBzCalib", dbz_calibration)?;
    require_usable("fSaturationDBM", saturation_dbm)?;

    let mut sweep = IqSweep {
        site: info.text("sSiteName").unwrap_or_default().to_owned(),
        task_name: info.text("taskID.sTaskName").unwrap_or_default().to_owned(),
        processor_version: info.text("sVersionString").unwrap_or_default().to_owned(),
        time_utc: 0,
        time_millis: 0,
        wavelength_m: info.float("fWavelengthCM")? / 100.0,
        pulse_width_s: info.float("fPWidthUSec")? * 1e-6,
        polarization_code: info.opt_int("iPolarization")?.unwrap_or(0),
        channels_recorded: geometry.channels,
        burst_samples,
        major_mode,
        nominal_sample_size: info.opt_int("iSampleSize")?.unwrap_or(0),
        range_mask_res_m,
        gate_spacing_m: uniform_spacing(&range_bins),
        first_gate_m: range_bins.first().copied().unwrap_or(0.0),
        range_bins,
        noise_dbm,
        dbz_calibration,
        saturation_dbm,
        pulses: Vec::new(),
    };

    let clock_hz = f64::from(info.float("fAqClkMHz")?) * 1e6;
    let mut cursor = info.end;
    let mut samples = Vec::new();
    while cursor < raw.len() && sweep.pulses.len() < max_pulses {
        let header = read_block(raw, cursor, &PULSE_TAGS, "pulse header")?;
        let step = PulseGeometry::read(&header)?;
        let end = step.sample_bytes_end(header.end, raw.len())?;
        if step.samples_per_channel != geometry.samples_per_channel
            || step.channels != geometry.channels
        {
            return Err(IqError::InconsistentGeometry {
                index: sweep.pulses.len(),
                found: step.samples_per_channel.saturating_sub(burst_samples),
                expected: sweep.range_bins.len(),
            });
        }

        packed::unpack_all(&raw[header.end..end], &mut samples);
        let pulse = build_pulse(&header, &step, &samples, burst_samples, clock_hz)?;
        if sweep.pulses.is_empty() {
            sweep.time_utc = pulse.time_utc;
            sweep.time_millis = pulse.time_millis;
        }
        sweep.pulses.push(pulse);
        cursor = end;
    }

    if sweep.pulses.is_empty() {
        return Err(IqError::NoPulses);
    }
    Ok(sweep)
}

/// Refuse a header number that decoded to something no arithmetic can use.
///
/// `"NaN"`, `"inf"` and `"-inf"` all parse as `f32` perfectly happily, so a
/// header can carry them and be read without complaint. The complaint has to
/// be made here, by name, for the reason the unsupported major modes are named
/// rather than silently dropped: the alternative is a record that decodes, a
/// frame that installs, badges that go up and a pane that is completely empty
/// with nothing anywhere saying why.
fn require_usable(key: &'static str, value: f32) -> Result<(), IqError> {
    if value.is_finite() {
        return Ok(());
    }
    Err(IqError::UnusableCalibration { key, value })
}

/// Name a mode this reader refuses, or `None` for one it will decode.
#[must_use]
fn refused_major_mode(code: i64) -> Option<&'static str> {
    match code {
        MAJOR_MODE_BATCH_STAGGERED_PRT => Some("batch / staggered PRT"),
        MAJOR_MODE_SZ2_PHASE_CODED => Some("SZ-2 phase coding"),
        _ => None,
    }
}

/// The geometry fields a pulse header must carry for its samples to be found.
#[derive(Clone, Copy)]
struct PulseGeometry {
    /// `iNumVecs`: samples recorded per channel, burst preamble included.
    samples_per_channel: usize,
    /// `iVIQPerBin`: channels recorded.
    channels: usize,
}

impl PulseGeometry {
    fn read(header: &text::Block<'_>) -> Result<Self, IqError> {
        let samples = header.int("iNumVecs")?;
        let channels = header.int("iVIQPerBin")?;
        if samples <= 0 {
            return Err(IqError::OutOfRange {
                key: "iNumVecs",
                value: samples,
            });
        }
        if !(1..=2).contains(&channels) {
            return Err(IqError::OutOfRange {
                key: "iVIQPerBin",
                value: channels,
            });
        }
        Ok(Self {
            samples_per_channel: samples as usize,
            channels: channels as usize,
        })
    }

    /// Bytes of packed I/Q that follow this pulse's header, or `None` when a
    /// declared `iNumVecs` is so large the count does not fit a `usize`.
    ///
    /// This is the whole stride rule: two bytes per packed value, an I and a
    /// Q per sample, `iNumVecs` samples per channel, `iVIQPerBin` channels.
    /// The multiplication is checked because `iNumVecs` is an attacker- or
    /// corruption-controlled `i64` straight out of the file: a value near
    /// 2^62 wraps in a release build and would turn a bounds check into a
    /// pass. Nothing else may be relied on to reject it first.
    fn sample_bytes(self) -> Option<usize> {
        self.samples_per_channel
            .checked_mul(self.channels)?
            .checked_mul(4)
    }

    /// Offset just past this pulse's samples.
    fn sample_bytes_end(self, data_start: usize, len: usize) -> Result<usize, IqError> {
        let needed = self.sample_bytes().ok_or(IqError::OutOfRange {
            key: "iNumVecs",
            value: self.samples_per_channel as i64,
        })?;
        let end = data_start.saturating_add(needed);
        if end > len {
            return Err(IqError::Truncated {
                what: "pulse samples",
                offset: data_start,
                needed,
                available: len.saturating_sub(data_start),
            });
        }
        Ok(end)
    }
}

/// Where a record's gates begin, how many there are, and what range each is
/// at.
///
/// Read once from the acquisition block and the FIRST pulse header, then used
/// by both [`peek_iq_time_series`] and [`decode_iq_time_series_limited`] so
/// the two can never disagree about a record.
struct GateLayout {
    /// Leading samples per channel that hold the burst rather than a gate.
    burst_samples: usize,
    /// Recorded gates per channel: `iNumVecs` less the burst preamble.
    gate_count: usize,
    /// Index into the `fRangeMaskRes` grid of every bin `iRangeMask` selects.
    mask_bins: Vec<usize>,
}

impl GateLayout {
    fn read(
        info: &text::Block<'_>,
        first: &text::Block<'_>,
        geometry: PulseGeometry,
    ) -> Result<Self, IqError> {
        let burst_samples = burst_sample_count(first)?;
        let gate_count = geometry
            .samples_per_channel
            .checked_sub(burst_samples)
            .filter(|count| *count > 0)
            .ok_or(IqError::BurstPreambleTooLong {
                burst: burst_samples,
                samples: geometry.samples_per_channel,
            })?;
        let mask_bins = range_mask_bins(info)?;
        // The burst is stored IN range bins, so the recorded samples occupy
        // `burst_samples + gate_count` mask positions, not `gate_count`.
        if burst_samples + gate_count > mask_bins.len() {
            return Err(IqError::InconsistentGeometry {
                index: 0,
                found: burst_samples + gate_count,
                expected: mask_bins.len(),
            });
        }
        Ok(Self {
            burst_samples,
            gate_count,
            mask_bins,
        })
    }

    /// Range in metres of every recorded gate, in ascending range order.
    ///
    /// The recorded samples map one-for-one onto the mask's selected bins in
    /// order, and the burst is written OVER the leading ones rather than being
    /// prepended beside them. The first gate is therefore at
    /// `mask_bins[burst_samples]`, not at `mask_bins[0]`.
    ///
    /// This is the second half of the burst trap and it is worth more than a
    /// line. Getting the burst COUNT right and this offset wrong strips the
    /// transmit samples correctly and then labels every surviving gate with
    /// the range of the gate `burst_samples` positions inward — 1 km on the
    /// reference record. The field still draws, the storm is still storm
    /// shaped, and every echo is a kilometre too close, which also mis-scales
    /// the `20 log10 r` term in its reflectivity.
    ///
    /// The evidence that the burst consumes mask positions:
    ///
    /// * NCAR LROSE names the constant
    ///   `IWRF_RVP8_NGATES_BURST 2 /* number of GATES used for storing RVP8
    ///   burst pulse */` and computes `n_gates = iNumVecs - n_gates_burst`,
    ///   i.e. the burst is stored in gates and the gate ladder is what is left
    ///   after them (`IwrfTsPulse::_deriveFromRvp8Header`).
    /// * `iMaxVecs` is exactly one more than the bits set in `iRangeMask` in
    ///   every reference record (501 of 500, 601 of 600): the acquisition
    ///   buffer is the mask plus a slot, so recorded samples are indexed by
    ///   mask position rather than sitting outside the mask.
    /// * The reference record's last gate then lands at 124.5 km against an
    ///   unambiguous range of 124.92 km — under it, and within one gate of it,
    ///   which is where a record that runs to the end of the interval should
    ///   stop. Starting the ladder at `mask_bins[0]` instead leaves the last
    ///   gate 1.4 km short of an interval the acquisition had no reason to
    ///   leave unused.
    ///
    /// # Where this differs from LROSE, and why
    ///
    /// LROSE offsets by ONE gate spacing, not by `burst_samples`:
    /// `IwrfTsInfo::_deriveRangeFromRvp8Info` computes the ladder from the
    /// mask and then does `startRangeM += gateSpacingM;` under the comment
    /// "pulse centered on PRT boundary, and first gate holds burst".
    ///
    /// That contradicts its own burst handling one file over. `IwrfTsPulse`
    /// sets `n_gates_burst` to 2, computes `n_gates = iNumVecs -
    /// n_gates_burst`, and indexes the burst samples as gates `-2` and `-1`
    /// (`for (int igate = -_hdr.n_gates_burst; ...)`) — two samples before
    /// gate 0, against a range ladder that leaves room for one. The single
    /// spacing reads as a constant written when the burst was thought to be
    /// one sample, and its comment gives two unrelated reasons for the same
    /// one gate.
    ///
    /// The rule here is the one that cannot disagree with itself: every
    /// recorded sample has a mask position, `sample k` is at
    /// `mask_bins[k] * res`, and the gates are the samples from
    /// `burst_samples` on. So this reader places gates 500 m further out than
    /// LROSE does on the reference record. Recorded here rather than left to
    /// be discovered as a discrepancy against another tool.
    fn range_bins(&self, range_mask_res_m: f32) -> Vec<f32> {
        self.mask_bins[self.burst_samples..][..self.gate_count]
            .iter()
            .map(|bin| *bin as f32 * range_mask_res_m)
            .collect()
    }
}

/// Indices of the range bins `iRangeMask` selects, in ascending range order.
///
/// The mask is a bitmap of 16-bit words, least significant bit first within
/// each word, at `fRangeMaskRes` metre resolution. It is NOT required to be
/// contiguous: `0x5555` selects alternate bins, which is how a 250 m mask
/// records 500 m gates.
fn range_mask_bins(info: &text::Block<'_>) -> Result<Vec<usize>, IqError> {
    let words = info.int_list("iRangeMask")?;
    let mut bins = Vec::new();
    for (index, word) in words.iter().enumerate() {
        if !(0..=0xFFFF).contains(word) {
            return Err(IqError::OutOfRange {
                key: "iRangeMask",
                value: *word,
            });
        }
        let word = *word as u16;
        for bit in 0..MASK_BITS_PER_WORD {
            if word >> bit & 1 == 1 {
                bins.push(index * MASK_BITS_PER_WORD + bit);
            }
        }
    }
    Ok(bins)
}

/// How many leading samples per channel are burst rather than gate.
///
/// The RVP8 reserves [`DEFAULT_BURST_SAMPLES`] of them and starts the gates
/// after. A producer that reserved a different number writes the sentinel
/// [`BURST_COUNT_DECLARED`] into `uiqOnce.iLong[0]` and the count into
/// `uiqOnce.iLong[1]`; that pair is otherwise a general-purpose per-pulse
/// scratch field and carries zeroes.
///
/// No arithmetic on `iMaxVecs` substitutes for this. `iMaxVecs` is the most
/// samples the acquisition COULD have recorded, and in every reference record
/// it happens to be one more than the bits set in `iRangeMask` — an inviting
/// coincidence, and a false one: those same records reserve TWO samples for
/// the burst. Reading the preamble off `iMaxVecs` therefore emits the
/// transmit pulse as gate 0 and shifts every real gate one recorded bin
/// outward, which is exactly the whole-field error that looks plausible on a
/// display. See the module note for what the samples themselves say.
fn burst_sample_count(header: &text::Block<'_>) -> Result<usize, IqError> {
    const KEY: &str = "uiqOnce.iLong";
    if header.get(KEY).is_none() {
        return Ok(DEFAULT_BURST_SAMPLES);
    }
    let scratch = header.int_list("uiqOnce.iLong")?;
    if scratch.first().copied() != Some(BURST_COUNT_DECLARED) {
        return Ok(DEFAULT_BURST_SAMPLES);
    }
    let Some(declared) = scratch.get(1).copied() else {
        return Ok(DEFAULT_BURST_SAMPLES);
    };
    usize::try_from(declared).map_err(|_| IqError::OutOfRange {
        key: "uiqOnce.iLong",
        value: declared,
    })
}

/// Spacing between recorded bins when it is the same everywhere.
fn uniform_spacing(range_bins: &[f32]) -> Option<f32> {
    let first = *range_bins.first()?;
    let second = *range_bins.get(1)?;
    let spacing = second - first;
    range_bins
        .windows(2)
        .all(|pair| (pair[1] - pair[0] - spacing).abs() < 1e-3)
        .then_some(spacing)
}

/// Turn one pulse header plus its decoded samples into an [`IqPulse`].
///
/// The channel split is the block one: all of channel 0 and then all of
/// channel 1, rather than a `(H, V)` pair per gate. Within a channel the
/// burst preamble leads — `burst_samples` of them — and the gates follow in
/// mask order.
fn build_pulse(
    header: &text::Block<'_>,
    geometry: &PulseGeometry,
    samples: &[f32],
    burst_samples: usize,
    clock_hz: f64,
) -> Result<IqPulse, IqError> {
    let per_channel = geometry.samples_per_channel * 2;
    let channel =
        |index: usize| -> &[f32] { &samples[index * per_channel..(index + 1) * per_channel] };

    let gates = |values: &[f32]| -> Vec<(f32, f32)> {
        values[burst_samples * 2..]
            .chunks_exact(2)
            .map(|pair| (pair[0], pair[1]))
            .collect()
    };

    let h_channel = channel(0);
    let h = gates(h_channel);
    let v = if geometry.channels == 2 {
        gates(channel(1))
    } else {
        Vec::new()
    };

    // The burst REFERENCE is the last preamble sample, not the first: it is
    // the one the processor phase-references the record to, and the one whose
    // magnitude it reports as RX[n].fBurstMag. LROSE reads the same sample,
    // as `iq[-2], iq[-1]` counted back from gate 0.
    let reference = (burst_samples.saturating_sub(1)) * 2;
    let burst = (burst_samples > 0).then(|| IqBurst {
        h: (h_channel[reference], h_channel[reference + 1]),
        v: (geometry.channels == 2).then(|| {
            let values = channel(1);
            (values[reference], values[reference + 1])
        }),
        preamble_samples: burst_samples,
        reported_magnitude: [
            header.float_or("RX[0].fBurstMag", 0.0).unwrap_or(0.0),
            header.float_or("RX[1].fBurstMag", 0.0).unwrap_or(0.0),
        ],
        reported_phase_rad: [
            binary_angle_to_radians(header.opt_int("RX[0].iBurstArg").unwrap_or(None)),
            binary_angle_to_radians(header.opt_int("RX[1].iBurstArg").unwrap_or(None)),
        ],
    });

    Ok(IqPulse {
        azimuth_deg: binary_angle_to_degrees(header.int("iAz")?).rem_euclid(360.0),
        elevation_deg: normalise_elevation(binary_angle_to_degrees(header.int("iEl")?)),
        prt_seconds: ticks_to_seconds(header.int("iNextPRT")?, clock_hz),
        prt_previous_seconds: ticks_to_seconds(header.int("iPrevPRT")?, clock_hz),
        time_utc: header.int("iTimeUTC")?,
        time_millis: header.opt_int("iMSecUTC")?.unwrap_or(0).clamp(0, 999) as u16,
        burst,
        h,
        v,
    })
}

/// A 16-bit binary angle in degrees.
fn binary_angle_to_degrees(raw: i64) -> f32 {
    raw as f32 * 360.0 / BINARY_ANGLE_FULL_CIRCLE
}

/// A 16-bit binary angle in radians, wrapped to `(-pi, pi]`.
fn binary_angle_to_radians(raw: Option<i64>) -> f32 {
    let Some(raw) = raw else { return 0.0 };
    let turns = raw as f32 / BINARY_ANGLE_FULL_CIRCLE;
    let radians = turns * std::f32::consts::TAU;
    if radians > std::f32::consts::PI {
        radians - std::f32::consts::TAU
    } else {
        radians
    }
}

/// Fold an unsigned binary elevation into `(-180, 180]`.
///
/// Elevations are written as unsigned binary angles, so a cut below the
/// horizon arrives as a value just under a full circle rather than as a
/// negative number.
fn normalise_elevation(degrees: f32) -> f32 {
    let wrapped = degrees.rem_euclid(360.0);
    if wrapped > 180.0 {
        wrapped - 360.0
    } else {
        wrapped
    }
}

/// Convert an acquisition-clock tick count to seconds.
fn ticks_to_seconds(ticks: i64, clock_hz: f64) -> f32 {
    if clock_hz <= 0.0 {
        return 0.0;
    }
    (ticks as f64 / clock_hz) as f32
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a synthetic record so the framing rules can be exercised
    /// independently of any one archive file. The real-file tests in
    /// `tests/iq_real.rs` are what prove the reader against actual radar
    /// data; these prove the edge cases that no single file exhibits.
    struct RecordBuilder {
        mask: Vec<u16>,
        max_vecs: i64,
        num_vecs: usize,
        channels: usize,
        major_mode: i64,
        pulses: usize,
        /// `uiqOnce.iLong`, the per-pulse scratch pair. `None` writes the
        /// field out as the zeroes a real record carries; `Some(n)` writes
        /// the 9999 sentinel and declares an `n`-sample burst preamble.
        declared_burst: Option<i64>,
    }

    impl Default for RecordBuilder {
        fn default() -> Self {
            Self {
                // Six contiguous bins: the burst is written over the leading
                // ones, so a record needs a mask position for every recorded
                // sample and not merely for every gate.
                mask: vec![0x003F],
                // The reference records all carry iMaxVecs = mask bits + 1.
                // It is written here for the same reason: to keep the
                // coincidence in front of the tests that must not use it.
                max_vecs: 7,
                // Two burst samples plus four gates.
                num_vecs: 6,
                channels: 2,
                major_mode: 0,
                pulses: 2,
                declared_burst: None,
            }
        }
    }

    impl RecordBuilder {
        fn build(&self) -> Vec<u8> {
            let mask = self
                .mask
                .iter()
                .map(|word| word.to_string())
                .collect::<Vec<_>>()
                .join(" ");
            let mut out = format!(
                "rvp8PulseInfo start\niVersion=4\niMajorMode={}\niPolarization=3\n\
                 taskID.sTaskName=Ascope_DEFAULT\nsSiteName=KOUN_RVP\niSampleSize=32\n\
                 fPWidthUSec=1.5\nfDBzCalib=-35.5\nfAqClkMHz=71.9364\nfWavelengthCM=11.08\n\
                 fSaturationDBM=6\nfRangeMaskRes=250\niRangeMask={mask}\n\
                 fNoiseDBm=-80.5555 -80.5955\nsVersionString=8.12.8\nrvp8PulseInfo end\n",
                self.major_mode
            )
            .into_bytes();
            let scratch = match self.declared_burst {
                None => "0 0".to_owned(),
                Some(count) => format!("{BURST_COUNT_DECLARED} {count}"),
            };
            for index in 0..self.pulses {
                out.extend_from_slice(
                    format!(
                        "rvp8PulseHdr start\niVersion=3\niTimeUTC={}\niMSecUTC=730\n\
                         iPrevPRT=59950\niNextPRT=59950\niAz=60335\niEl=728\niNumVecs={}\n\
                         iMaxVecs={}\niVIQPerBin={}\nuiqPerm.iLong=0 0\nuiqOnce.iLong={}\n\
                         RX[0].fBurstMag=0.4\nRX[0].iBurstArg=26\n\
                         rvp8PulseHdr end\n",
                        1_369_079_161 + index as i64,
                        self.num_vecs,
                        self.max_vecs,
                        self.channels,
                        scratch,
                    )
                    .as_bytes(),
                );
                // Sample bytes: a recognisable ramp so gate order is testable.
                for channel in 0..self.channels {
                    for sample in 0..self.num_vecs {
                        let code = 0x0001 + (channel * 100 + sample) as u16;
                        out.extend_from_slice(&code.to_le_bytes());
                        out.extend_from_slice(&code.to_le_bytes());
                    }
                }
            }
            out
        }
    }

    #[test]
    fn decodes_a_synthetic_record_end_to_end() {
        let raw = RecordBuilder::default().build();
        let sweep = decode_iq_time_series(&raw).unwrap();
        assert_eq!(sweep.site, "KOUN_RVP");
        assert_eq!(sweep.task_name, "Ascope_DEFAULT");
        assert_eq!(sweep.pulses.len(), 2);
        assert_eq!(sweep.channels_recorded, 2);
        // Six samples per channel, two of them the burst preamble: four
        // gates, and the mask has exactly four bits.
        assert_eq!(sweep.burst_samples, DEFAULT_BURST_SAMPLES);
        assert_eq!(sweep.gate_count(), 4);
        // The burst covers mask bins 0 and 1, so the gates are bins 2..6.
        assert_eq!(sweep.range_bins, vec![500.0, 750.0, 1000.0, 1250.0]);
        assert_eq!(sweep.gate_spacing_m, Some(250.0));
        assert_eq!(sweep.first_gate_m, 500.0);
        assert_eq!(sweep.pulses[0].h.len(), 4);
        assert_eq!(sweep.pulses[0].v.len(), 4);
        assert!((sweep.wavelength_m - 0.1108).abs() < 1e-6);
        assert!((sweep.pulse_width_s - 1.5e-6).abs() < 1e-12);
    }

    #[test]
    fn two_leading_samples_are_burst_and_the_reference_is_the_second() {
        let raw = RecordBuilder::default().build();
        let sweep = decode_iq_time_series(&raw).unwrap();
        let pulse = &sweep.pulses[0];
        let burst = pulse.burst.expect("record reserves a burst preamble");
        assert_eq!(burst.preamble_samples, 2);
        // Codes: channel 0 samples are 0x0001..0x0006. The preamble takes the
        // first TWO and the burst reference is the second of them; the four
        // gates take the rest. Taking 0x0001 as the reference, or 0x0002 as
        // gate 0, is the whole-field error this test exists for.
        assert_eq!(burst.h, (packed::unpack(0x0002), packed::unpack(0x0002)));
        assert_eq!(pulse.h[0], (packed::unpack(0x0003), packed::unpack(0x0003)));
        assert_eq!(pulse.h[3], (packed::unpack(0x0006), packed::unpack(0x0006)));
        // Channel 1 starts a whole block later, not interleaved per gate.
        assert_eq!(
            burst.v,
            Some((packed::unpack(0x0066), packed::unpack(0x0066)))
        );
        assert_eq!(pulse.v[0], (packed::unpack(0x0067), packed::unpack(0x0067)));
    }

    #[test]
    fn the_burst_preamble_is_not_read_off_i_max_vecs() {
        // iMaxVecs is one more than the four mask bits here, exactly as it is
        // in every real record. That relationship must move nothing: the
        // preamble is two samples either way, so the gate count stays four
        // and gate 0 stays the third sample.
        for max_vecs in [4, 5, 9, 501] {
            let raw = RecordBuilder {
                max_vecs,
                ..RecordBuilder::default()
            }
            .build();
            let sweep = decode_iq_time_series(&raw).unwrap();
            assert_eq!(sweep.burst_samples, 2, "iMaxVecs {max_vecs}");
            assert_eq!(sweep.gate_count(), 4, "iMaxVecs {max_vecs}");
            assert_eq!(
                sweep.pulses[0].h[0],
                (packed::unpack(0x0003), packed::unpack(0x0003)),
                "iMaxVecs {max_vecs}"
            );
        }
    }

    #[test]
    fn a_declared_burst_preamble_overrides_the_default() {
        // uiqOnce.iLong = 9999 n is the producer's way of saying the reserve
        // is n rather than two.
        for (declared, gates, first_gate_code) in [(0, 6, 0x0001), (1, 5, 0x0002), (3, 3, 0x0004)] {
            let raw = RecordBuilder {
                mask: vec![0x003F],
                declared_burst: Some(declared),
                ..RecordBuilder::default()
            }
            .build();
            let sweep = decode_iq_time_series(&raw).unwrap();
            assert_eq!(
                sweep.burst_samples, declared as usize,
                "declared {declared}"
            );
            assert_eq!(sweep.gate_count(), gates, "declared {declared}");
            assert_eq!(
                sweep.pulses[0].h[0],
                (
                    packed::unpack(first_gate_code),
                    packed::unpack(first_gate_code)
                ),
                "declared {declared}"
            );
            assert_eq!(sweep.pulses[0].burst.is_some(), declared > 0);
        }
    }

    #[test]
    fn an_alternate_bin_mask_places_gates_at_double_spacing() {
        // 0x5555 is the real KOUN case: every other bit of a 250 m mask, so
        // 500 m gates. A reader that assumed contiguous bins would put gate 1
        // at 250 m instead of 500 m and every gate after it would be wrong.
        let raw = RecordBuilder {
            mask: vec![0x5555, 0x5555],
            max_vecs: 17,
            num_vecs: 10,
            ..RecordBuilder::default()
        }
        .build();
        let sweep = decode_iq_time_series(&raw).unwrap();
        assert_eq!(sweep.gate_count(), 8);
        // Mask bins 0, 2, 4, ... at 250 m. The burst covers the first two of
        // them, so gate 0 is mask bin 4 — 1000 m — and the ladder steps 500 m.
        assert_eq!(
            sweep.range_bins,
            vec![
                1000.0, 1500.0, 2000.0, 2500.0, 3000.0, 3500.0, 4000.0, 4500.0
            ]
        );
        assert_eq!(sweep.gate_spacing_m, Some(500.0));
    }

    #[test]
    fn an_irregular_mask_still_places_every_gate_and_declines_a_single_spacing() {
        let raw = RecordBuilder {
            // Bits 0, 1, 4, 9, 10 and 15: deliberately uneven.
            mask: vec![0b1000_0110_0001_0011],
            max_vecs: 7,
            num_vecs: 6,
            ..RecordBuilder::default()
        }
        .build();
        let sweep = decode_iq_time_series(&raw).unwrap();
        // The burst covers bits 0 and 1; the gates are bits 4, 9, 10 and 15.
        assert_eq!(sweep.range_bins, vec![1000.0, 2250.0, 2500.0, 3750.0]);
        assert_eq!(sweep.gate_spacing_m, None);
    }

    #[test]
    fn a_record_with_no_burst_sample_gives_every_vector_to_a_gate() {
        let raw = RecordBuilder {
            mask: vec![0x000F],
            max_vecs: 4,
            num_vecs: 4,
            declared_burst: Some(0),
            ..RecordBuilder::default()
        }
        .build();
        let sweep = decode_iq_time_series(&raw).unwrap();
        assert_eq!(sweep.burst_samples, 0);
        assert_eq!(sweep.gate_count(), 4);
        assert!(sweep.pulses[0].burst.is_none());
        assert_eq!(
            sweep.pulses[0].h[0],
            (packed::unpack(0x0001), packed::unpack(0x0001))
        );
    }

    #[test]
    fn a_burst_preamble_that_leaves_no_gate_is_refused() {
        for (num_vecs, declared) in [(2, None), (1, None), (4, Some(4)), (4, Some(9))] {
            let raw = RecordBuilder {
                mask: vec![0x000F],
                num_vecs,
                declared_burst: declared,
                ..RecordBuilder::default()
            }
            .build();
            let error = decode_iq_time_series(&raw).unwrap_err();
            assert!(
                matches!(error, IqError::BurstPreambleTooLong { .. }),
                "iNumVecs {num_vecs} declared {declared:?}: {error}"
            );
        }
    }

    #[test]
    fn a_negative_declared_burst_preamble_is_refused() {
        let raw = RecordBuilder {
            declared_burst: Some(-1),
            ..RecordBuilder::default()
        }
        .build();
        let error = decode_iq_time_series(&raw).unwrap_err();
        assert!(matches!(error, IqError::OutOfRange { .. }), "{error}");
        assert!(error.to_string().contains("uiqOnce.iLong"), "{error}");
    }

    #[test]
    fn more_gates_than_the_range_mask_can_place_is_refused() {
        // Two mask bits but eight vectors: after the two-sample preamble the
        // record claims six gates the mask cannot give a range to.
        let raw = RecordBuilder {
            mask: vec![0x0003],
            num_vecs: 8,
            ..RecordBuilder::default()
        }
        .build();
        let error = decode_iq_time_series(&raw).unwrap_err();
        assert!(
            matches!(error, IqError::InconsistentGeometry { .. }),
            "{error}"
        );
    }

    #[test]
    fn single_channel_records_leave_the_vertical_channel_empty() {
        let raw = RecordBuilder {
            channels: 1,
            ..RecordBuilder::default()
        }
        .build();
        let sweep = decode_iq_time_series(&raw).unwrap();
        assert!(!sweep.is_dual_channel());
        assert_eq!(sweep.channels_recorded, 1);
        assert_eq!(sweep.pulses[0].h.len(), 4);
        assert!(sweep.pulses[0].v.is_empty());
        assert!(sweep.pulses[0].burst.unwrap().v.is_none());
    }

    #[test]
    fn batch_staggered_prt_is_refused_by_name() {
        let raw = RecordBuilder {
            major_mode: MAJOR_MODE_BATCH_STAGGERED_PRT,
            ..RecordBuilder::default()
        }
        .build();
        let error = decode_iq_time_series(&raw).unwrap_err();
        let message = error.to_string();
        assert!(message.contains("iMajorMode 12"), "{message}");
        assert!(message.contains("staggered PRT"), "{message}");
    }

    #[test]
    fn sz2_phase_coding_is_refused_by_name() {
        let raw = RecordBuilder {
            major_mode: MAJOR_MODE_SZ2_PHASE_CODED,
            ..RecordBuilder::default()
        }
        .build();
        let error = decode_iq_time_series(&raw).unwrap_err();
        let message = error.to_string();
        assert!(message.contains("iMajorMode 15"), "{message}");
        assert!(message.contains("SZ-2"), "{message}");
    }

    /// The same record with one header field rewritten. The header is text and
    /// the sample offsets are taken from the block terminators, so the
    /// replacement does not have to be the same length.
    fn with_field(record: &[u8], from: &str, to: &str) -> Vec<u8> {
        let at = record
            .windows(from.len())
            .position(|window| window == from.as_bytes())
            .unwrap_or_else(|| panic!("the fixture carries {from:?}"));
        let mut out = record[..at].to_vec();
        out.extend_from_slice(to.as_bytes());
        out.extend_from_slice(&record[at + from.len()..]);
        out
    }

    /// A calibration no arithmetic can use is refused by name, exactly as the
    /// unsupported major modes are.
    ///
    /// `"NaN"`, `"inf"` and `"-inf"` all parse as `f32`, so every one of these
    /// headers used to decode: a frame installed, the pane raised its LEVEL 1
    /// and COMPUTED badges and wrote the provenance line, and then produced an
    /// entirely empty pane - every moment of every gate NaN - with nothing
    /// anywhere saying the record's calibration was unusable.
    ///
    /// `fRangeMaskRes=NaN` is the sharpest of them: it makes every range bin
    /// NaN, and the sweep processor's range-uniformity check compares
    /// deviations with `>`, so a ladder of NaNs sails through a test written
    /// to catch precisely a range mask that cannot be drawn.
    #[test]
    fn a_calibration_that_is_not_a_number_is_refused_by_name() {
        let record = RecordBuilder::default().build();
        decode_iq_time_series(&record).expect("the fixture itself decodes");

        for (from, to, key) in [
            (
                "fNoiseDBm=-80.5555 -80.5955",
                "fNoiseDBm=NaN -80.5955",
                "fNoiseDBm",
            ),
            (
                "fNoiseDBm=-80.5555 -80.5955",
                "fNoiseDBm=-80.5555 -inf",
                "fNoiseDBm",
            ),
            // A single-channel record states one floor and the reader reuses
            // it for the second: an unusable one is unusable twice.
            ("fNoiseDBm=-80.5555 -80.5955", "fNoiseDBm=NaN", "fNoiseDBm"),
            ("fSaturationDBM=6", "fSaturationDBM=inf", "fSaturationDBM"),
            ("fDBzCalib=-35.5", "fDBzCalib=NaN", "fDBzCalib"),
            ("fRangeMaskRes=250", "fRangeMaskRes=NaN", "fRangeMaskRes"),
            ("fRangeMaskRes=250", "fRangeMaskRes=0", "fRangeMaskRes"),
            ("fRangeMaskRes=250", "fRangeMaskRes=-250", "fRangeMaskRes"),
        ] {
            let broken = with_field(&record, from, to);
            let message = match decode_iq_time_series(&broken) {
                Ok(sweep) => panic!(
                    "{to:?} decoded into {} pulses of {} bins instead of being refused",
                    sweep.pulses.len(),
                    sweep.range_bins.len()
                ),
                Err(error) => error.to_string(),
            };
            assert!(
                message.contains(key),
                "{to:?} was not refused by name: {message}"
            );
        }
    }

    #[test]
    fn other_major_modes_are_decoded() {
        // Mode 13 appears in the real KOUN archive alongside mode 0; only 12
        // and 15 are hostile to pulse-pair.
        for mode in [0, 1, 13, 20] {
            let raw = RecordBuilder {
                major_mode: mode,
                ..RecordBuilder::default()
            }
            .build();
            assert!(decode_iq_time_series(&raw).is_ok(), "mode {mode}");
        }
    }

    #[test]
    fn truncated_samples_are_reported_not_silently_dropped() {
        let mut raw = RecordBuilder::default().build();
        raw.truncate(raw.len() - 6);
        let error = decode_iq_time_series(&raw).unwrap_err();
        assert!(matches!(error, IqError::Truncated { .. }), "{error}");
    }

    #[test]
    fn a_pulse_that_changes_geometry_mid_record_is_refused() {
        let mut raw = RecordBuilder {
            pulses: 1,
            ..RecordBuilder::default()
        }
        .build();
        let second = RecordBuilder {
            pulses: 1,
            num_vecs: 4,
            max_vecs: 5,
            ..RecordBuilder::default()
        }
        .build();
        // Append only the second record's pulse, not its info block.
        let cut = second
            .windows(18)
            .position(|window| window == b"rvp8PulseHdr start")
            .unwrap();
        raw.extend_from_slice(&second[cut..]);
        let error = decode_iq_time_series(&raw).unwrap_err();
        assert!(
            matches!(error, IqError::InconsistentGeometry { .. }),
            "{error}"
        );
    }

    #[test]
    fn the_pulse_limit_stops_early_without_changing_calibration() {
        let raw = RecordBuilder {
            pulses: 5,
            ..RecordBuilder::default()
        }
        .build();
        let whole = decode_iq_time_series(&raw).unwrap();
        let clipped = decode_iq_time_series_limited(&raw, 2).unwrap();
        assert_eq!(whole.pulses.len(), 5);
        assert_eq!(clipped.pulses.len(), 2);
        assert_eq!(clipped.range_bins, whole.range_bins);
        assert_eq!(clipped.noise_dbm, whole.noise_dbm);
        assert_eq!(clipped.saturation_dbm, whole.saturation_dbm);
    }

    #[test]
    fn peek_counts_pulses_without_decoding_them() {
        let raw = RecordBuilder {
            pulses: 7,
            ..RecordBuilder::default()
        }
        .build();
        let summary = peek_iq_time_series(&raw).unwrap();
        assert_eq!(summary.pulse_count, 7);
        assert_eq!(summary.site, "KOUN_RVP");
        assert_eq!(summary.channels_recorded, 2);
        assert_eq!(summary.major_mode, 0);
        assert_eq!(summary.time_utc, 1_369_079_161);
        assert_eq!(summary.burst_samples, DEFAULT_BURST_SAMPLES);
        assert_eq!(summary.gate_count, 4);
    }

    #[test]
    fn peek_and_decode_never_disagree_about_a_record() {
        // A router peeks to describe a file and then decodes it. If the two
        // read the geometry differently it prints a gate count for a record
        // the decoder is about to refuse, or a different one from what it
        // returns. Both must come from the same reading.
        let cases = [
            ("default", RecordBuilder::default()),
            (
                "alternate mask",
                RecordBuilder {
                    mask: vec![0x5555],
                    num_vecs: 10,
                    ..RecordBuilder::default()
                },
            ),
            (
                "declared preamble",
                RecordBuilder {
                    declared_burst: Some(1),
                    ..RecordBuilder::default()
                },
            ),
            (
                "no preamble",
                RecordBuilder {
                    declared_burst: Some(0),
                    ..RecordBuilder::default()
                },
            ),
            (
                "preamble swallows the record",
                RecordBuilder {
                    num_vecs: 2,
                    ..RecordBuilder::default()
                },
            ),
            (
                "more gates than the mask can place",
                RecordBuilder {
                    mask: vec![0x0003],
                    num_vecs: 8,
                    ..RecordBuilder::default()
                },
            ),
            (
                "negative declared preamble",
                RecordBuilder {
                    declared_burst: Some(-1),
                    ..RecordBuilder::default()
                },
            ),
        ];
        for (name, builder) in cases {
            let raw = builder.build();
            match (peek_iq_time_series(&raw), decode_iq_time_series(&raw)) {
                (Ok(summary), Ok(sweep)) => {
                    assert_eq!(summary.gate_count, sweep.gate_count(), "{name}");
                    assert_eq!(summary.burst_samples, sweep.burst_samples, "{name}");
                    assert_eq!(summary.pulse_count, sweep.pulses.len(), "{name}");
                }
                (Err(peeked), Err(decoded)) => {
                    assert_eq!(peeked.to_string(), decoded.to_string(), "{name}");
                }
                (peeked, decoded) => panic!(
                    "{name}: peek and decode disagree — peek {:?}, decode {:?}",
                    peeked.map(|summary| summary.gate_count),
                    decoded.map(|sweep| sweep.gate_count()),
                ),
            }
        }
    }

    #[test]
    fn an_absurd_i_num_vecs_is_refused_rather_than_wrapping_the_stride() {
        // iNumVecs comes straight out of the file as an i64. Multiplied out
        // unchecked, a value near 2^62 wraps in a release build and turns the
        // truncation check into a pass.
        const DECLARED: &[u8] = b"iNumVecs=6\n";
        for num_vecs in [1i64 << 61, 1i64 << 62, i64::MAX] {
            let mut raw = RecordBuilder::default().build();
            let at = raw
                .windows(DECLARED.len())
                .position(|window| window == DECLARED)
                .expect("builder writes iNumVecs");
            raw.splice(
                at..at + DECLARED.len(),
                format!("iNumVecs={num_vecs}\n").bytes(),
            );
            let error = decode_iq_time_series(&raw).unwrap_err();
            assert!(
                matches!(
                    error,
                    IqError::OutOfRange { .. }
                        | IqError::Truncated { .. }
                        | IqError::InconsistentGeometry { .. }
                ),
                "iNumVecs {num_vecs}: {error}"
            );
            // And the stride arithmetic reports the overflow rather than
            // wrapping to a small, passing byte count.
            let geometry = PulseGeometry {
                samples_per_channel: num_vecs as usize,
                channels: 2,
            };
            assert_eq!(geometry.sample_bytes(), None, "iNumVecs {num_vecs}");
        }
    }

    #[test]
    fn derived_geometry_matches_the_prt() {
        let raw = RecordBuilder::default().build();
        let sweep = decode_iq_time_series(&raw).unwrap();
        // 59950 ticks of a 71.9364 MHz clock is 833.4 us.
        let prt = sweep.pulses[0].prt_seconds;
        assert!((prt - 833.375e-6).abs() < 1e-9, "{prt}");
        let nyquist = sweep.nyquist_velocity_m_s().unwrap();
        assert!((nyquist - 33.24).abs() < 0.02, "{nyquist}");
        let range = sweep.unambiguous_range_m().unwrap();
        assert!((range - 124_920.0).abs() < 50.0, "{range}");
        assert!(sweep.has_uniform_prt(1e-9));
    }

    #[test]
    fn noise_power_is_the_saturation_referenced_linear_floor() {
        let raw = RecordBuilder::default().build();
        let sweep = decode_iq_time_series(&raw).unwrap();
        // -80.5555 dBm against a 6 dBm saturation reference is -86.5555 dB
        // below unit magnitude squared.
        let expected = 10f32.powf(-86.5555 / 10.0);
        assert!((sweep.noise_power(0) - expected).abs() < 1e-12);
        assert!(sweep.noise_power(1) < sweep.noise_power(0));
    }

    #[test]
    fn angles_convert_from_binary_and_fold_below_the_horizon() {
        assert!((binary_angle_to_degrees(60335) - 331.43).abs() < 0.01);
        assert!((binary_angle_to_degrees(728) - 4.0).abs() < 0.01);
        assert!((normalise_elevation(binary_angle_to_degrees(65_354)) + 1.0).abs() < 0.02);
        assert_eq!(normalise_elevation(4.0), 4.0);
    }

    #[test]
    fn the_sniff_accepts_both_spellings_and_rejects_archive_two() {
        assert!(looks_like_iq_time_series(b"rvp8PulseInfo start\n"));
        assert!(looks_like_iq_time_series(b"rvptsPulseInfo start\n"));
        assert!(!looks_like_iq_time_series(b"AR2V0006.473"));
        assert!(!looks_like_iq_time_series(b"\x89HDF\r\n\x1a\n"));
        assert!(!looks_like_iq_time_series(b""));
    }

    #[test]
    fn non_time_series_bytes_are_rejected_with_a_readable_error() {
        let error = decode_iq_time_series(b"AR2V0006.473 not a time series").unwrap_err();
        assert!(matches!(error, IqError::MissingBlock { .. }), "{error}");
    }
}

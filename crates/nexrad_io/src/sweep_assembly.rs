//! Conservative assembly of one-cut Archive II research-radar exports.
//!
//! GR2Analyst-style `.msg31` exports can store one antenna sweep per file even
//! though the Archive II volume-header extension still carries the acquisition
//! system's three-digit volume sequence.  A vertical product needs the sweeps
//! back in one [`RadarVolume`], but proximity in a directory or similarity in a
//! filename is not evidence that two mobile-radar files share a scan.
//!
//! This module therefore admits a member only when the bytes and the decoded
//! sweep agree on all of the facts that identify it: Archive II family and
//! volume sequence, radar id, exact recorded position, UTC day, VCP, one cut,
//! and monotonic radial time.  Joining two admitted members additionally
//! requires those keys to match and their acquisition times to be ordered and
//! no more than fifteen minutes apart.  Anything weaker is a typed refusal and
//! remains an independent frame.

use chrono::NaiveDate;
use radar_core::RadarVolume;

/// A deployment pause is not part of one logical volume, even if a writer
/// accidentally reuses its three-digit sequence number afterwards.
const MAX_INTRA_VOLUME_GAP_MS: i32 = 15 * 60 * 1_000;
const MILLISECONDS_PER_DAY: i32 = 24 * 60 * 60 * 1_000;

/// Exact radar position recorded in the Message 31 volume-constant block.
///
/// Bits, rather than rounded floats, make the grouping rule literal: two
/// mobile positions that merely look alike on screen are not silently merged.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RecordedRadarPosition {
    pub latitude_bits: u32,
    pub longitude_bits: u32,
    pub elevation_bits: u32,
}

/// Internal identity shared by sweepfiles that provably belong to one source
/// volume.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ArchiveVolumeKey {
    /// The nine-byte Archive II tape identifier, for example `AR2V0002.`.
    pub archive_family: String,
    /// Numeric bytes 9..12 of the Archive II volume header.
    pub volume_sequence: u16,
    pub site_id: String,
    pub position: RecordedRadarPosition,
    pub utc_date: NaiveDate,
    pub vcp: Option<u16>,
}

/// Evidence carried by one admitted sweep, or by the admitted chain after
/// more sweeps have been appended.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProvenSweepMembership {
    pub key: ArchiveVolumeKey,
    /// Milliseconds after UTC midnight from the Message 31 radial headers.
    pub first_radial_ms: i32,
    pub last_radial_ms: i32,
    /// Recorded cut facts retained for audit and status. They are deliberately
    /// not part of the volume key: DOW deployments can repeat a low tilt or
    /// scan tilts out of elevation order inside one explicit source volume.
    /// Inferring a boundary from ascending angle would contradict the stronger
    /// internal volume sequence on those files.
    pub elevation_number: Option<u8>,
    pub elevation_angle_bits: u32,
    pub member_count: usize,
}

/// Why a file cannot participate in automatic logical-volume assembly.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SweepAssemblyRefusal {
    NotPlainArchiveIi,
    MissingInternalVolumeSequence,
    HeaderSiteMismatch,
    MissingSite,
    MissingPosition,
    NotOneCut { cuts: usize },
    EmptySweep,
    InvalidElevation,
    InvalidRadialTime,
    NonMonotonicRadialTime,
    DifferentArchiveFamily,
    DifferentVolumeSequence,
    DifferentSite,
    DifferentPosition,
    DifferentUtcDate,
    DifferentVcp,
    OverlappingOrOutOfOrder,
    TimeGapTooLarge,
}

impl SweepAssemblyRefusal {
    /// Concise operator-facing explanation for a safe frame boundary.
    pub const fn label(&self) -> &'static str {
        match self {
            Self::NotPlainArchiveIi => "not a plain Archive II sweep export",
            Self::MissingInternalVolumeSequence => "no internal Archive II volume sequence",
            Self::HeaderSiteMismatch => "header and decoded radar ids disagree",
            Self::MissingSite => "no recorded radar id",
            Self::MissingPosition => "no complete recorded radar position",
            Self::NotOneCut { .. } => "already a complete multi-cut volume",
            Self::EmptySweep => "sweep contains no radials",
            Self::InvalidElevation => "sweep elevation is invalid",
            Self::InvalidRadialTime => "radial clock is outside one UTC day",
            Self::NonMonotonicRadialTime => "radial clock runs backwards",
            Self::DifferentArchiveFamily => "Archive II families differ",
            Self::DifferentVolumeSequence => "internal volume sequences differ",
            Self::DifferentSite => "radar ids differ",
            Self::DifferentPosition => "recorded radar positions differ",
            Self::DifferentUtcDate => "UTC acquisition days differ",
            Self::DifferentVcp => "recorded scan patterns differ",
            Self::OverlappingOrOutOfOrder => "sweep times overlap or run backwards",
            Self::TimeGapTooLarge => "sweeps are more than 15 minutes apart",
        }
    }

    /// Ordinary containers and already-complete volumes need no playlist
    /// warning; every other refusal describes an Archive II candidate that an
    /// analyst may reasonably have expected to assemble.
    pub const fn should_report_for_playlist(&self) -> bool {
        !matches!(self, Self::NotPlainArchiveIi | Self::NotOneCut { .. })
    }
}

/// Classification is explicit so a caller cannot confuse absence of evidence
/// with evidence that two files are unrelated.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SweepAssemblyClassification {
    Proven(ProvenSweepMembership),
    Refused(SweepAssemblyRefusal),
}

/// Whether two adjacent admitted members may be joined.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SweepAssemblyDecision {
    ProvenSameVolume,
    Refused(SweepAssemblyRefusal),
}

/// Classify a decoded file using its internal Archive II and Message 31 facts.
///
/// Filenames are deliberately not an argument.  If the three numeric volume
/// bytes are absent from the file itself, this returns a refusal rather than
/// treating a `vNNN` filename token as acquisition metadata.
pub fn classify_archive_sweep(raw: &[u8], volume: &RadarVolume) -> SweepAssemblyClassification {
    if raw.len() < super::VOLUME_HEADER_LEN {
        return SweepAssemblyClassification::Refused(SweepAssemblyRefusal::NotPlainArchiveIi);
    }
    let archive_family = super::ascii_trim(&raw[..9]);
    if !(archive_family.starts_with("AR2V") || archive_family == "ARCHIVE2.") {
        return SweepAssemblyClassification::Refused(SweepAssemblyRefusal::NotPlainArchiveIi);
    }
    let sequence_bytes = &raw[9..12];
    if !sequence_bytes.iter().all(u8::is_ascii_digit) {
        return SweepAssemblyClassification::Refused(
            SweepAssemblyRefusal::MissingInternalVolumeSequence,
        );
    }
    let volume_sequence = sequence_bytes
        .iter()
        .fold(0u16, |number, digit| number * 10 + u16::from(*digit - b'0'));
    // NOAA/NWS ROC ICD 2620010J section 7.3.3 defines this field as starting
    // at 001, increasing through 999, and then rolling over.  `000` therefore
    // cannot prove volume identity; treat a writer's all-zero sentinel exactly
    // like absent sequence metadata rather than joining unrelated sweeps.
    // https://www.roc.noaa.gov/public-documents/icds/2620010J.pdf
    if volume_sequence == 0 {
        return SweepAssemblyClassification::Refused(
            SweepAssemblyRefusal::MissingInternalVolumeSequence,
        );
    }

    let header_site = super::ascii_trim(&raw[20..24]).to_ascii_uppercase();
    let site_id = volume.site.id.trim().to_ascii_uppercase();
    if site_id.is_empty() {
        return SweepAssemblyClassification::Refused(SweepAssemblyRefusal::MissingSite);
    }
    if header_site.is_empty() || header_site != site_id {
        return SweepAssemblyClassification::Refused(SweepAssemblyRefusal::HeaderSiteMismatch);
    }

    let (Some(latitude), Some(longitude), Some(elevation)) = (
        volume.site.latitude_deg,
        volume.site.longitude_deg,
        volume.site.elevation_m,
    ) else {
        return SweepAssemblyClassification::Refused(SweepAssemblyRefusal::MissingPosition);
    };
    if !latitude.is_finite() || !longitude.is_finite() || !elevation.is_finite() {
        return SweepAssemblyClassification::Refused(SweepAssemblyRefusal::MissingPosition);
    }

    if volume.cuts.len() != 1 {
        return SweepAssemblyClassification::Refused(SweepAssemblyRefusal::NotOneCut {
            cuts: volume.cuts.len(),
        });
    }
    let cut = &volume.cuts[0];
    if cut.radials.is_empty() {
        return SweepAssemblyClassification::Refused(SweepAssemblyRefusal::EmptySweep);
    }
    if !cut.elevation_deg.is_finite() {
        return SweepAssemblyClassification::Refused(SweepAssemblyRefusal::InvalidElevation);
    }
    if !cut
        .radials
        .iter()
        .all(|radial| (0..MILLISECONDS_PER_DAY).contains(&radial.time_offset_ms))
    {
        return SweepAssemblyClassification::Refused(SweepAssemblyRefusal::InvalidRadialTime);
    }
    if !cut
        .radials
        .windows(2)
        .all(|pair| pair[0].time_offset_ms <= pair[1].time_offset_ms)
    {
        return SweepAssemblyClassification::Refused(SweepAssemblyRefusal::NonMonotonicRadialTime);
    }
    let first_radial_ms = cut
        .radials
        .first()
        .expect("an empty sweep returned above")
        .time_offset_ms;
    let last_radial_ms = cut
        .radials
        .last()
        .expect("an empty sweep returned above")
        .time_offset_ms;

    SweepAssemblyClassification::Proven(ProvenSweepMembership {
        key: ArchiveVolumeKey {
            archive_family,
            volume_sequence,
            site_id,
            position: RecordedRadarPosition {
                latitude_bits: latitude.to_bits(),
                longitude_bits: longitude.to_bits(),
                elevation_bits: elevation.to_bits(),
            },
            utc_date: volume.volume_time.date_naive(),
            vcp: volume.vcp.as_ref().map(|vcp| vcp.pattern),
        },
        first_radial_ms,
        last_radial_ms,
        elevation_number: cut.elevation_number,
        elevation_angle_bits: cut.elevation_deg.to_bits(),
        member_count: 1,
    })
}

/// Decide whether `next` is the next sweep of `current`'s logical volume.
pub fn decide_adjacent_sweeps(
    current: &ProvenSweepMembership,
    next: &ProvenSweepMembership,
) -> SweepAssemblyDecision {
    // Elevation is intentionally not compared here. The source volume number
    // is explicit, while an ascending-tilt convention is not: real mobile
    // volumes contain repeated low scans and descending pairs. Time still has
    // to advance, so duplicate or reordered files cannot hide behind that id.
    let refusal = if current.key.archive_family != next.key.archive_family {
        Some(SweepAssemblyRefusal::DifferentArchiveFamily)
    } else if current.key.volume_sequence != next.key.volume_sequence {
        Some(SweepAssemblyRefusal::DifferentVolumeSequence)
    } else if current.key.site_id != next.key.site_id {
        Some(SweepAssemblyRefusal::DifferentSite)
    } else if current.key.position != next.key.position {
        Some(SweepAssemblyRefusal::DifferentPosition)
    } else if current.key.utc_date != next.key.utc_date {
        Some(SweepAssemblyRefusal::DifferentUtcDate)
    } else if current.key.vcp != next.key.vcp {
        Some(SweepAssemblyRefusal::DifferentVcp)
    } else if next.first_radial_ms < current.last_radial_ms {
        Some(SweepAssemblyRefusal::OverlappingOrOutOfOrder)
    } else if next.first_radial_ms - current.last_radial_ms > MAX_INTRA_VOLUME_GAP_MS {
        Some(SweepAssemblyRefusal::TimeGapTooLarge)
    } else {
        None
    };
    refusal.map_or(
        SweepAssemblyDecision::ProvenSameVolume,
        SweepAssemblyDecision::Refused,
    )
}

/// Append one admitted sweep after re-checking the typed evidence.
///
/// The move preserves the compact gate arrays.  Radial times are already UTC
/// midnight offsets in both source volumes, and same-day membership is part of
/// the key, so no timestamp rebasing or gate-data copy is required.
pub fn append_proven_sweep(
    target: &mut RadarVolume,
    current: &mut ProvenSweepMembership,
    mut incoming: RadarVolume,
    next: ProvenSweepMembership,
) -> Result<(), SweepAssemblyRefusal> {
    match decide_adjacent_sweeps(current, &next) {
        SweepAssemblyDecision::ProvenSameVolume => {}
        SweepAssemblyDecision::Refused(reason) => return Err(reason),
    }

    target.volume_time = target.volume_time.min(incoming.volume_time);
    target.cuts.append(&mut incoming.cuts);
    target.metadata.message_count = target
        .metadata
        .message_count
        .saturating_add(incoming.metadata.message_count);
    target.metadata.decoded_radial_count = target
        .metadata
        .decoded_radial_count
        .saturating_add(incoming.metadata.decoded_radial_count);
    target.metadata.skipped_message_count = target
        .metadata
        .skipped_message_count
        .saturating_add(incoming.metadata.skipped_message_count);

    current.last_radial_ms = next.last_radial_ms;
    current.elevation_number = next.elevation_number;
    current.elevation_angle_bits = next.elevation_angle_bits;
    current.member_count = current.member_count.saturating_add(next.member_count);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};
    use radar_core::{ElevationCut, RadarSite, Radial};

    fn position() -> RecordedRadarPosition {
        RecordedRadarPosition {
            latitude_bits: 39.7278f32.to_bits(),
            longitude_bits: (-101.5425f32).to_bits(),
            elevation_bits: 1_020.0f32.to_bits(),
        }
    }

    fn membership(sequence: u16, site: &str, first: i32, last: i32) -> ProvenSweepMembership {
        ProvenSweepMembership {
            key: ArchiveVolumeKey {
                archive_family: "AR2V0002.".to_owned(),
                volume_sequence: sequence,
                site_id: site.to_owned(),
                position: position(),
                utc_date: Utc
                    .with_ymd_and_hms(2026, 5, 16, 0, 0, 0)
                    .unwrap()
                    .date_naive(),
                vcp: None,
            },
            first_radial_ms: first,
            last_radial_ms: last,
            elevation_number: Some(1),
            elevation_angle_bits: 1.0f32.to_bits(),
            member_count: 1,
        }
    }

    fn volume(elevation: f32, first: i32, last: i32) -> RadarVolume {
        let mut site = RadarSite::new("DOW7");
        site.latitude_deg = Some(39.7278);
        site.longitude_deg = Some(-101.5425);
        site.elevation_m = Some(1_020.0);
        let mut volume =
            RadarVolume::new(site, Utc.with_ymd_and_hms(2026, 5, 16, 22, 8, 11).unwrap());
        volume.vcp = None;
        let mut cut = ElevationCut::new(elevation, Some(1));
        for time_offset_ms in [first, last] {
            cut.radials.push(Radial {
                azimuth_deg: 0.0,
                elevation_deg: elevation,
                time_offset_ms,
                gate_range: radar_core::GateRange {
                    first_gate_m: 0,
                    gate_spacing_m: 150,
                    gate_count: 1,
                },
                nyquist_velocity_mps: None,
                radial_status: None,
            });
        }
        volume.cuts.push(cut);
        volume.metadata.message_count = 2;
        volume.metadata.decoded_radial_count = 2;
        volume
    }

    #[test]
    fn classification_reads_the_internal_sequence_not_a_filename() {
        let mut raw = crate::tests::synthetic_variable_framed_archive(3);
        raw[9..12].copy_from_slice(b"207");
        let volume = crate::decode_volume_from_bytes(&raw).expect("synthetic sweep decodes");

        let SweepAssemblyClassification::Proven(evidence) = classify_archive_sweep(&raw, &volume)
        else {
            panic!("numeric internal Archive II sequence should be admitted")
        };
        assert_eq!(evidence.key.volume_sequence, 207);
        assert_eq!(evidence.key.site_id, "KTLX");

        raw[9..12].copy_from_slice(b"x07");
        assert_eq!(
            classify_archive_sweep(&raw, &volume),
            SweepAssemblyClassification::Refused(
                SweepAssemblyRefusal::MissingInternalVolumeSequence
            )
        );

        raw[9..12].copy_from_slice(b"000");
        assert_eq!(
            classify_archive_sweep(&raw, &volume),
            SweepAssemblyClassification::Refused(
                SweepAssemblyRefusal::MissingInternalVolumeSequence
            )
        );
    }

    #[test]
    fn only_exact_internal_identity_and_order_can_join() {
        let current = membership(210, "DOW7", 79_691_322, 79_700_964);
        let next = membership(210, "DOW7", 79_700_964, 79_711_000);
        assert_eq!(
            decide_adjacent_sweeps(&current, &next),
            SweepAssemblyDecision::ProvenSameVolume
        );

        let mut moved = next.clone();
        moved.key.position.longitude_bits = (-101.54f32).to_bits();
        assert_eq!(
            decide_adjacent_sweeps(&current, &moved),
            SweepAssemblyDecision::Refused(SweepAssemblyRefusal::DifferentPosition)
        );

        let overlap = membership(210, "DOW7", 79_699_000, 79_712_000);
        assert_eq!(
            decide_adjacent_sweeps(&current, &overlap),
            SweepAssemblyDecision::Refused(SweepAssemblyRefusal::OverlappingOrOutOfOrder)
        );

        let delayed = membership(210, "DOW7", 80_601_000, 80_612_000);
        assert_eq!(
            decide_adjacent_sweeps(&current, &delayed),
            SweepAssemblyDecision::Refused(SweepAssemblyRefusal::TimeGapTooLarge)
        );
    }

    #[test]
    fn append_moves_cuts_and_sums_decode_bookkeeping() {
        let mut assembled = volume(0.9, 79_691_322, 79_700_000);
        let second = volume(1.3, 79_700_000, 79_711_000);
        let mut current = membership(210, "DOW7", 79_691_322, 79_700_000);
        let next = membership(210, "DOW7", 79_700_000, 79_711_000);

        append_proven_sweep(&mut assembled, &mut current, second, next).unwrap();

        assert_eq!(assembled.cuts.len(), 2);
        assert_eq!(assembled.metadata.decoded_radial_count, 4);
        assert_eq!(assembled.metadata.message_count, 4);
        assert_eq!(current.member_count, 2);
    }

    #[test]
    fn a_complete_multi_cut_volume_is_not_an_assembly_member() {
        let mut raw = crate::tests::synthetic_variable_framed_archive(3);
        raw[9..12].copy_from_slice(b"207");
        let mut volume = crate::decode_volume_from_bytes(&raw).unwrap();
        volume.cuts.push(volume.cuts[0].clone());
        assert_eq!(
            classify_archive_sweep(&raw, &volume),
            SweepAssemblyClassification::Refused(SweepAssemblyRefusal::NotOneCut { cuts: 2 })
        );
    }

    #[test]
    fn vcp_is_part_of_the_internal_identity() {
        let current = membership(210, "DOW7", 1_000, 2_000);
        let mut next = membership(210, "DOW7", 2_000, 3_000);
        next.key.vcp = Some(212);
        assert_eq!(
            decide_adjacent_sweeps(&current, &next),
            SweepAssemblyDecision::Refused(SweepAssemblyRefusal::DifferentVcp)
        );
    }
}

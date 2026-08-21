//! Golden-fixture test for the two censoring facts the NEXRAD generic data
//! moment header carries: the SNR THRESHOLD the operational processor
//! censored a moment at, and the CONTROL FLAGS byte that says whether it
//! recombined the sweep (NEXRAD ICD 2620002W, Build 22.0, 05 June 2023,
//! Table XVII-B, "Data Block (Descriptor of Generic Data Moment Type)",
//! bytes 16-17 and byte 18).
//!
//! Fixture provenance: `tests/data/KDVN20260819_192802_V06.rec0_1_7_79` is
//! REAL Archive II, KDVN (Davenport, Iowa) 2026-08-19 19:28:02.991 UTC,
//! VCP 212, fetched 2026-08-19 from the public NEXRAD Level II archive. The
//! whole volume is 11 MB of 83 LDM records, 9,720 message 31 radials and
//! 46,440 moment blocks; four of its records are kept here verbatim, byte for
//! byte, with the 24-byte tape header in front of them:
//!
//! - record 0, the 134-record metadata block, so VCP 212 and the site
//!   coordinates decode exactly as they do from the whole file;
//! - record 1, the first 120 radials of elevation 1 - the contiguous
//!   surveillance half of the lowest split cut, REF/ZDR/PHI/RHO/CFP;
//! - record 7, the first 120 radials of elevation 2 - the Doppler half of
//!   that same split cut, REF/VEL/SW;
//! - record 79, 120 radials of elevation 16 - a batch cut collected natively
//!   at 1.0 degree azimuth.
//!
//! The expected values below were extracted from the whole 11 MB volume with
//! an independent Python reader, not with this crate. Across all 46,440
//! moment blocks the SNR THRESHOLD halfword takes exactly two values, raw 16
//! on every contiguous-surveillance cut (elevations 1, 3, 5, 9, 11) and raw
//! 28 on every Doppler and batch cut (all the rest), and the CONTROL FLAGS
//! byte is 0 on every single block. The three records kept here reproduce
//! both thresholds and the third case that matters: elevation 16 is a
//! 1.0-degree sweep that still reports flag 0, because one-degree collection
//! is not recombination. Nothing in this volume - or in this fixture - can
//! exercise flags 1, 2 or 3; those are pinned by a unit test in `radar_core`.

use radar_core::{MomentRecombination, MomentType};

const KDVN: &[u8] = include_bytes!("data/KDVN20260819_192802_V06.rec0_1_7_79");

/// The censoring facts for one moment of one cut, or a panic naming what was
/// missing - a silent `None` here would let the decoder stop reading the
/// fields without the test noticing.
fn censoring(
    volume: &radar_core::RadarVolume,
    cut_index: usize,
    moment: MomentType,
) -> (f32, MomentRecombination) {
    let cut = volume
        .cuts
        .get(cut_index)
        .unwrap_or_else(|| panic!("cut {cut_index} decoded"));
    let grid = cut
        .moments
        .get(&moment)
        .unwrap_or_else(|| panic!("{moment} present on cut {cut_index}"));
    (
        grid.snr_threshold_db
            .unwrap_or_else(|| panic!("{moment} SNR threshold on cut {cut_index}")),
        grid.recombination
            .unwrap_or_else(|| panic!("{moment} control flags on cut {cut_index}")),
    )
}

#[test]
fn real_level2_carries_its_snr_thresholds_and_control_flags() {
    let volume = nexrad_io::decode_volume_from_bytes(KDVN).expect("decode KDVN fixture");

    assert_eq!(volume.site.id, "KDVN");
    assert_eq!(volume.vcp.as_ref().map(|vcp| vcp.pattern), Some(212));
    assert_eq!(volume.cuts.len(), 3);
    assert_eq!(volume.metadata.decoded_radial_count, 360);

    // Cut 0: elevation 1, the contiguous surveillance half of the lowest
    // split cut. Raw 16 counts at 0.125 dB per count is 2.0 dB, the value the
    // ICD gives as typical - and the reason 0.125 is the right scale, since
    // the alternative in that table cell would make this 1.6 dB.
    for moment in [
        MomentType::Reflectivity,
        MomentType::DifferentialReflectivity,
        MomentType::DifferentialPhase,
        MomentType::CorrelationCoefficient,
        MomentType::Unknown("CFP".to_owned()),
    ] {
        assert_eq!(
            censoring(&volume, 0, moment.clone()),
            (2.0, MomentRecombination::None),
            "cut 0 {moment}"
        );
    }

    // Cut 1: elevation 2, the Doppler half of that same split cut, censored
    // harder. Raw 28 is 3.5 dB. The split-cut structure is visible in the
    // censoring itself, which is the whole point of showing the number.
    for moment in [
        MomentType::Reflectivity,
        MomentType::Velocity,
        MomentType::SpectrumWidth,
    ] {
        assert_eq!(
            censoring(&volume, 1, moment.clone()),
            (3.5, MomentRecombination::None),
            "cut 1 {moment}"
        );
    }

    // Cut 2: elevation 16, a batch cut the radar collected at 1.0 degree
    // azimuth. It reports flag 0 all the same. A coarse sweep is not a
    // recombined one, and inferring recombination from azimuth spacing would
    // report a resolution loss that never happened.
    assert_eq!(volume.cuts[2].radials.len(), 120);
    let azimuth_step_deg =
        (volume.cuts[2].radials[1].azimuth_deg - volume.cuts[2].radials[0].azimuth_deg).abs();
    assert!(
        (azimuth_step_deg - 1.0).abs() < 0.15,
        "elevation 16 is a 1.0-degree sweep, got {azimuth_step_deg} degree steps"
    );
    for moment in [
        MomentType::Reflectivity,
        MomentType::Velocity,
        MomentType::SpectrumWidth,
        MomentType::DifferentialReflectivity,
        MomentType::DifferentialPhase,
        MomentType::CorrelationCoefficient,
        MomentType::Unknown("CFP".to_owned()),
    ] {
        assert_eq!(
            censoring(&volume, 2, moment.clone()),
            (3.5, MomentRecombination::None),
            "cut 2 {moment}"
        );
    }

    // No sweep in this volume was recombined, so nothing may claim a
    // resolution loss.
    for (index, cut) in volume.cuts.iter().enumerate() {
        for (moment, grid) in &cut.moments {
            assert_eq!(
                grid.recombination,
                Some(MomentRecombination::None),
                "cut {index} {moment}"
            );
            assert!(
                !grid
                    .recombination
                    .expect("control flags decoded")
                    .reduces_resolution(),
                "cut {index} {moment} must not report reduced resolution"
            );
        }
    }
}

/// Reading two more fields out of the moment header must not disturb the
/// fields that were already read, on either word size. The 16-bit moments
/// here matter most: SNR THRESHOLD sits two bytes before the word size byte,
/// so an off-by-one in the new reads would show up as a mis-sized moment.
#[test]
fn real_level2_moment_geometry_is_unchanged() {
    let volume = nexrad_io::decode_volume_from_bytes(KDVN).expect("decode KDVN fixture");

    let reflectivity = volume.cuts[0]
        .moments
        .get(&MomentType::Reflectivity)
        .expect("REF on cut 0");
    assert_eq!(reflectivity.gate_range.gate_count, 1832);
    assert_eq!(reflectivity.gate_range.first_gate_m, 2125);
    assert_eq!(reflectivity.gate_range.gate_spacing_m, 250);
    assert_eq!(reflectivity.scale, 2.0);
    assert_eq!(reflectivity.offset, 66.0);
    assert!(matches!(
        reflectivity.storage,
        radar_core::MomentStorage::U8(_)
    ));

    let phase = volume.cuts[0]
        .moments
        .get(&MomentType::DifferentialPhase)
        .expect("PHI on cut 0");
    assert_eq!(phase.gate_range.gate_count, 1192);
    assert_eq!(phase.offset, 2.0);
    assert!(matches!(phase.storage, radar_core::MomentStorage::U16(_)));
}

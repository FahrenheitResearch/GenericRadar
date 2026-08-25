//! Product-aware selection of newly arriving live radar sweeps.
//!
//! The network poll cadence and the minimum time between accepted sweeps are
//! intentionally unrelated. Polling can remain fast enough to notice a new
//! chunk while this policy decides whether the picture should actually move.

use std::time::Duration;

use chrono::{DateTime, TimeDelta, Utc};
use product_engine::{CutSelectionPolicy, VolumeCapabilities};
use radar_core::{MomentType, RadarVolume};

/// A two-degree full-circle sweep is coarse but still a defensible research
/// scan. Requiring 360 or 720 rays would incorrectly exclude terminal radars
/// and acquisition systems with coarser or irregular azimuth spacing.
const MIN_COMPLETE_SWEEP_RADIALS: usize = 180;
/// A live NEXRAD chunk normally carries about 240 rays. Thirty-two gives the
/// existing sweep animator a real, usable leading wedge without moving a pane
/// to a handful of spokes or to a moment that has not arrived yet.
const MIN_PARTIAL_SWEEP_RADIALS: usize = 32;
/// Duplicated azimuths are not an advancing sweep even when their row count is
/// large. Ten degrees is deliberately earlier than a normal half-degree chunk.
const MIN_PARTIAL_SWEEP_AZIMUTH_COVERAGE_DEG: f32 = 10.0;
const MILLISECONDS_PER_DAY: i32 = 86_400_000;

/// The analyst's independent controls for following usable incoming sweeps.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct LiveFollowPolicy {
    pub(crate) enabled: bool,
    pub(crate) max_elevation_deg: f32,
    pub(crate) min_interval: Duration,
}

impl Default for LiveFollowPolicy {
    fn default() -> Self {
        Self {
            enabled: false,
            max_elevation_deg: 1.4,
            min_interval: Duration::from_secs(30),
        }
    }
}

/// One actual sweep, identified by its position and measured collection time.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct LiveFollowCandidate {
    pub(crate) cut_index: usize,
    pub(crate) scan_time: DateTime<Utc>,
}

/// Find the newest usable, product-compatible sweep allowed by `policy`.
///
/// `capabilities` must describe `volume`. The caller already measures those
/// capabilities for rendering, so this function never rescans or re-sorts a
/// sweep's radials merely to make a follow decision.
///
/// `required_moment` and `cut_policy` keep the surveillance and Doppler legs of
/// a split cut distinct: a newer short-range Doppler sweep cannot take a
/// reflectivity pane away from its scientifically correct surveillance leg.
///
/// A partially arrived sweep is eligible once it has enough angular coverage
/// and product rows to draw honestly. Selecting it while it is still growing is
/// what lets the existing sweep animator reveal real incoming radials over the
/// previous completed image; waiting until `complete` would bypass that entire
/// animation.
pub(crate) fn newest_eligible_cut(
    volume: &RadarVolume,
    capabilities: &VolumeCapabilities,
    required_moment: &MomentType,
    cut_policy: CutSelectionPolicy,
    policy: LiveFollowPolicy,
    last_followed_scan: Option<DateTime<Utc>>,
) -> Option<LiveFollowCandidate> {
    if !policy.enabled || !policy.max_elevation_deg.is_finite() || policy.max_elevation_deg < 0.0 {
        return None;
    }

    let min_interval = TimeDelta::from_std(policy.min_interval).ok()?;

    capabilities
        .groups
        .iter()
        .enumerate()
        .filter(|(_, group)| {
            // A commanded-elevation group can straddle the user's ceiling by a
            // few hundredths; its median must not hide an eligible real sweep.
            group.members.iter().any(|index| {
                capabilities.cut(*index).is_some_and(|measured| {
                    measured.nominal_elevation_deg.is_finite()
                        && measured.nominal_elevation_deg <= policy.max_elevation_deg
                })
            })
        })
        .filter_map(|(group_index, group)| {
            let preferred = product_engine::cut_selection::select_in_group(
                capabilities,
                group_index,
                required_moment,
                cut_policy,
            )?;

            group
                .members
                .iter()
                .filter_map(|index| capabilities.cut(*index))
                .filter(|measured| {
                    measured.leg == preferred.leg
                        && has_usable_sweep_geometry(measured)
                        && measured.nominal_elevation_deg.is_finite()
                        && measured.nominal_elevation_deg <= policy.max_elevation_deg
                        && measured.has_moment(required_moment)
                })
                .filter_map(|measured| {
                    let actual = volume.cuts.get(measured.index)?;
                    // An older growing-volume snapshot must not accidentally
                    // select a differently sized replacement sweep.
                    if actual.radials.len() != measured.radial_count
                        || actual.moments.get(required_moment).is_none_or(|grid| {
                            grid.radial_count()
                                < MIN_PARTIAL_SWEEP_RADIALS.max(
                                    MIN_COMPLETE_SWEEP_RADIALS
                                        .min((measured.radial_count / 2).max(1)),
                                )
                        })
                    {
                        return None;
                    }

                    let scan_time = measured_scan_time(volume, measured.median_radial_time_ms)?;
                    if last_followed_scan.as_ref().is_some_and(|previous| {
                        let elapsed = scan_time.signed_duration_since(*previous);
                        elapsed <= TimeDelta::zero() || elapsed < min_interval
                    }) {
                        return None;
                    }

                    Some(LiveFollowCandidate {
                        cut_index: measured.index,
                        scan_time,
                    })
                })
                .max_by(|left, right| {
                    left.scan_time
                        .cmp(&right.scan_time)
                        .then_with(|| left.cut_index.cmp(&right.cut_index))
                })
        })
        // Equal timestamps can occur on split cuts and on research formats
        // that know only a sweep start. File order makes the answer stable.
        .max_by(|left, right| {
            left.scan_time
                .cmp(&right.scan_time)
                .then_with(|| left.cut_index.cmp(&right.cut_index))
        })
}

fn has_usable_sweep_geometry(measured: &product_engine::CutCapabilities) -> bool {
    if measured.complete {
        measured.radial_count >= MIN_COMPLETE_SWEEP_RADIALS
    } else {
        measured.radial_count >= MIN_PARTIAL_SWEEP_RADIALS
            && measured.azimuth_coverage_deg.is_finite()
            && measured.azimuth_coverage_deg >= MIN_PARTIAL_SWEEP_AZIMUTH_COVERAGE_DEG
    }
}

/// NEXRAD's `collect_ms` is a UTC time-of-day, while ODIM, CfRadial, DORADE
/// and I/Q decoders store offsets relative to `volume_time`. Several research
/// formats also populate `archive_version`, so only an actual NEXRAD Archive
/// II signature is evidence of its time-of-day convention.
fn measured_scan_time(volume: &RadarVolume, offset_ms: i32) -> Option<DateTime<Utc>> {
    let offset = TimeDelta::milliseconds(i64::from(offset_ms));
    let is_nexrad_archive = volume
        .metadata
        .archive_version
        .as_deref()
        .is_some_and(|version| {
            let version = version.trim_start();
            version
                .get(..4)
                .is_some_and(|prefix| prefix.eq_ignore_ascii_case("AR2V"))
                || version
                    .get(..8)
                    .is_some_and(|prefix| prefix.eq_ignore_ascii_case("ARCHIVE2"))
        });
    if !is_nexrad_archive {
        return volume.volume_time.checked_add_signed(offset);
    }
    if !(0..MILLISECONDS_PER_DAY).contains(&offset_ms) {
        return None;
    }

    let midnight = volume
        .volume_time
        .date_naive()
        .and_hms_opt(0, 0, 0)?
        .and_utc();

    [-1_i64, 0, 1]
        .into_iter()
        .filter_map(|day| {
            midnight
                .checked_add_signed(TimeDelta::days(day))?
                .checked_add_signed(offset)
        })
        .min_by_key(|candidate| {
            candidate
                .signed_duration_since(volume.volume_time)
                .num_milliseconds()
                .unsigned_abs()
        })
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;
    use radar_core::{ElevationCut, GateRange, MomentGrid, RadarSite, Radial, RadialStatus};

    use super::*;

    fn volume_at(hour: u32, minute: u32, second: u32) -> RadarVolume {
        RadarVolume::new(
            RadarSite::new("KTLX"),
            Utc.with_ymd_and_hms(2026, 8, 24, hour, minute, second)
                .single()
                .expect("valid fixture time"),
        )
    }

    fn add_cut(
        volume: &mut RadarVolume,
        stored_elevation_deg: f32,
        measured_elevation_deg: f32,
        offset_ms: i32,
        radial_count: usize,
        azimuth_span_deg: f32,
        moments: &[MomentType],
    ) {
        let gate_range = GateRange {
            first_gate_m: 250,
            gate_spacing_m: 250,
            gate_count: 1,
        };
        let mut cut = ElevationCut::new(stored_elevation_deg, Some(volume.cuts.len() as u8));
        for index in 0..radial_count {
            cut.radials.push(Radial {
                azimuth_deg: index as f32 * azimuth_span_deg / radial_count as f32,
                elevation_deg: measured_elevation_deg,
                time_offset_ms: offset_ms,
                gate_range: gate_range.clone(),
                nyquist_velocity_mps: Some(25.0),
                radial_status: Some(if index + 1 == radial_count {
                    RadialStatus::EndElevation
                } else {
                    RadialStatus::Intermediate
                }),
            });
        }
        for moment in moments {
            let mut grid = MomentGrid::new_u8(
                moment.clone(),
                gate_range.clone(),
                2.0,
                66.0,
                Some(0),
                Some(1),
            );
            for index in 0..radial_count {
                grid.push_u8_row_slice(index, &[2])
                    .expect("valid one-gate fixture radial");
            }
            cut.moments.insert(moment.clone(), grid);
        }
        volume.cuts.push(cut);
    }

    fn enabled() -> LiveFollowPolicy {
        LiveFollowPolicy {
            enabled: true,
            ..LiveFollowPolicy::default()
        }
    }

    #[test]
    fn following_is_disabled_by_default() {
        let mut volume = volume_at(18, 0, 0);
        add_cut(
            &mut volume,
            0.5,
            0.5,
            30_000,
            360,
            360.0,
            &[MomentType::Reflectivity],
        );
        let capabilities = VolumeCapabilities::analyze(&volume);

        assert_eq!(
            newest_eligible_cut(
                &volume,
                &capabilities,
                &MomentType::Reflectivity,
                CutSelectionPolicy::LongestUnfoldedRange,
                LiveFollowPolicy::default(),
                None,
            ),
            None
        );
    }

    #[test]
    fn chooses_newest_actual_sweep_under_the_configurable_measured_tilt() {
        let mut volume = volume_at(18, 0, 0);
        add_cut(
            &mut volume,
            0.5,
            0.5,
            90_000,
            360,
            360.0,
            &[MomentType::Reflectivity],
        );
        // The stored first-radial angle lies; measured median elevation wins.
        add_cut(
            &mut volume,
            1.8,
            1.35,
            120_000,
            720,
            360.0,
            &[MomentType::Reflectivity],
        );
        // File order cannot override the sweep's actual scan time.
        add_cut(
            &mut volume,
            0.5,
            0.5,
            60_000,
            360,
            360.0,
            &[MomentType::Reflectivity],
        );
        let capabilities = VolumeCapabilities::analyze(&volume);

        assert_eq!(
            newest_eligible_cut(
                &volume,
                &capabilities,
                &MomentType::Reflectivity,
                CutSelectionPolicy::LongestUnfoldedRange,
                enabled(),
                None,
            )
            .map(|candidate| candidate.cut_index),
            Some(1)
        );
        assert_eq!(
            newest_eligible_cut(
                &volume,
                &capabilities,
                &MomentType::Reflectivity,
                CutSelectionPolicy::LongestUnfoldedRange,
                LiveFollowPolicy {
                    max_elevation_deg: 1.0,
                    ..enabled()
                },
                None,
            )
            .map(|candidate| candidate.cut_index),
            Some(0)
        );
    }

    #[test]
    fn insufficient_partial_coverage_and_sparse_sweeps_cannot_replace_a_full_revolution() {
        let mut volume = volume_at(18, 0, 0);
        add_cut(
            &mut volume,
            0.5,
            0.5,
            30_000,
            180,
            360.0,
            &[MomentType::Reflectivity],
        );
        add_cut(
            &mut volume,
            0.5,
            0.5,
            60_000,
            720,
            8.0,
            &[MomentType::Reflectivity],
        );
        add_cut(
            &mut volume,
            0.5,
            0.5,
            90_000,
            90,
            360.0,
            &[MomentType::Reflectivity],
        );
        let capabilities = VolumeCapabilities::analyze(&volume);

        assert_eq!(
            newest_eligible_cut(
                &volume,
                &capabilities,
                &MomentType::Reflectivity,
                CutSelectionPolicy::LongestUnfoldedRange,
                enabled(),
                None,
            )
            .map(|candidate| candidate.cut_index),
            Some(0)
        );
    }

    #[test]
    fn growing_partial_sweep_preempts_older_complete_sweep_below_the_tilt_ceiling() {
        let mut volume = volume_at(18, 0, 0);
        add_cut(
            &mut volume,
            0.5,
            0.5,
            30_000,
            360,
            360.0,
            &[MomentType::Reflectivity],
        );
        add_cut(
            &mut volume,
            0.5,
            0.5,
            75_000,
            64,
            32.0,
            &[MomentType::Reflectivity],
        );
        add_cut(
            &mut volume,
            1.6,
            1.6,
            90_000,
            64,
            32.0,
            &[MomentType::Reflectivity],
        );
        let capabilities = VolumeCapabilities::analyze(&volume);
        let previous = volume.volume_time + TimeDelta::seconds(30);

        assert!(!capabilities.cuts[1].complete);

        let candidate = newest_eligible_cut(
            &volume,
            &capabilities,
            &MomentType::Reflectivity,
            CutSelectionPolicy::LongestUnfoldedRange,
            enabled(),
            Some(previous),
        )
        .expect("a real growing low sweep is ready for the native radial animator");

        assert_eq!(candidate.cut_index, 1);
        assert_eq!(
            candidate.scan_time,
            volume.volume_time + TimeDelta::seconds(75)
        );

        assert_eq!(
            newest_eligible_cut(
                &volume,
                &capabilities,
                &MomentType::Reflectivity,
                CutSelectionPolicy::LongestUnfoldedRange,
                LiveFollowPolicy {
                    min_interval: Duration::from_secs(46),
                    ..enabled()
                },
                Some(previous),
            ),
            None,
            "a partial sweep still obeys the independently configured minimum interval"
        );
    }

    #[test]
    fn partial_sweep_waits_for_enough_rows_of_the_requested_product() {
        let mut volume = volume_at(18, 0, 0);
        add_cut(
            &mut volume,
            0.5,
            0.5,
            30_000,
            360,
            360.0,
            &[MomentType::Reflectivity],
        );
        add_cut(
            &mut volume,
            0.5,
            0.5,
            75_000,
            80,
            40.0,
            &[MomentType::Reflectivity],
        );
        volume.cuts[1]
            .moments
            .get_mut(&MomentType::Reflectivity)
            .expect("the fixture contains reflectivity")
            .radial_indices
            .truncate(31);
        let capabilities = VolumeCapabilities::analyze(&volume);
        let previous = volume.volume_time + TimeDelta::seconds(30);

        assert_eq!(
            newest_eligible_cut(
                &volume,
                &capabilities,
                &MomentType::Reflectivity,
                CutSelectionPolicy::LongestUnfoldedRange,
                enabled(),
                Some(previous),
            ),
            None,
            "available radial geometry alone is not evidence that this product is ready"
        );
    }

    #[test]
    fn preserves_surveillance_and_doppler_legs_of_split_cuts() {
        let mut volume = volume_at(18, 0, 0);
        add_cut(
            &mut volume,
            0.5,
            0.5,
            30_000,
            360,
            360.0,
            &[MomentType::Reflectivity],
        );
        add_cut(
            &mut volume,
            0.5,
            0.5,
            60_000,
            360,
            360.0,
            &[MomentType::Velocity],
        );
        add_cut(
            &mut volume,
            0.5,
            0.5,
            90_000,
            360,
            360.0,
            &[MomentType::Reflectivity],
        );
        add_cut(
            &mut volume,
            0.5,
            0.5,
            120_000,
            360,
            360.0,
            &[MomentType::Reflectivity, MomentType::Velocity],
        );
        add_cut(
            &mut volume,
            0.5,
            0.5,
            150_000,
            64,
            32.0,
            &[MomentType::Reflectivity, MomentType::Velocity],
        );
        let capabilities = VolumeCapabilities::analyze(&volume);

        assert_eq!(
            newest_eligible_cut(
                &volume,
                &capabilities,
                &MomentType::Velocity,
                CutSelectionPolicy::VelocityLeg,
                enabled(),
                None,
            )
            .map(|candidate| candidate.cut_index),
            Some(4),
            "a growing Doppler sweep can animate the velocity pane immediately"
        );
        assert_eq!(
            newest_eligible_cut(
                &volume,
                &capabilities,
                &MomentType::Reflectivity,
                CutSelectionPolicy::LongestUnfoldedRange,
                enabled(),
                None,
            )
            .map(|candidate| candidate.cut_index),
            Some(2),
            "even the newer growing Doppler leg must not replace long-range surveillance reflectivity"
        );
    }

    #[test]
    fn interval_uses_sweep_time_and_never_moves_backwards_or_repeats() {
        let mut volume = volume_at(18, 0, 0);
        add_cut(
            &mut volume,
            0.5,
            0.5,
            30_000,
            360,
            360.0,
            &[MomentType::Reflectivity],
        );
        add_cut(
            &mut volume,
            0.5,
            0.5,
            59_000,
            360,
            360.0,
            &[MomentType::Reflectivity],
        );
        let capabilities = VolumeCapabilities::analyze(&volume);
        let last = volume.volume_time + TimeDelta::seconds(30);

        assert_eq!(
            newest_eligible_cut(
                &volume,
                &capabilities,
                &MomentType::Reflectivity,
                CutSelectionPolicy::LongestUnfoldedRange,
                enabled(),
                Some(last),
            ),
            None
        );

        let faster = LiveFollowPolicy {
            min_interval: Duration::from_secs(29),
            ..enabled()
        };
        let candidate = newest_eligible_cut(
            &volume,
            &capabilities,
            &MomentType::Reflectivity,
            CutSelectionPolicy::LongestUnfoldedRange,
            faster,
            Some(last),
        )
        .expect("the exact configured interval is eligible");
        assert_eq!(candidate.cut_index, 1);
        assert_eq!(
            newest_eligible_cut(
                &volume,
                &capabilities,
                &MomentType::Reflectivity,
                CutSelectionPolicy::LongestUnfoldedRange,
                LiveFollowPolicy {
                    min_interval: Duration::ZERO,
                    ..enabled()
                },
                Some(candidate.scan_time),
            ),
            None,
            "a zero interval still cannot follow the same or an older sweep"
        );
    }

    #[test]
    fn nexrad_milliseconds_since_midnight_follow_correctly_across_utc_midnight() {
        let mut volume = volume_at(23, 59, 45);
        volume.metadata.archive_version = Some("AR2V0006".to_owned());
        add_cut(
            &mut volume,
            0.5,
            0.5,
            86_390_000,
            360,
            360.0,
            &[MomentType::Reflectivity],
        );
        add_cut(
            &mut volume,
            0.5,
            0.5,
            15_000,
            360,
            360.0,
            &[MomentType::Reflectivity],
        );
        let capabilities = VolumeCapabilities::analyze(&volume);
        let previous = Utc
            .with_ymd_and_hms(2026, 8, 24, 23, 59, 50)
            .single()
            .expect("valid fixture time");

        let candidate = newest_eligible_cut(
            &volume,
            &capabilities,
            &MomentType::Reflectivity,
            CutSelectionPolicy::LongestUnfoldedRange,
            LiveFollowPolicy {
                min_interval: Duration::from_secs(25),
                ..enabled()
            },
            Some(previous),
        )
        .expect("the next-day sweep is newer");

        assert_eq!(candidate.cut_index, 1);
        assert_eq!(
            candidate.scan_time,
            Utc.with_ymd_and_hms(2026, 8, 25, 0, 0, 15)
                .single()
                .expect("valid fixture time")
        );
    }

    #[test]
    fn research_offsets_remain_relative_to_the_volume_even_near_midnight() {
        let mut volume = volume_at(23, 59, 45);
        volume.metadata.archive_version = Some("ODIM_H5".to_owned());
        add_cut(
            &mut volume,
            0.5,
            0.5,
            30_000,
            360,
            360.0,
            &[MomentType::Reflectivity],
        );
        let capabilities = VolumeCapabilities::analyze(&volume);

        let candidate = newest_eligible_cut(
            &volume,
            &capabilities,
            &MomentType::Reflectivity,
            CutSelectionPolicy::LongestUnfoldedRange,
            enabled(),
            None,
        )
        .expect("the relative timestamp is valid");

        assert_eq!(
            candidate.scan_time,
            Utc.with_ymd_and_hms(2026, 8, 25, 0, 0, 15)
                .single()
                .expect("valid fixture time")
        );
    }
}

use std::collections::VecDeque;
use std::mem::size_of;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use radar_core::{ElevationCut, MomentGrid, MomentStorage, MomentType, RadarVolume, Radial};
use serde::{Deserialize, Serialize};

const DEFAULT_HISTORY_FRAMES: usize = 30;
const DEFAULT_HISTORY_BYTES: usize = 1024 * 1024 * 1024;
const APPROX_BTREE_NODE_LINK_BYTES: usize = size_of::<usize>() * 4;

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub struct FrameIdentity {
    pub site_id: String,
    pub volume_time: DateTime<Utc>,
}

impl FrameIdentity {
    pub fn for_volume(volume: &RadarVolume) -> Self {
        Self {
            site_id: volume.site.id.to_ascii_uppercase(),
            volume_time: volume.volume_time,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum FrameOrigin {
    Live,
    Archive,
    Local,
    Url,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub enum FrameStage {
    Preview,
    Partial,
    Complete,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum PlaybackState {
    Paused,
    Playing,
}

/// One immutable history snapshot.
#[derive(Clone)]
pub struct VolumeFrame {
    pub identity: FrameIdentity,
    pub origin: FrameOrigin,
    pub stage: FrameStage,
    pub source_label: String,
    pub volume: Arc<RadarVolume>,
    pub estimated_bytes: usize,
}

impl VolumeFrame {
    pub fn new(
        volume: Arc<RadarVolume>,
        origin: FrameOrigin,
        stage: FrameStage,
        source_label: impl Into<String>,
    ) -> Self {
        let identity = FrameIdentity::for_volume(&volume);
        let estimated_bytes = estimate_radar_volume_bytes(&volume);
        Self {
            identity,
            origin,
            stage,
            source_label: source_label.into(),
            volume,
            estimated_bytes,
        }
    }

    pub fn with_estimated_bytes(mut self, estimated_bytes: usize) -> Self {
        self.estimated_bytes = estimated_bytes.max(1);
        self
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct HistoryPolicy {
    pub max_frames: usize,
    pub max_estimated_bytes: usize,
}

impl HistoryPolicy {
    /// Build a history policy. Zero means unlimited for that dimension.
    ///
    /// This is intentionally literal rather than replacing zero with a small
    /// positive value: local research playlists may contain thousands of
    /// operator-selected volumes, and an unstated eviction ceiling is data
    /// loss from the timeline even when the source files remain on disk.
    pub const fn new(max_frames: usize, max_estimated_bytes: usize) -> Self {
        Self {
            max_frames,
            max_estimated_bytes,
        }
    }

    pub const fn unlimited() -> Self {
        Self::new(0, 0)
    }
}

impl Default for HistoryPolicy {
    fn default() -> Self {
        // Safe for an unattended live feed. Operator-selected local sessions
        // explicitly install `HistoryPolicy::unlimited()` unless the settings
        // store carries a positive limit.
        Self::new(DEFAULT_HISTORY_FRAMES, DEFAULT_HISTORY_BYTES)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InstallDisposition {
    Inserted,
    Replaced,
    IgnoredStageDowngrade,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InstallReport {
    pub disposition: InstallDisposition,
    pub evicted: Vec<FrameIdentity>,
}

/// Chronological volume history with optional operator-configured ceilings.
///
/// The selected frame is protected from ordinary eviction while another frame
/// can be removed. A single frame larger than the byte budget is retained so
/// loading a large volume never produces an empty display. Both policy values
/// use zero as Unlimited. The runtime's general default remains bounded for an
/// unattended live feed; the workstation explicitly chooses Unlimited for a
/// local operator-selected session.
pub struct VolumeHistory {
    frames: VecDeque<VolumeFrame>,
    selected: Option<usize>,
    follow_live: bool,
    playback: PlaybackState,
    policy: HistoryPolicy,
    estimated_bytes: usize,
}

impl Default for VolumeHistory {
    fn default() -> Self {
        Self::new(HistoryPolicy::default())
    }
}

impl VolumeHistory {
    pub fn new(policy: HistoryPolicy) -> Self {
        Self {
            frames: VecDeque::new(),
            selected: None,
            follow_live: true,
            playback: PlaybackState::Paused,
            policy: HistoryPolicy::new(policy.max_frames, policy.max_estimated_bytes),
            estimated_bytes: 0,
        }
    }

    pub fn len(&self) -> usize {
        self.frames.len()
    }

    pub fn is_empty(&self) -> bool {
        self.frames.is_empty()
    }

    pub fn frames(&self) -> &VecDeque<VolumeFrame> {
        &self.frames
    }

    pub fn current(&self) -> Option<&VolumeFrame> {
        self.selected.and_then(|index| self.frames.get(index))
    }

    pub fn selected_index(&self) -> Option<usize> {
        self.selected
    }

    pub fn estimated_bytes(&self) -> usize {
        self.estimated_bytes
    }

    pub fn policy(&self) -> HistoryPolicy {
        self.policy
    }

    pub fn playback(&self) -> PlaybackState {
        self.playback
    }

    pub fn set_playback(&mut self, playback: PlaybackState) {
        self.playback = playback;
    }

    pub fn follows_live(&self) -> bool {
        self.follow_live
    }

    pub fn at_live_edge(&self) -> bool {
        self.selected
            .is_some_and(|index| index + 1 == self.frames.len())
    }

    pub fn install(&mut self, frame: VolumeFrame) -> InstallReport {
        let selected_identity = self.current().map(|frame| frame.identity.clone());
        let disposition = if let Some(index) = self
            .frames
            .iter()
            .position(|existing| existing.identity == frame.identity)
        {
            if frame.stage < self.frames[index].stage {
                return InstallReport {
                    disposition: InstallDisposition::IgnoredStageDowngrade,
                    evicted: Vec::new(),
                };
            }
            let previous_bytes = self.frames[index].estimated_bytes;
            self.frames[index] = frame;
            self.estimated_bytes = self
                .estimated_bytes
                .saturating_sub(previous_bytes)
                .saturating_add(self.frames[index].estimated_bytes);
            InstallDisposition::Replaced
        } else {
            let insert_at = self
                .frames
                .iter()
                .position(|existing| existing.identity > frame.identity)
                .unwrap_or(self.frames.len());
            self.estimated_bytes = self.estimated_bytes.saturating_add(frame.estimated_bytes);
            self.frames.insert(insert_at, frame);
            InstallDisposition::Inserted
        };

        if self.follow_live {
            self.selected = self.frames.len().checked_sub(1);
        } else if let Some(identity) = selected_identity {
            self.selected = self
                .frames
                .iter()
                .position(|frame| frame.identity == identity)
                .or_else(|| self.frames.len().checked_sub(1));
        } else {
            self.selected = self.frames.len().checked_sub(1);
        }

        let evicted = self.enforce_policy();
        InstallReport {
            disposition,
            evicted,
        }
    }

    /// Insert an independently admitted local-playlist frame even when its
    /// radar/time identity matches another frame already retained.
    ///
    /// Live previews and growing live volumes must continue to use
    /// [`Self::install`] so a newer stage replaces the earlier one. This mode
    /// exists for different operator-selected source paths: equal timestamps
    /// are common in research exports and are not proof that one file may be
    /// discarded. Equal identities remain adjacent in their input order.
    pub fn install_distinct(&mut self, frame: VolumeFrame) -> InstallReport {
        let selected_before = self.selected;
        let insert_at = self
            .frames
            .iter()
            .position(|existing| existing.identity > frame.identity)
            .unwrap_or(self.frames.len());
        self.estimated_bytes = self.estimated_bytes.saturating_add(frame.estimated_bytes);
        self.frames.insert(insert_at, frame);

        if self.follow_live {
            self.selected = self.frames.len().checked_sub(1);
        } else if let Some(selected) = selected_before {
            self.selected = Some(selected + usize::from(insert_at <= selected));
        } else {
            self.selected = self.frames.len().checked_sub(1);
        }

        InstallReport {
            disposition: InstallDisposition::Inserted,
            evicted: self.enforce_policy(),
        }
    }

    pub fn set_policy(&mut self, policy: HistoryPolicy) -> Vec<FrameIdentity> {
        self.policy = HistoryPolicy::new(policy.max_frames, policy.max_estimated_bytes);
        self.enforce_policy()
    }

    pub fn select(&mut self, index: usize) -> bool {
        if index >= self.frames.len() {
            return false;
        }
        self.selected = Some(index);
        self.follow_live = false;
        true
    }

    pub fn go_live(&mut self) -> Option<&VolumeFrame> {
        self.selected = self.frames.len().checked_sub(1);
        self.follow_live = true;
        self.current()
    }

    pub fn step_clamped(&mut self, delta: isize) -> Option<&VolumeFrame> {
        let len = self.frames.len();
        if len == 0 {
            self.selected = None;
            return None;
        }
        let current = self.selected.unwrap_or(len - 1) as isize;
        let target = current.saturating_add(delta).clamp(0, len as isize - 1) as usize;
        self.selected = Some(target);
        self.follow_live = false;
        self.current()
    }

    pub fn next_index_wrapping(&self) -> Option<usize> {
        let len = self.frames.len();
        if len == 0 {
            return None;
        }
        Some((self.selected.unwrap_or(len - 1) + 1) % len)
    }

    pub fn advance_wrapping(&mut self) -> Option<&VolumeFrame> {
        self.selected = self.next_index_wrapping();
        self.follow_live = false;
        self.current()
    }

    pub fn clear(&mut self) {
        self.frames.clear();
        self.selected = None;
        self.follow_live = true;
        self.playback = PlaybackState::Paused;
        self.estimated_bytes = 0;
    }

    fn enforce_policy(&mut self) -> Vec<FrameIdentity> {
        let mut evicted = Vec::new();
        while self.frames.len() > 1
            && ((self.policy.max_frames > 0 && self.frames.len() > self.policy.max_frames)
                || (self.policy.max_estimated_bytes > 0
                    && self.estimated_bytes > self.policy.max_estimated_bytes))
        {
            let selected = self.selected.unwrap_or(self.frames.len() - 1);
            let remove_at = (0..self.frames.len())
                .find(|index| *index != selected)
                .expect("more than one frame leaves an unselected eviction candidate");
            let removed = self
                .frames
                .remove(remove_at)
                .expect("eviction index is in range");
            self.estimated_bytes = self.estimated_bytes.saturating_sub(removed.estimated_bytes);
            evicted.push(removed.identity);
            self.selected = Some(if remove_at < selected {
                selected - 1
            } else {
                selected
            });
        }

        // Snap to the newest frame when following the live edge, and also when
        // eviction left the selection unset or past the end.
        if self.frames.is_empty() {
            self.selected = None;
        } else if self.follow_live || self.selected.is_none_or(|index| index >= self.frames.len()) {
            self.selected = Some(self.frames.len() - 1);
        }
        evicted
    }
}

/// Conservative estimate of the resident allocations owned by a decoded
/// volume. It intentionally estimates capacities rather than lengths. Shared
/// textures, product caches, and worker scratch buffers have separate owners
/// and are not included here.
pub fn estimate_radar_volume_bytes(volume: &RadarVolume) -> usize {
    let mut bytes = size_of::<RadarVolume>();
    bytes = add_capacity(bytes, volume.site.id.capacity());
    if let Some(name) = &volume.site.name {
        bytes = add_capacity(bytes, name.capacity());
    }
    if let Some(value) = &volume.metadata.source_path {
        bytes = add_capacity(bytes, value.capacity());
    }
    if let Some(value) = &volume.metadata.archive_version {
        bytes = add_capacity(bytes, value.capacity());
    }
    if let Some(value) = &volume.metadata.compression {
        bytes = add_capacity(bytes, value.capacity());
    }

    bytes = bytes.saturating_add(
        volume
            .cuts
            .capacity()
            .saturating_mul(size_of::<ElevationCut>()),
    );
    for cut in &volume.cuts {
        bytes = bytes.saturating_add(cut.radials.capacity().saturating_mul(size_of::<Radial>()));
        for (moment, grid) in &cut.moments {
            bytes = bytes
                .saturating_add(size_of::<(MomentType, MomentGrid)>())
                .saturating_add(APPROX_BTREE_NODE_LINK_BYTES);
            if let MomentType::Unknown(name) = moment {
                bytes = add_capacity(bytes, name.capacity());
            }
            bytes = bytes.saturating_add(
                grid.radial_indices
                    .capacity()
                    .saturating_mul(size_of::<usize>()),
            );
            bytes = bytes.saturating_add(match &grid.storage {
                MomentStorage::U8(values) => values.capacity(),
                MomentStorage::U16(values) => values.capacity().saturating_mul(size_of::<u16>()),
                MomentStorage::F32(values) => values.capacity().saturating_mul(size_of::<f32>()),
            });
            if let MomentType::Unknown(name) = &grid.moment {
                bytes = add_capacity(bytes, name.capacity());
            }
        }
    }
    bytes.max(1)
}

fn add_capacity(bytes: usize, capacity: usize) -> usize {
    bytes.saturating_add(capacity)
}

#[cfg(test)]
mod tests {
    use chrono::{TimeZone, Timelike};
    use radar_core::{GateRange, RadarSite};

    use super::*;

    fn volume(minute: u32, gates: usize) -> Arc<RadarVolume> {
        let mut volume = RadarVolume::new(
            RadarSite::new("KTLX"),
            Utc.with_ymd_and_hms(2026, 5, 20, 1, minute, 0).unwrap(),
        );
        let mut cut = ElevationCut::new(0.5, Some(1));
        let mut grid = MomentGrid::new_u8(
            MomentType::Reflectivity,
            GateRange {
                first_gate_m: 250,
                gate_spacing_m: 250,
                gate_count: gates,
            },
            2.0,
            66.0,
            Some(0),
            Some(1),
        );
        grid.radial_indices = vec![0];
        grid.storage = MomentStorage::U8(vec![2; gates]);
        cut.moments.insert(MomentType::Reflectivity, grid);
        volume.cuts.push(cut);
        Arc::new(volume)
    }

    fn frame(minute: u32, stage: FrameStage, bytes: usize) -> VolumeFrame {
        VolumeFrame::new(volume(minute, 16), FrameOrigin::Live, stage, "test")
            .with_estimated_bytes(bytes)
    }

    #[test]
    fn complete_replaces_partial_and_downgrade_is_ignored() {
        let mut history = VolumeHistory::default();
        assert_eq!(
            history
                .install(frame(0, FrameStage::Partial, 10))
                .disposition,
            InstallDisposition::Inserted
        );
        assert_eq!(
            history
                .install(frame(0, FrameStage::Complete, 12))
                .disposition,
            InstallDisposition::Replaced
        );
        assert_eq!(history.current().unwrap().stage, FrameStage::Complete);
        assert_eq!(
            history
                .install(frame(0, FrameStage::Preview, 8))
                .disposition,
            InstallDisposition::IgnoredStageDowngrade
        );
        assert_eq!(history.current().unwrap().stage, FrameStage::Complete);
    }

    #[test]
    fn out_of_order_archive_results_install_chronologically() {
        let mut history = VolumeHistory::default();
        history.install(frame(20, FrameStage::Complete, 10));
        history.install(frame(0, FrameStage::Complete, 10));
        history.install(frame(10, FrameStage::Complete, 10));
        let minutes = history
            .frames()
            .iter()
            .map(|frame| frame.identity.volume_time.minute())
            .collect::<Vec<_>>();
        assert_eq!(minutes, vec![0, 10, 20]);
    }

    #[test]
    fn paused_selection_survives_new_live_frames() {
        let mut history = VolumeHistory::default();
        history.install(frame(0, FrameStage::Complete, 10));
        history.install(frame(10, FrameStage::Complete, 10));
        assert!(history.select(0));
        history.install(frame(20, FrameStage::Complete, 10));
        assert_eq!(history.current().unwrap().identity.volume_time.minute(), 0);
        assert!(!history.follows_live());
        history.go_live();
        assert_eq!(history.current().unwrap().identity.volume_time.minute(), 20);
        assert!(history.follows_live());
    }

    #[test]
    fn selected_frame_is_protected_while_another_can_be_evicted() {
        let mut history = VolumeHistory::new(HistoryPolicy::new(3, 25));
        history.install(frame(0, FrameStage::Complete, 10));
        history.install(frame(10, FrameStage::Complete, 10));
        assert!(history.select(0));
        let report = history.install(frame(20, FrameStage::Complete, 10));
        assert_eq!(report.evicted.len(), 1);
        assert_eq!(history.len(), 2);
        assert_eq!(history.current().unwrap().identity.volume_time.minute(), 0);
        assert!(history.estimated_bytes() <= 25);
    }

    #[test]
    fn one_oversized_frame_is_retained() {
        let mut history = VolumeHistory::new(HistoryPolicy::new(10, 5));
        history.install(frame(0, FrameStage::Complete, 100));
        assert_eq!(history.len(), 1);
        assert_eq!(history.estimated_bytes(), 100);
    }

    #[test]
    fn unlimited_history_retains_1075_operator_selected_frames() {
        let mut history = VolumeHistory::new(HistoryPolicy::unlimited());
        let base = Utc.with_ymd_and_hms(2026, 5, 20, 0, 0, 0).unwrap();
        for index in 0..1_075 {
            let mut volume = (*volume(0, 16)).clone();
            volume.volume_time = base + chrono::TimeDelta::seconds(index);
            let report = history.install(
                VolumeFrame::new(
                    Arc::new(volume),
                    FrameOrigin::Local,
                    FrameStage::Complete,
                    format!("selected-{index:04}"),
                )
                .with_estimated_bytes(10),
            );
            assert!(
                report.evicted.is_empty(),
                "frame {index} was silently evicted"
            );
        }

        assert_eq!(history.policy(), HistoryPolicy::unlimited());
        assert_eq!(history.len(), 1_075);
        assert_eq!(history.estimated_bytes(), 10_750);
    }

    #[test]
    fn either_positive_limit_remains_independently_optional() {
        let mut frame_limited = VolumeHistory::new(HistoryPolicy::new(2, 0));
        frame_limited.install(frame(0, FrameStage::Complete, 100));
        frame_limited.install(frame(10, FrameStage::Complete, 100));
        let frame_report = frame_limited.install(frame(20, FrameStage::Complete, 100));
        assert_eq!(frame_report.evicted.len(), 1);
        assert_eq!(frame_limited.len(), 2);

        let mut byte_limited = VolumeHistory::new(HistoryPolicy::new(0, 25));
        byte_limited.install(frame(0, FrameStage::Complete, 10));
        byte_limited.install(frame(10, FrameStage::Complete, 10));
        let byte_report = byte_limited.install(frame(20, FrameStage::Complete, 10));
        assert_eq!(byte_report.evicted.len(), 1);
        assert_eq!(byte_limited.len(), 2);
    }

    #[test]
    fn distinct_local_sources_with_the_same_site_and_time_are_both_retained() {
        let mut history = VolumeHistory::new(HistoryPolicy::unlimited());
        let mut first = frame(0, FrameStage::Complete, 10);
        first.origin = FrameOrigin::Local;
        let mut second = frame(0, FrameStage::Complete, 10);
        second.origin = FrameOrigin::Local;
        second.source_label = "different-selected-path".to_owned();

        assert_eq!(
            history.install_distinct(first).disposition,
            InstallDisposition::Inserted
        );
        assert_eq!(
            history.install_distinct(second).disposition,
            InstallDisposition::Inserted
        );
        assert_eq!(history.len(), 2);
        assert_eq!(history.frames()[0].source_label, "test");
        assert_eq!(history.frames()[1].source_label, "different-selected-path");
    }

    #[test]
    fn estimate_tracks_compact_moment_storage() {
        let small = estimate_radar_volume_bytes(&volume(0, 10));
        let large = estimate_radar_volume_bytes(&volume(0, 10_000));
        assert!(large > small + 9_000);
    }
}

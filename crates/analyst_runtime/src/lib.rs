//! UI-independent runtime contracts for the radar workstation.
//!
//! This crate owns serializable user intent and pure scheduling/history policy.
//! It deliberately does not depend on egui, network clients, decoders, GPU
//! handles, or application panels.

#![forbid(unsafe_code)]

mod camera_motion;
mod generation;
mod history;
mod jobs;
mod view;
mod workspace;

pub use camera_motion::{
    CameraMotion, MotionStep, PAN_FLING_DECAY_SECONDS, PAN_FLING_MAX_SPEED_POINTS_PER_SECOND,
    PAN_VELOCITY_WINDOW_SECONDS, ZOOM_RESPONSE_SECONDS,
};
pub use generation::{Generation, GenerationClock, RenderStamp, SceneStamp};
pub use history::{
    FrameIdentity, FrameOrigin, FrameStage, HistoryPolicy, InstallDisposition, InstallReport,
    PlaybackState, VolumeFrame, VolumeHistory, estimate_radar_volume_bytes,
};
pub use jobs::{
    LatestLaneReceiver, LatestLaneSender, SendClosed, SubmitOutcome, latest_lane_channel,
};
pub use view::{
    BURST_MEMORY_SECONDS, Camera2D, DEFAULT_KM_PER_POINT, GeometryCacheKey,
    KEY_PAN_FRACTION_PER_SECOND, KEY_ZOOM_RATE_PER_SECOND, LodBucket, LodSelector, MAX_BURST_GAIN,
    MAX_KM_PER_POINT, MAX_NAV_STEP_SECONDS, MAX_SCALE_CHANGE_PER_FRAME, MIN_KM_PER_POINT, NavInput,
    RasterView, ScreenPoint, TRACKPAD_POINTS_PER_NOTCH, ViewportMetrics, WheelNotches, WorldPoint,
    ZOOM_PER_NOTCH, ZoomResponder, zoom_factor_for_notches,
};
pub use workspace::{
    MAX_PANES, NEXRAD_SURVEILLANCE_RANGE_KM, PaneId, PaneIntent, PaneLayout, PaneLinkGroups,
    SmoothingMode, StormMotionIntent, TiltSelection, WorkspaceState,
};

//! Warning and hazard polygons.
//!
//! Two sources, one record type.
//!
//! * A **self-hosted `nwws-rs` daemon**, which takes the NWWS-OI wire at the
//!   source. It carries the full VTEC lifecycle (NEW / CON / COR / CAN / EXP)
//!   and has no polling latency or rate limit.
//! * **`api.weather.gov/alerts/active`**, which is public, needs no account,
//!   and is what the workstation uses when no daemon is running.
//!
//! The daemon is better when it is there; the public API is what makes the
//! warnings layer work on a machine that has never run one. [`WarningsSource`]
//! defaults to trying the daemon and falling back.
//!
//! # Things this module knows that cost real time to discover
//!
//! 1. **`/v1/warnings/active` carries NO geometry.** Its record type has no
//!    polygon field at all. `/v1/timeline` is the only daemon endpoint that
//!    returns one, so that is the one this reads.
//! 2. **`nwws-rs`'s own types are `Serialize`-only.** They cannot parse its
//!    responses, which is why the wire structs below are ours. They are
//!    deliberately permissive -- every field `#[serde(default)]` -- so a daemon
//!    upgrade that adds a field cannot black out the warnings layer.
//! 3. **`tags.damage_threat` is scoped to the TORNADO and TSTM threat lines.**
//!    A `FLASH FLOOD DAMAGE THREAT...CONSIDERABLE` bulletin leaves it null and
//!    reports the threat only in `text_tags`. Verified against a live
//!    `nwws serve`: a real FFW came back with `damage_threat: null` and
//!    `text_tags[kind="flash_flood_damage_threat"] = "CONSIDERABLE"`, so a
//!    reader of the top-level field alone paints a considerable-threat flood in
//!    the ordinary flood colour.
//! 4. **The `nwws-rs` LIBRARY path returns raw POSITIVE western longitudes**
//!    (88.5 where the truth is -88.5) and its `normalize_longitude` is private.
//!    The HTTP path returns them already signed. We take the HTTP path;
//!    [`normalize_bulletin_longitude`] exists for anyone who does not, because
//!    the failure mode is polygons drawn in China.
//! 5. **`api.weather.gov` timestamps carry a LOCAL UTC offset**
//!    (`2026-08-17T16:16:00-05:00`), and [`WarningRecord::is_active_at`]
//!    compares RFC3339 strings byte-wise. Local-offset strings do not compare
//!    in time order, so every timestamp is converted to UTC `Z` form at parse
//!    time. Skipping that makes a warning look expired an hour before it is.
//! 6. **`api.weather.gov` parameter values are ARRAYS of strings**, and their
//!    contents are free text from the bulletin: `maxHailSize` arrives as
//!    `"Up to .75"` as often as `"0.75"`, and `maxWindGust` as `"60 MPH"`.
//!    Observed live on 2026-08-17 across 491 active alerts.
//!
//! # Degradation
//!
//! No daemon is the ordinary state, and so is no network. Every failure
//! resolves to a [`WarningsState`] carrying the reason as text within a bounded
//! timeout. Nothing here can hang the workstation and nothing here panics.

use std::collections::BTreeMap;
use std::time::Duration as StdDuration;

use chrono::SecondsFormat;
use serde::Deserialize;

/// Default endpoint for a locally run daemon: `nwws serve --bind 127.0.0.1:8080`.
pub const DEFAULT_DAEMON_BASE_URL: &str = "http://127.0.0.1:8080";

/// Public active-alerts feed. `status=actual` drops the exercise and test
/// products the NWS also publishes here, which are real records that must
/// never be painted as real weather.
pub const WEATHER_GOV_ALERTS_URL: &str = "https://api.weather.gov/alerts/active?status=actual";

/// How long to wait for a daemon before calling it unreachable. Short on
/// purpose: a workstation must not stall on a backend that is not there.
const CONNECT_TIMEOUT: StdDuration = StdDuration::from_secs(2);
const REQUEST_TIMEOUT: StdDuration = StdDuration::from_secs(20);

/// Where warnings come from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WarningsSource {
    /// Try the daemon; use the public feed when it does not answer. The
    /// default, because it is right both on the machine that runs `nwws serve`
    /// and on the one that never will.
    Auto { daemon_base_url: String },
    /// The daemon only. Reports offline rather than falling back, which is what
    /// an operator wants when the daemon is the thing under test.
    Daemon { base_url: String },
    /// The public feed only.
    WeatherGov,
}

impl Default for WarningsSource {
    fn default() -> Self {
        Self::Auto {
            daemon_base_url: DEFAULT_DAEMON_BASE_URL.to_owned(),
        }
    }
}

impl WarningsSource {
    /// Parse a configured value: a base URL selects the daemon, and `off`,
    /// `none` or `public` select the public feed.
    pub fn parse(value: &str) -> Self {
        let value = value.trim();
        match value.to_ascii_lowercase().as_str() {
            "" | "auto" => Self::default(),
            "off" | "none" | "public" | "weather.gov" => Self::WeatherGov,
            _ => Self::Daemon {
                base_url: value.trim_end_matches('/').to_owned(),
            },
        }
    }
}

/// Connection state, shown as its own chip so an operator can tell "no warnings
/// out" from "we are not receiving warnings".
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WarningsState {
    /// Nothing contacted yet.
    Unknown,
    /// Daemon up and ingesting the wire.
    Live { active: usize },
    /// Daemon up but running `--no-ingest`: archive replay only. Real warnings
    /// still render; new ones will not arrive.
    ArchiveOnly { active: usize },
    /// The public feed answered.
    Public { active: usize },
    /// Nothing answered. `reason` is shown on hover.
    Offline { reason: String },
}

impl WarningsState {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Unknown => "WX ...",
            Self::Live { .. } => "WX LIVE",
            Self::ArchiveOnly { .. } => "WX ARCHIVE",
            Self::Public { .. } => "WX NWS",
            Self::Offline { .. } => "WX OFFLINE",
        }
    }

    pub fn detail(&self) -> String {
        match self {
            Self::Unknown => "not contacted yet".to_owned(),
            Self::Live { active } => format!("nwws-rs daemon, {active} active"),
            Self::ArchiveOnly { active } => {
                format!("{active} active; daemon has --no-ingest, nothing new will arrive")
            }
            Self::Public { active } => format!("api.weather.gov, {active} active"),
            Self::Offline { reason } => reason.clone(),
        }
    }

    /// Active count, for the chip. `None` when nothing has answered.
    pub fn active(&self) -> Option<usize> {
        match self {
            Self::Live { active } | Self::ArchiveOnly { active } | Self::Public { active } => {
                Some(*active)
            }
            Self::Unknown | Self::Offline { .. } => None,
        }
    }

    pub fn is_offline(&self) -> bool {
        matches!(self, Self::Offline { .. })
    }
}

/// How loudly the map should draw a hazard. Ordered: `Extreme` sorts first.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    /// Tornado emergency, PDS, or a catastrophic damage threat.
    Extreme,
    /// Tornado warning, or a considerable/destructive thunderstorm.
    Severe,
    /// Ordinary warning.
    Moderate,
    /// Watch, advisory, statement.
    Minor,
}

impl Severity {
    pub fn label(self) -> &'static str {
        match self {
            Self::Extreme => "EXTREME",
            Self::Severe => "SEVERE",
            Self::Moderate => "MODERATE",
            Self::Minor => "MINOR",
        }
    }
}

/// One warning, in the workstation's own terms.
#[derive(Clone, Debug, PartialEq)]
pub struct WarningRecord {
    /// VTEC event id, e.g. `KLOT.O.TO.W.0001`. Stable across CON updates.
    pub event_id: String,
    pub office: String,
    /// VTEC phenomenon code: `TO`, `SV`, `FF`, ... `SPS` and `MWS` are used for
    /// the non-VTEC statements the public feed also carries.
    pub phenomenon: String,
    /// VTEC significance: `W` warning, `A` watch, `Y` advisory, `S` statement.
    pub significance: String,
    /// VTEC action: `NEW`, `CON`, `COR`, `CAN`, `EXP`, `UPG`.
    pub action: String,
    pub event_family: String,
    pub headline: Option<String>,
    /// RFC3339 **UTC** (`...Z`). Normalised at parse time -- see the module
    /// note on byte-wise comparison.
    pub valid_start: Option<String>,
    pub valid_end: Option<String>,
    pub severity: Severity,
    /// `RADAR INDICATED` / `OBSERVED`, when tagged.
    pub tornado: Option<String>,
    pub hail_inches: Option<f32>,
    pub wind_mph: Option<u16>,
    pub damage_threat: Option<String>,
    /// Polygon vertices as `(lon, lat)` -- x first, matching every projection
    /// call in this workspace. Empty for a UGC-only product.
    pub points: Vec<(f32, f32)>,
    /// `[west, south, east, north]`, for cheap culling. `None` when there is no
    /// polygon.
    pub bbox: Option<[f32; 4]>,
    /// Storm motion from `TIME...MOT...LOC`: (direction it comes FROM in
    /// degrees, speed in knots).
    pub motion: Option<(u16, u8)>,
    pub ugcs: Vec<String>,
}

impl WarningRecord {
    /// True when the event is still in force at `now` (RFC3339 UTC). A
    /// cancelled or expired VTEC action is out regardless of its end time.
    pub fn is_active_at(&self, now_rfc3339: &str) -> bool {
        if matches!(self.action.as_str(), "CAN" | "EXP" | "UPG") {
            return false;
        }
        // RFC3339 UTC strings compare lexicographically in time order, which is
        // the whole reason both parsers normalise to `Z`.
        let started = self
            .valid_start
            .as_deref()
            .map(|start| start.as_bytes() <= now_rfc3339.as_bytes())
            .unwrap_or(true);
        let ended = self
            .valid_end
            .as_deref()
            .map(|end| end.as_bytes() <= now_rfc3339.as_bytes())
            .unwrap_or(false);
        started && !ended
    }
}

/// Re-sign a longitude that came from the `nwws-rs` LIBRARY (not the HTTP API).
///
/// The bulletin `LAT...LON` block encodes west longitude as a positive number
/// with the hundreds digit dropped: `8850` means 88.50 W. `nwws-rs` applies the
/// fix internally before serving JSON but its `normalize_longitude` is private,
/// so a caller that parses bulletins directly must apply it too. Guam (PGUM) is
/// the one office in the eastern hemisphere.
pub fn normalize_bulletin_longitude(raw: f32, office: &str) -> f32 {
    let mut longitude = raw;
    if longitude < 40.0 {
        longitude += 100.0;
    }
    if office == "PGUM" {
        longitude
    } else {
        -longitude
    }
}

/// The map's draw weight and the list's sort key.
pub fn severity_of(
    phenomenon: &str,
    significance: &str,
    tornado: Option<&str>,
    damage_threat: Option<&str>,
    flash_flood_emergency: bool,
) -> Severity {
    let catastrophic = damage_threat
        .map(|threat| threat.eq_ignore_ascii_case("CATASTROPHIC"))
        .unwrap_or(false);
    let observed_tornado = tornado
        .map(|source| source.eq_ignore_ascii_case("OBSERVED"))
        .unwrap_or(false);

    if flash_flood_emergency || (phenomenon == "TO" && catastrophic) {
        return Severity::Extreme;
    }
    if significance != "W" {
        return Severity::Minor;
    }
    match phenomenon {
        "TO" if observed_tornado => Severity::Extreme,
        "TO" => Severity::Severe,
        "SV" if damage_threat.is_some_and(|threat| {
            threat.eq_ignore_ascii_case("DESTRUCTIVE")
                || threat.eq_ignore_ascii_case("CONSIDERABLE")
        }) =>
        {
            Severity::Severe
        }
        "FF" if catastrophic => Severity::Extreme,
        _ => Severity::Moderate,
    }
}

/// Bounding box of a vertex ring, as `[west, south, east, north]`.
fn bounds_of(points: &[(f32, f32)]) -> Option<[f32; 4]> {
    if points.is_empty() {
        return None;
    }
    let mut bounds = [f32::MAX, f32::MAX, f32::MIN, f32::MIN];
    for (lon, lat) in points {
        bounds[0] = bounds[0].min(*lon);
        bounds[1] = bounds[1].min(*lat);
        bounds[2] = bounds[2].max(*lon);
        bounds[3] = bounds[3].max(*lat);
    }
    Some(bounds)
}

/// Reject anything off the globe rather than drawing a spike to the horizon.
/// Bad geometry is a defect, not a hazard.
fn on_globe(lon: f32, lat: f32) -> bool {
    lon.is_finite() && lat.is_finite() && lat.abs() <= 90.0 && lon.abs() <= 180.0
}

/// Convert an RFC3339 timestamp with any offset into UTC `Z` form.
///
/// Byte-wise comparison of RFC3339 strings is only ordered when every string is
/// in the same zone; see the module note.
fn to_utc_rfc3339(raw: &str) -> Option<String> {
    let parsed = chrono::DateTime::parse_from_rfc3339(raw.trim()).ok()?;
    Some(
        parsed
            .with_timezone(&chrono::Utc)
            .to_rfc3339_opts(SecondsFormat::Secs, true),
    )
}

// ---------------------------------------------------------------------------
// nwws-rs wire types. Ours, because the daemon's are Serialize-only, and
// permissive, because a daemon upgrade must not black out the warnings layer.
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Deserialize)]
struct TimelineReport {
    #[serde(default)]
    records: Vec<TimelineRecord>,
}

#[derive(Debug, Default, Deserialize)]
struct TimelineRecord {
    #[serde(default)]
    event_id: String,
    #[serde(default)]
    office: String,
    #[serde(default)]
    phenomenon: String,
    #[serde(default)]
    significance: String,
    #[serde(default)]
    action: String,
    #[serde(default)]
    event_family: String,
    #[serde(default)]
    headline: Option<String>,
    #[serde(default)]
    valid_start: Option<String>,
    #[serde(default)]
    valid_end: Option<String>,
    #[serde(default)]
    ugcs: Vec<String>,
    #[serde(default)]
    tags: WireTags,
    #[serde(default)]
    polygon: Option<WirePolygon>,
    #[serde(default)]
    time_mot_loc: Option<WireMotion>,
}

#[derive(Debug, Default, Deserialize)]
struct WireTags {
    #[serde(default)]
    tornado: Option<String>,
    #[serde(default)]
    hail_inches: Option<f32>,
    #[serde(default)]
    wind_mph: Option<u16>,
    #[serde(default)]
    damage_threat: Option<String>,
    #[serde(default)]
    flash_flood_emergency: bool,
    /// The daemon's per-line tag list. See module note 3: this is the only
    /// place a flash-flood damage threat appears.
    #[serde(default)]
    text_tags: Vec<WireTextTag>,
}

impl WireTags {
    /// The damage threat, from whichever field the daemon put it in.
    fn threat(&self) -> Option<String> {
        if let Some(threat) = &self.damage_threat {
            return Some(threat.clone());
        }
        self.text_tags
            .iter()
            .find(|tag| tag.kind.ends_with("damage_threat"))
            .and_then(|tag| {
                tag.normalized_value
                    .clone()
                    .or_else(|| tag.raw_value.clone())
            })
    }
}

#[derive(Debug, Default, Deserialize)]
struct WireTextTag {
    #[serde(default)]
    kind: String,
    #[serde(default)]
    normalized_value: Option<String>,
    #[serde(default)]
    raw_value: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct WirePolygon {
    #[serde(default)]
    points: Vec<WirePoint>,
}

#[derive(Debug, Default, Deserialize)]
struct WirePoint {
    #[serde(default)]
    lat: f32,
    #[serde(default)]
    lon: f32,
}

#[derive(Debug, Default, Deserialize)]
struct WireMotion {
    #[serde(default)]
    direction_degrees: u16,
    #[serde(default)]
    speed_knots: u8,
}

#[derive(Debug, Default, Deserialize)]
struct HealthReport {
    /// `null` when the daemon runs `--no-ingest`. That is the archive-only
    /// signal, and it is the only one -- the status field still says "ok".
    #[serde(default)]
    ingest: Option<serde_json::Value>,
}

/// Parse a `/v1/timeline` body. Public so the shape can be tested against a
/// captured response without a live daemon.
pub fn parse_timeline(body: &str) -> Result<Vec<WarningRecord>, String> {
    let report: TimelineReport =
        serde_json::from_str(body).map_err(|error| format!("timeline JSON: {error}"))?;
    Ok(report.records.into_iter().map(convert_timeline).collect())
}

fn convert_timeline(record: TimelineRecord) -> WarningRecord {
    let points: Vec<(f32, f32)> = record
        .polygon
        .map(|polygon| {
            polygon
                .points
                .into_iter()
                .filter(|point| on_globe(point.lon, point.lat))
                .map(|point| (point.lon, point.lat))
                .collect()
        })
        .unwrap_or_default();

    // Resolved ONCE, from whichever field the daemon populated -- see
    // `WireTags::threat`. Both the severity tier and the map colour read it,
    // and they must not disagree.
    let damage_threat = record.tags.threat();
    let severity = severity_of(
        &record.phenomenon,
        &record.significance,
        record.tags.tornado.as_deref(),
        damage_threat.as_deref(),
        record.tags.flash_flood_emergency,
    );

    WarningRecord {
        event_id: record.event_id,
        office: record.office,
        phenomenon: record.phenomenon,
        significance: record.significance,
        action: record.action,
        event_family: record.event_family,
        headline: record.headline,
        // The daemon already emits UTC, but normalising anyway costs nothing
        // and keeps the comparison rule true for both sources.
        valid_start: record.valid_start.as_deref().and_then(to_utc_rfc3339),
        valid_end: record.valid_end.as_deref().and_then(to_utc_rfc3339),
        severity,
        tornado: record.tags.tornado,
        hail_inches: record.tags.hail_inches,
        wind_mph: record.tags.wind_mph,
        damage_threat,
        bbox: bounds_of(&points),
        points,
        motion: record
            .time_mot_loc
            .map(|motion| (motion.direction_degrees, motion.speed_knots)),
        ugcs: record.ugcs,
    }
}

// ---------------------------------------------------------------------------
// api.weather.gov wire types
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Deserialize)]
struct AlertCollection {
    #[serde(default)]
    features: Vec<AlertFeature>,
}

#[derive(Debug, Default, Deserialize)]
struct AlertFeature {
    #[serde(default)]
    geometry: Option<AlertGeometry>,
    #[serde(default)]
    properties: AlertProperties,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum AlertGeometry {
    Polygon {
        #[serde(default)]
        coordinates: Vec<Vec<Vec<f64>>>,
    },
    MultiPolygon {
        #[serde(default)]
        coordinates: Vec<Vec<Vec<Vec<f64>>>>,
    },
    #[serde(other)]
    Other,
}

impl AlertGeometry {
    /// The outer ring. A warning polygon is one ring in practice; a
    /// MultiPolygon takes the first, because drawing only part of a hazard is
    /// better than drawing a line between its pieces.
    fn outer_ring(&self) -> &[Vec<f64>] {
        match self {
            Self::Polygon { coordinates } => coordinates.first().map(Vec::as_slice).unwrap_or(&[]),
            Self::MultiPolygon { coordinates } => coordinates
                .first()
                .and_then(|polygon| polygon.first())
                .map(Vec::as_slice)
                .unwrap_or(&[]),
            Self::Other => &[],
        }
    }
}

#[derive(Debug, Default, Deserialize)]
struct AlertProperties {
    #[serde(default)]
    event: String,
    #[serde(default)]
    headline: Option<String>,
    #[serde(default)]
    onset: Option<String>,
    #[serde(default)]
    effective: Option<String>,
    #[serde(default)]
    ends: Option<String>,
    #[serde(default)]
    expires: Option<String>,
    #[serde(default)]
    geocode: AlertGeocode,
    /// Free-text bulletin values, always wrapped in an array. See module note 6.
    #[serde(default)]
    parameters: BTreeMap<String, Vec<serde_json::Value>>,
}

#[derive(Debug, Default, Deserialize)]
struct AlertGeocode {
    #[serde(rename = "UGC", default)]
    ugc: Vec<String>,
}

impl AlertProperties {
    /// First value of a parameter, as text.
    fn parameter(&self, key: &str) -> Option<&str> {
        self.parameters
            .get(key)?
            .first()
            .and_then(serde_json::Value::as_str)
    }
}

/// One parsed P-VTEC string.
struct Vtec {
    action: String,
    office: String,
    phenomenon: String,
    significance: String,
    event_tracking_number: String,
}

/// Parse a P-VTEC string such as
/// `/O.NEW.KRLX.FF.W.0177.260817T2108Z-260818T0015Z/`.
///
/// Only the fixed-width head is read; the time range is deliberately ignored
/// because `onset`/`ends` carry the same instants in a form chrono can parse.
fn parse_vtec(raw: &str) -> Option<Vtec> {
    let fields: Vec<&str> = raw.trim().trim_matches('/').split('.').collect();
    if fields.len() < 6 {
        return None;
    }
    Some(Vtec {
        action: fields[1].to_owned(),
        office: fields[2].to_owned(),
        phenomenon: fields[3].to_owned(),
        significance: fields[4].to_owned(),
        event_tracking_number: fields[5].to_owned(),
    })
}

/// Phenomenon and significance for a product with no VTEC.
///
/// Statements (`SPS`, marine weather statements) carry polygons but no VTEC, so
/// the event name is the only classifier available. They are given significance
/// `S`, which keeps them below every warning in [`severity_of`].
fn classify_by_event_name(event: &str) -> (String, String) {
    let upper = event.to_ascii_uppercase();
    let phenomenon = if upper.contains("TORNADO") {
        "TO"
    } else if upper.contains("SEVERE THUNDERSTORM") {
        "SV"
    } else if upper.contains("FLASH FLOOD") {
        "FF"
    } else if upper.contains("FLOOD") {
        "FA"
    } else if upper.contains("SPECIAL MARINE") {
        "MA"
    } else if upper.contains("MARINE") {
        "MWS"
    } else if upper.contains("FIRE") || upper.contains("RED FLAG") {
        "FW"
    } else if upper.contains("SNOW SQUALL") {
        "SQ"
    } else {
        "SPS"
    };
    let significance = if upper.contains("WARNING") {
        "W"
    } else if upper.contains("WATCH") {
        "A"
    } else if upper.contains("ADVISORY") {
        "Y"
    } else {
        "S"
    };
    (phenomenon.to_owned(), significance.to_owned())
}

/// Hail size in inches from the bulletin's free text.
///
/// The value arrives as `"1.00"`, as `"Up to .75"`, and as `"0.00"` when the
/// forecaster tagged no hail. All three were observed live on 2026-08-17.
/// `0.00` becomes `None`, because "0.00 in hail" is not a thing to draw.
fn parse_hail_inches(raw: &str) -> Option<f32> {
    let text = raw.trim();
    let numeric = text
        .rsplit(|c: char| !(c.is_ascii_digit() || c == '.'))
        .find(|token| !token.is_empty())?;
    let inches: f32 = numeric.parse().ok()?;
    (inches.is_finite() && inches > 0.0).then_some(inches)
}

/// Gust in mph from `"60 MPH"`.
fn parse_wind_mph(raw: &str) -> Option<u16> {
    let digits: String = raw
        .trim()
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    digits.parse().ok()
}

/// Storm motion from `eventMotionDescription`, e.g.
/// `2026-08-17T21:16:00-00:00...storm...002DEG...8KT...30.72,-89.01`.
///
/// The fields are `...`-separated, so the direction and speed are found by
/// their unit suffix rather than by position -- the leading timestamp's own
/// offset can contain digits and the location trails it.
fn parse_event_motion(raw: &str) -> Option<(u16, u8)> {
    let mut direction = None;
    let mut speed = None;
    for field in raw.split("...") {
        let field = field.trim();
        if let Some(value) = field.strip_suffix("DEG") {
            direction = value.trim().parse().ok();
        } else if let Some(value) = field.strip_suffix("KT") {
            speed = value.trim().parse().ok();
        }
    }
    Some((direction?, speed?))
}

/// Parse an `api.weather.gov/alerts/active` GeoJSON body. Public so the shape
/// can be tested against a captured response without a network.
pub fn parse_weather_gov_alerts(body: &str) -> Result<Vec<WarningRecord>, String> {
    let collection: AlertCollection =
        serde_json::from_str(body).map_err(|error| format!("alerts JSON: {error}"))?;
    Ok(collection
        .features
        .into_iter()
        .map(convert_alert)
        .collect::<Vec<_>>())
}

fn convert_alert(feature: AlertFeature) -> WarningRecord {
    let properties = feature.properties;
    let vtec = properties.parameter("VTEC").and_then(parse_vtec);
    let (phenomenon, significance) = match &vtec {
        Some(vtec) => (vtec.phenomenon.clone(), vtec.significance.clone()),
        None => classify_by_event_name(&properties.event),
    };
    let office = vtec
        .as_ref()
        .map(|vtec| vtec.office.clone())
        .or_else(|| {
            properties
                .parameter("WMOidentifier")
                .and_then(|wmo| wmo.split_whitespace().nth(1).map(str::to_owned))
        })
        .unwrap_or_default();
    let action = vtec
        .as_ref()
        .map(|vtec| vtec.action.clone())
        .unwrap_or_else(|| "NEW".to_owned());
    let event_id = match &vtec {
        Some(vtec) => format!(
            "{}.O.{}.{}.{}",
            vtec.office, vtec.phenomenon, vtec.significance, vtec.event_tracking_number
        ),
        // No VTEC means no stable id; the AWIPS id plus the office is the
        // closest thing, and it is only used for display and de-duplication.
        None => format!(
            "{office}.{}",
            properties.parameter("AWIPSidentifier").unwrap_or("?")
        ),
    };

    // GeoJSON coordinates are [lon, lat], which is already this workspace's
    // order. The ring's repeated closing vertex is dropped: the renderer closes
    // the outline itself, and a duplicate vertex is a zero-length segment.
    let mut points: Vec<(f32, f32)> = feature
        .geometry
        .iter()
        .flat_map(|geometry| geometry.outer_ring())
        .filter_map(|vertex| {
            let lon = *vertex.first()? as f32;
            let lat = *vertex.get(1)? as f32;
            on_globe(lon, lat).then_some((lon, lat))
        })
        .collect();
    if points.len() > 2 && points.first() == points.last() {
        points.pop();
    }

    // Whichever threat line this product carries. Only one is ever populated,
    // and severity and colour both read the resolved value.
    let damage_threat = ["tornadoDamageThreat", "thunderstormDamageThreat"]
        .iter()
        .chain(["flashFloodDamageThreat"].iter())
        .find_map(|key| properties.parameter(key))
        .map(str::to_owned);

    let tornado = properties.parameter("tornadoDetection").map(str::to_owned);
    let severity = severity_of(
        &phenomenon,
        &significance,
        tornado.as_deref(),
        damage_threat.as_deref(),
        // A flash-flood emergency is tagged as a CATASTROPHIC flash-flood
        // damage threat, which `severity_of` already raises to Extreme; there
        // is no separate emergency field in this feed.
        false,
    );

    WarningRecord {
        event_id,
        office,
        phenomenon,
        significance,
        action,
        event_family: properties.event.clone(),
        headline: properties.headline.clone(),
        // Local-offset timestamps, normalised to UTC. See module note 5.
        valid_start: properties
            .onset
            .as_deref()
            .or(properties.effective.as_deref())
            .and_then(to_utc_rfc3339),
        valid_end: properties
            .ends
            .as_deref()
            .or(properties.expires.as_deref())
            .and_then(to_utc_rfc3339),
        severity,
        tornado,
        hail_inches: properties
            .parameter("maxHailSize")
            .and_then(parse_hail_inches),
        wind_mph: properties.parameter("maxWindGust").and_then(parse_wind_mph),
        damage_threat,
        bbox: bounds_of(&points),
        points,
        motion: properties
            .parameter("eventMotionDescription")
            .and_then(parse_event_motion),
        ugcs: properties.geocode.ugc,
    }
}

// ---------------------------------------------------------------------------
// Fetching
// ---------------------------------------------------------------------------

/// The result of one poll: a state to show and, when anything answered, the
/// records behind it.
pub struct WarningsFetch {
    pub state: WarningsState,
    pub records: Option<Vec<WarningRecord>>,
}

fn warnings_http_client() -> Result<reqwest::blocking::Client, String> {
    reqwest::blocking::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(REQUEST_TIMEOUT)
        .user_agent(crate::http_user_agent())
        .build()
        .map_err(|error| format!("HTTP client: {error}"))
}

/// Count what is in force now, so the chip reports the picture rather than the
/// archive size.
fn count_active(records: &[WarningRecord]) -> usize {
    let now = chrono::Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true);
    records
        .iter()
        .filter(|record| record.is_active_at(&now))
        .count()
}

/// One round trip. Blocking, bounded, and never panics -- it runs on a worker
/// thread and reports every failure as text.
pub fn fetch_warnings(source: &WarningsSource) -> WarningsFetch {
    let client = match warnings_http_client() {
        Ok(client) => client,
        Err(reason) => {
            return WarningsFetch {
                state: WarningsState::Offline { reason },
                records: None,
            };
        }
    };
    match source {
        WarningsSource::Daemon { base_url } => fetch_from_daemon(&client, base_url),
        WarningsSource::WeatherGov => fetch_from_weather_gov(&client),
        WarningsSource::Auto { daemon_base_url } => {
            let attempt = fetch_from_daemon(&client, daemon_base_url);
            if attempt.state.is_offline() {
                // The daemon is the better source but it is not the one most
                // machines have. Falling back silently is the difference
                // between a warnings layer and an error chip.
                fetch_from_weather_gov(&client)
            } else {
                attempt
            }
        }
    }
}

fn fetch_from_daemon(client: &reqwest::blocking::Client, base_url: &str) -> WarningsFetch {
    let base_url = base_url.trim_end_matches('/');
    let offline = |reason: String| WarningsFetch {
        state: WarningsState::Offline { reason },
        records: None,
    };

    let ingesting = match client.get(format!("{base_url}/healthz")).send() {
        Ok(response) if response.status().is_success() => match response
            .text()
            .map_err(|error| error.to_string())
            .and_then(|body| {
                serde_json::from_str::<HealthReport>(&body).map_err(|error| error.to_string())
            }) {
            Ok(health) => health.ingest.is_some(),
            Err(error) => return offline(format!("unreadable /healthz: {error}")),
        },
        Ok(response) => {
            return offline(format!(
                "daemon returned {} for /healthz",
                response.status()
            ));
        }
        Err(error) => return offline(daemon_offline_reason(base_url, &error)),
    };

    let body = match client
        .get(format!("{base_url}/v1/timeline"))
        // The daemon treats `days` as a SYMMETRIC window around the reference
        // instant clamped forward to today -- it is not a plain lookback.
        .query(&[("days", "1")])
        .send()
    {
        Ok(response) if response.status().is_success() => match response.text() {
            Ok(body) => body,
            Err(error) => return offline(format!("unreadable /v1/timeline: {error}")),
        },
        Ok(response) => {
            return offline(format!(
                "daemon returned {} for /v1/timeline",
                response.status()
            ));
        }
        Err(error) => return offline(daemon_offline_reason(base_url, &error)),
    };

    match parse_timeline(&body) {
        Ok(records) => {
            let active = count_active(&records);
            WarningsFetch {
                state: if ingesting {
                    WarningsState::Live { active }
                } else {
                    WarningsState::ArchiveOnly { active }
                },
                records: Some(records),
            }
        }
        Err(reason) => offline(reason),
    }
}

fn fetch_from_weather_gov(client: &reqwest::blocking::Client) -> WarningsFetch {
    let offline = |reason: String| WarningsFetch {
        state: WarningsState::Offline { reason },
        records: None,
    };
    let body = match client.get(WEATHER_GOV_ALERTS_URL).send() {
        Ok(response) if response.status().is_success() => match response.text() {
            Ok(body) => body,
            Err(error) => return offline(format!("unreadable alerts feed: {error}")),
        },
        Ok(response) => {
            return offline(format!(
                "api.weather.gov returned {} for active alerts",
                response.status()
            ));
        }
        Err(error) => return offline(format!("api.weather.gov unreachable: {error}")),
    };
    match parse_weather_gov_alerts(&body) {
        Ok(records) => {
            let active = count_active(&records);
            WarningsFetch {
                state: WarningsState::Public { active },
                records: Some(records),
            }
        }
        Err(reason) => offline(reason),
    }
}

/// Turn a transport failure into words an operator can act on. "connection
/// refused" means start the daemon; a timeout means it is wedged.
fn daemon_offline_reason(base_url: &str, error: &reqwest::Error) -> String {
    if error.is_connect() {
        format!("no nwws-rs daemon at {base_url} -- start `nwws serve`")
    } else if error.is_timeout() {
        format!("nwws-rs daemon at {base_url} did not answer in time")
    } else {
        format!("nwws-rs daemon at {base_url}: {error}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A verbatim `/v1/timeline` response from a real `nwws serve --no-ingest`
    /// run over an archived NWS tornado bulletin, trimmed to one record.
    const REAL_TIMELINE: &str = r#"{
      "errors": [], "failures": 0, "messages": 2, "parsed_files": 2,
      "query_time_utc": "2026-08-16T12:00:00Z", "scanned_files": 2, "warning_records": 2,
      "records": [{
        "action": "NEW", "awips_id": "TORLOT", "event_class": "O",
        "event_family": "tornado", "event_id": "KLOT.O.TO.W.0001",
        "event_tracking_number": 1, "expires_at": "2026-04-21T16:30:00Z",
        "headline": "BULLETIN - EAS ACTIVATION REQUESTED",
        "lifecycle_status": "future", "office": "KLOT", "phenomenon": "TO",
        "polygon": {"points": [
          {"lat": 42.15, "lon": -88.5}, {"lat": 42.03, "lon": -88.2},
          {"lat": 41.94, "lon": -88.1}, {"lat": 41.98, "lon": -87.86},
          {"lat": 42.13, "lon": -87.84}, {"lat": 42.22, "lon": -88.39}
        ], "raw": "LAT...LON 4215 8850 4203 8820 4194 8810 4198 8786 4213 8784 4222 8839"},
        "significance": "W",
        "tags": {"damage_threat": null, "flash_flood_emergency": false,
                 "flash_flood_observed": false, "hail_inches": 1.0,
                 "tornado": "RADAR INDICATED", "wind_mph": null},
        "time_mot_loc": {"direction_degrees": 265, "speed_knots": 31,
                         "raw": "TIME...MOT...LOC 1600Z 265DEG 31KT 4208 8837",
                         "time": "1600Z", "locations": []},
        "ugcs": ["ILC031", "ILC043", "ILC197"],
        "valid_end": "2026-04-21T16:30:00Z", "valid_start": "2026-04-21T16:00:00Z",
        "vtec": "/O.NEW.KLOT.TO.W.0001.260421T1600Z-260421T1630Z/"
      }]
    }"#;

    /// A REAL flash-flood response from `nwws serve`, trimmed to the fields
    /// that matter. Captured 2026-08-16 from a live daemon.
    ///
    /// The point of it: `tags.damage_threat` is NULL and the threat appears
    /// only in `text_tags`.
    const REAL_FLASH_FLOOD: &str = r#"{
      "records": [{
        "event_id": "KOUN.O.FF.W.0044", "office": "KOUN",
        "phenomenon": "FF", "significance": "W", "action": "NEW",
        "event_family": "flash-flood",
        "valid_start": "2026-08-16T23:13:00Z", "valid_end": "2026-08-17T00:13:00Z",
        "tags": {
          "damage_threat": null,
          "flash_flood_emergency": false,
          "tornado": null,
          "text_tags": [
            {"kind": "source", "normalized_value": "RADAR INDICATED.",
             "raw_name": "SOURCE", "raw_value": "Radar indicated."},
            {"kind": "flash_flood_damage_threat", "normalized_value": "CONSIDERABLE",
             "raw_name": "FLASH FLOOD DAMAGE THREAT", "raw_value": "CONSIDERABLE"}
          ]
        },
        "polygon": {"points": [
          {"lat": 34.90, "lon": -98.20}, {"lat": 34.90, "lon": -97.70},
          {"lat": 34.50, "lon": -97.75}, {"lat": 34.52, "lon": -98.25}
        ]},
        "ugcs": ["OKC051", "OKC125"]
      }]
    }"#;

    /// A verbatim feature from `api.weather.gov/alerts/active?status=actual`,
    /// captured 2026-08-17T21:16Z. Nothing is edited: the local-offset
    /// timestamps, the array-wrapped parameters, the `"Up to .75"` hail and the
    /// `"60 MPH"` gust are all exactly as the feed served them.
    const REAL_WEATHER_GOV: &str = r#"{
      "type": "FeatureCollection",
      "features": [{
        "id": "urn:oid:2.49.0.1.840.0.e15fd1d2bf59b9b166b1b9e88e2f99b2949de3b1.001.1",
        "type": "Feature",
        "geometry": {"type": "Polygon", "coordinates": [[
          [-89.08, 30.8], [-88.9, 30.79], [-88.88, 30.71],
          [-88.88, 30.68], [-89.09, 30.7], [-89.08, 30.8]
        ]]},
        "properties": {
          "areaDesc": "Stone, MS",
          "geocode": {"SAME": ["028131"], "UGC": ["MSC131"]},
          "sent": "2026-08-17T16:16:00-05:00",
          "effective": "2026-08-17T16:16:00-05:00",
          "onset": "2026-08-17T16:16:00-05:00",
          "expires": "2026-08-17T17:00:00-05:00",
          "ends": "2026-08-17T17:00:00-05:00",
          "status": "Actual", "messageType": "Alert", "category": "Met",
          "severity": "Severe", "certainty": "Observed", "urgency": "Immediate",
          "event": "Severe Thunderstorm Warning",
          "senderName": "NWS Mobile AL",
          "headline": "Severe Thunderstorm Warning issued August 17 at 4:16PM CDT until August 17 at 5:00PM CDT by NWS Mobile AL",
          "parameters": {
            "AWIPSidentifier": ["SVRMOB"],
            "WMOidentifier": ["WUUS54 KMOB 172116"],
            "eventMotionDescription": ["2026-08-17T21:16:00-00:00...storm...002DEG...8KT...30.72,-89.01"],
            "windThreat": ["RADAR INDICATED"],
            "maxWindGust": ["60 MPH"],
            "hailThreat": ["RADAR INDICATED"],
            "maxHailSize": ["Up to .75"],
            "BLOCKCHANNEL": ["EAS", "NWEM", "CMAS"],
            "VTEC": ["/O.NEW.KMOB.SV.W.0221.260817T2116Z-260817T2200Z/"],
            "eventEndingTime": ["2026-08-17T22:00:00+00:00"]
          },
          "eventCode": {"SAME": ["SVR"], "NationalWeatherService": ["SVW"]}
        }
      }]
    }"#;

    #[test]
    fn a_damage_threat_that_lands_only_in_text_tags_is_still_found() {
        let records = parse_timeline(REAL_FLASH_FLOOD).expect("parses");
        assert_eq!(records.len(), 1);
        assert_eq!(
            records[0].damage_threat.as_deref(),
            Some("CONSIDERABLE"),
            "the threat is in text_tags, not tags.damage_threat"
        );
        assert_eq!(records[0].points.len(), 4);
    }

    /// The top-level field still WINS when the daemon populates it -- the
    /// text_tags scan is a fallback, not a replacement.
    #[test]
    fn the_top_level_damage_threat_wins_over_text_tags() {
        let body = r#"{"records":[{
          "event_id":"KOUN.O.TO.W.0011","office":"KOUN",
          "phenomenon":"TO","significance":"W","action":"NEW",
          "event_family":"tornado",
          "tags":{"damage_threat":"CATASTROPHIC","tornado":"OBSERVED",
                  "text_tags":[{"kind":"tornado_damage_threat",
                                "normalized_value":"CONSIDERABLE"}]}
        }]}"#;
        let records = parse_timeline(body).expect("parses");
        assert_eq!(records[0].damage_threat.as_deref(), Some("CATASTROPHIC"));
        assert_eq!(records[0].severity, Severity::Extreme);
    }

    #[test]
    fn parses_a_real_daemon_response_including_geometry_and_motion() {
        let records = parse_timeline(REAL_TIMELINE).expect("real response parses");
        assert_eq!(records.len(), 1);
        let record = &records[0];
        assert_eq!(record.event_id, "KLOT.O.TO.W.0001");
        assert_eq!(record.phenomenon, "TO");
        assert_eq!(record.tornado.as_deref(), Some("RADAR INDICATED"));
        assert_eq!(record.hail_inches, Some(1.0));
        assert_eq!(record.motion, Some((265, 31)));
        assert_eq!(record.ugcs.len(), 3);
        assert_eq!(record.points.len(), 6);
        // (lon, lat), western hemisphere, already signed by the HTTP path.
        assert_eq!(record.points[0], (-88.5, 42.15));
        let bbox = record.bbox.expect("polygon has a bbox");
        assert!(bbox[0] <= -88.5 && bbox[2] >= -87.84);
        assert!(bbox[1] <= 41.94 && bbox[3] >= 42.22);
        assert_eq!(record.severity, Severity::Severe);
    }

    #[test]
    fn unknown_fields_and_missing_fields_do_not_black_out_the_layer() {
        let sparse = r#"{"records":[{"event_id":"X","brand_new_field":42}]}"#;
        let records = parse_timeline(sparse).expect("permissive parse");
        assert_eq!(records.len(), 1);
        assert!(records[0].points.is_empty());
        assert!(records[0].bbox.is_none());
        assert_eq!(records[0].severity, Severity::Minor);

        // A body that is not a timeline at all fails loudly rather than
        // silently returning nothing.
        assert!(parse_timeline("not json").is_err());
        // An empty archive is a valid answer, not an error.
        assert_eq!(parse_timeline(r#"{"records":[]}"#).unwrap().len(), 0);
    }

    #[test]
    fn off_globe_vertices_are_dropped_rather_than_drawn() {
        let body = r#"{"records":[{"event_id":"X","polygon":{"points":[
            {"lat":42.0,"lon":-88.0},{"lat":999.0,"lon":-88.0},{"lat":42.0,"lon":-500.0}
        ]}}]}"#;
        let records = parse_timeline(body).unwrap();
        assert_eq!(records[0].points, vec![(-88.0, 42.0)]);
    }

    #[test]
    fn severity_ranks_the_way_an_operator_expects() {
        assert_eq!(
            severity_of("TO", "W", Some("OBSERVED"), None, false),
            Severity::Extreme
        );
        assert_eq!(
            severity_of("TO", "W", Some("RADAR INDICATED"), None, false),
            Severity::Severe
        );
        assert_eq!(
            severity_of("TO", "W", None, Some("CATASTROPHIC"), false),
            Severity::Extreme
        );
        assert_eq!(
            severity_of("SV", "W", None, None, false),
            Severity::Moderate
        );
        assert_eq!(
            severity_of("SV", "W", None, Some("DESTRUCTIVE"), false),
            Severity::Severe
        );
        assert_eq!(severity_of("FF", "W", None, None, true), Severity::Extreme);
        assert_eq!(severity_of("TO", "A", None, None, false), Severity::Minor);
        // Ordering: Extreme sorts before Minor.
        let mut order = vec![Severity::Minor, Severity::Extreme, Severity::Moderate];
        order.sort();
        assert_eq!(
            order,
            vec![Severity::Extreme, Severity::Moderate, Severity::Minor]
        );
    }

    #[test]
    fn cancelled_and_expired_events_are_never_active() {
        let base = parse_timeline(REAL_TIMELINE).unwrap().remove(0);
        assert!(base.is_active_at("2026-04-21T16:10:00Z"));
        assert!(!base.is_active_at("2026-04-21T15:50:00Z"), "before start");
        assert!(!base.is_active_at("2026-04-21T16:40:00Z"), "after end");

        for action in ["CAN", "EXP", "UPG"] {
            let mut record = base.clone();
            record.action = action.to_owned();
            assert!(!record.is_active_at("2026-04-21T16:10:00Z"), "{action}");
        }
    }

    #[test]
    fn the_library_longitude_footgun_has_a_named_fix() {
        // 8850 in the bulletin is 88.50 W.
        assert_eq!(normalize_bulletin_longitude(88.5, "KLOT"), -88.5);
        // Values under 40 lost their hundreds digit: 3.5 is 103.5 W.
        assert_eq!(normalize_bulletin_longitude(3.5, "KABQ"), -103.5);
        // Guam is the one office east of the meridian.
        assert_eq!(normalize_bulletin_longitude(144.8, "PGUM"), 144.8);
    }

    // -- api.weather.gov -----------------------------------------------------

    #[test]
    fn parses_a_real_weather_gov_alert() {
        let records = parse_weather_gov_alerts(REAL_WEATHER_GOV).expect("real feed parses");
        assert_eq!(records.len(), 1);
        let record = &records[0];
        assert_eq!(record.event_id, "KMOB.O.SV.W.0221");
        assert_eq!(record.office, "KMOB");
        assert_eq!(record.phenomenon, "SV");
        assert_eq!(record.significance, "W");
        assert_eq!(record.action, "NEW");
        assert_eq!(record.ugcs, vec!["MSC131".to_owned()]);
        assert_eq!(record.severity, Severity::Moderate);
        // The repeated closing vertex is dropped: six in the feed, five here.
        assert_eq!(record.points.len(), 5);
        assert_eq!(record.points[0], (-89.08, 30.8));
    }

    /// Byte-wise RFC3339 comparison is only ordered inside one zone. The feed
    /// serves local offsets, so a record that is in force must not read as
    /// expired once it is compared against a UTC "now".
    #[test]
    fn local_offset_timestamps_are_normalised_to_utc() {
        let record = &parse_weather_gov_alerts(REAL_WEATHER_GOV).unwrap()[0];
        assert_eq!(record.valid_start.as_deref(), Some("2026-08-17T21:16:00Z"));
        assert_eq!(record.valid_end.as_deref(), Some("2026-08-17T22:00:00Z"));
        assert!(record.is_active_at("2026-08-17T21:30:00Z"));
        assert!(!record.is_active_at("2026-08-17T21:00:00Z"));
        assert!(!record.is_active_at("2026-08-17T22:30:00Z"));

        // The un-normalised string would have compared as 16:16, which is
        // BEFORE a 21:30 "now" and after a 21:00 one -- both wrong.
        assert!("2026-08-17T16:16:00-05:00" < "2026-08-17T21:00:00Z");
    }

    /// Free text, exactly as the bulletin writes it. All three hail spellings
    /// and the gust suffix were observed live on 2026-08-17.
    #[test]
    fn bulletin_free_text_measurements_are_read_in_every_spelling_seen() {
        assert_eq!(parse_hail_inches("Up to .75"), Some(0.75));
        assert_eq!(parse_hail_inches("0.75"), Some(0.75));
        assert_eq!(parse_hail_inches("1.00"), Some(1.0));
        // A forecaster tagging no hail is not a 0.00 inch hailstone.
        assert_eq!(parse_hail_inches("0.00"), None);
        assert_eq!(parse_hail_inches(""), None);

        assert_eq!(parse_wind_mph("60 MPH"), Some(60));
        assert_eq!(parse_wind_mph("70 mph"), Some(70));
        assert_eq!(parse_wind_mph("unknown"), None);

        let record = &parse_weather_gov_alerts(REAL_WEATHER_GOV).unwrap()[0];
        assert_eq!(record.hail_inches, Some(0.75));
        assert_eq!(record.wind_mph, Some(60));
    }

    /// The direction and speed are found by unit suffix, not position: the
    /// leading timestamp contains digits and its own offset.
    #[test]
    fn storm_motion_survives_the_timestamp_in_front_of_it() {
        assert_eq!(
            parse_event_motion("2026-08-17T21:16:00-00:00...storm...002DEG...8KT...30.72,-89.01"),
            Some((2, 8))
        );
        assert_eq!(
            parse_event_motion("2026-08-17T21:15:00-00:00...storm...272DEG...38KT...35.01,-90.46"),
            Some((272, 38))
        );
        assert_eq!(parse_event_motion("no motion here"), None);

        let record = &parse_weather_gov_alerts(REAL_WEATHER_GOV).unwrap()[0];
        assert_eq!(record.motion, Some((2, 8)));
    }

    /// 362 of the 491 alerts live on 2026-08-17 had no geometry at all, and 42
    /// of the ones that did had no VTEC. Neither may drop a record or panic.
    #[test]
    fn alerts_without_geometry_or_vtec_still_parse() {
        let body = r#"{"features":[
          {"geometry":null,"properties":{"event":"Heat Advisory",
            "parameters":{"VTEC":["/O.CON.KPSR.HT.Y.0009.000000T0000Z-260819T0400Z/"]}}},
          {"geometry":{"type":"Polygon","coordinates":[[[-108.9,46.5],[-108.6,46.5],[-108.6,46.3]]]},
           "properties":{"event":"Special Weather Statement",
            "parameters":{"AWIPSidentifier":["SPSBYZ"],"WMOidentifier":["WWUS75 KBYZ 172114"]}}}
        ]}"#;
        let records = parse_weather_gov_alerts(body).expect("parses");
        assert_eq!(records.len(), 2);

        assert!(records[0].points.is_empty());
        assert_eq!(records[0].phenomenon, "HT");
        assert_eq!(records[0].action, "CON");

        // No VTEC: classified from the event name, and given statement
        // significance so it can never outrank a warning.
        assert_eq!(records[1].phenomenon, "SPS");
        assert_eq!(records[1].significance, "S");
        assert_eq!(records[1].severity, Severity::Minor);
        assert_eq!(
            records[1].office, "KBYZ",
            "office recovered from the WMO id"
        );
        assert_eq!(records[1].points.len(), 3);
    }

    #[test]
    fn event_names_classify_the_products_that_carry_polygons() {
        for (event, phenomenon, significance) in [
            ("Tornado Warning", "TO", "W"),
            ("Severe Thunderstorm Warning", "SV", "W"),
            ("Tornado Watch", "TO", "A"),
            ("Flash Flood Warning", "FF", "W"),
            ("Flood Advisory", "FA", "Y"),
            ("Special Marine Warning", "MA", "W"),
            ("Marine Weather Statement", "MWS", "S"),
            ("Special Weather Statement", "SPS", "S"),
            ("Snow Squall Warning", "SQ", "W"),
        ] {
            assert_eq!(
                classify_by_event_name(event),
                (phenomenon.to_owned(), significance.to_owned()),
                "{event}"
            );
        }
    }

    #[test]
    fn a_multipolygon_alert_draws_its_first_ring_rather_than_nothing() {
        let body = r#"{"features":[{"geometry":{"type":"MultiPolygon","coordinates":[
            [[[-97.5,35.2],[-97.0,35.2],[-97.0,35.6],[-97.5,35.2]]],
            [[[-90.0,40.0],[-89.0,40.0],[-89.0,41.0],[-90.0,40.0]]]
        ]},"properties":{"event":"Flood Warning"}}]}"#;
        let records = parse_weather_gov_alerts(body).expect("parses");
        assert_eq!(records[0].points.len(), 3);
        assert_eq!(records[0].points[0], (-97.5, 35.2));
    }

    #[test]
    fn a_configured_source_is_read_the_way_an_operator_would_write_it() {
        assert_eq!(WarningsSource::parse(""), WarningsSource::default());
        assert_eq!(WarningsSource::parse("off"), WarningsSource::WeatherGov);
        assert_eq!(WarningsSource::parse("NONE"), WarningsSource::WeatherGov);
        assert_eq!(
            WarningsSource::parse("http://warnings.example.invalid:8080/"),
            WarningsSource::Daemon {
                base_url: "http://warnings.example.invalid:8080".to_owned()
            },
            "a trailing slash must not become a double slash in the path"
        );
    }

    /// An absent daemon must resolve to actionable text within a bounded time,
    /// never hang. Port 1 is reserved and nothing listens there.
    #[test]
    fn a_missing_daemon_resolves_to_offline_with_actionable_text() {
        let client = warnings_http_client().expect("client builds");
        let started = std::time::Instant::now();
        let fetched = fetch_from_daemon(&client, "http://127.0.0.1:1");
        assert!(
            started.elapsed() < StdDuration::from_secs(20),
            "an absent daemon must resolve, not hang"
        );
        assert!(fetched.state.is_offline());
        assert!(fetched.records.is_none());
        let reason = fetched.state.detail();
        assert!(
            reason.contains("nwws serve") || reason.contains("did not answer"),
            "offline reason should tell the operator what to do: {reason}"
        );
    }
}

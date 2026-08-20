//! Public radar data-source helpers.

pub mod tuning;
pub mod warnings;

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::thread;
use std::time::{Duration as StdDuration, Instant, SystemTime, UNIX_EPOCH};

use chrono::{DateTime, Datelike, Duration, NaiveDate, NaiveDateTime, Utc};
use serde::Deserialize;
use thiserror::Error;

pub const LEVEL2_ARCHIVE_BUCKET: &str = "unidata-nexrad-level2";
pub const LEVEL2_CHUNKS_BUCKET: &str = "unidata-nexrad-level2-chunks";
const HTTP_CONNECT_TIMEOUT: StdDuration = StdDuration::from_secs(4);
const HTTP_METADATA_TIMEOUT: StdDuration = StdDuration::from_secs(8);
const HTTP_DOWNLOAD_TIMEOUT: StdDuration = StdDuration::from_secs(45);
const HTTP_USER_AGENT: &str = "GenericRadar/0.1 local-desktop";
const REALTIME_VOLUME_ID_MODULUS: u16 = 1000;
const REALTIME_CHUNK_LIST_MAX_KEYS: usize = 1000;
// The chunk batch size, the per-object attempt count and the pause between
// attempts moved to [`tuning`], where they are settable and fenced. Their
// shipped values are unchanged and live there as
// `tuning::DEFAULT_CHUNK_DOWNLOAD_BATCH`, `tuning::DEFAULT_DOWNLOAD_ATTEMPTS`
// and `tuning::DEFAULT_RETRY_BACKOFF`.
/// How many active volume ids to walk backwards before giving up on finding a
/// complete predecessor. Four covers a couple of aborted or skipped volumes
/// without turning a quiet site into a long chain of listings.
const REALTIME_PREVIOUS_VOLUME_LOOKBACK: usize = 4;
/// How far before the current volume a predecessor may start and still be
/// treated as "the previous volume".
///
/// This is the guard against the recycled-id trap, and it has to be generous
/// enough for the slowest clear-air VCP (VCP 31/32 run about 10 minutes per
/// volume; VCP 35 was observed at 7 minutes on KTLX) yet far shorter than the
/// bucket's retention, which is measured in days: on 2026-08-18 the KTLX
/// prefix held ids 1..=680 covering 2026-08-16T08:20Z to 2026-08-18T17:57Z.
/// Without this bound a wrapped id resolves to a two-day-old volume.
const REALTIME_PREVIOUS_VOLUME_MAX_GAP_MINUTES: i64 = 30;
const MIN_RECENT_LEVEL2_SITE_CATALOG_COUNT: usize = 100;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RadarDataLevel {
    Level2Archive,
    Level2RealtimeChunks,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DataSourceKind {
    LocalFile,
    LocalDirectory,
    PublicLevel2Archive,
    PublicLevel2RealtimeChunks,
    NceiArchive,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SourcePriority {
    pub sources: Vec<DataSourceKind>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct RadarSite {
    pub level2_id: String,
    pub name: Option<String>,
    pub latitude_deg: Option<f32>,
    pub longitude_deg: Option<f32>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct S3Object {
    pub key: String,
    pub size: u64,
    pub last_modified: Option<DateTime<Utc>>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DownloadedObject {
    pub object: S3Object,
    pub path: PathBuf,
    pub url: String,
    pub cache_hit: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LatestObject {
    pub object: S3Object,
    pub cache_hit: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RealtimeChunkType {
    Start,
    Intermediate,
    End,
}

impl RealtimeChunkType {
    fn from_code(value: &str) -> Option<Self> {
        match value {
            "S" => Some(Self::Start),
            "I" => Some(Self::Intermediate),
            "E" => Some(Self::End),
            _ => None,
        }
    }

    fn is_end(self) -> bool {
        matches!(self, Self::End)
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Start => "start",
            Self::Intermediate => "intermediate",
            Self::End => "end",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RealtimeChunkObject {
    pub object: S3Object,
    pub site: String,
    pub volume_id: u16,
    pub volume_time: DateTime<Utc>,
    pub chunk_id: u16,
    pub chunk_type: RealtimeChunkType,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RealtimeLevel2Volume {
    pub site: String,
    pub volume_id: u16,
    pub volume_time: DateTime<Utc>,
    pub chunks: Vec<RealtimeChunkObject>,
    pub complete: bool,
    pub total_size: u64,
}

impl RealtimeLevel2Volume {
    /// How far behind `now` this volume's start time is, never negative.
    ///
    /// See [`volume_age_at`]. This is the number a live display has to show:
    /// "newest in the feed" and "current" are different claims, and only this
    /// one can tell them apart.
    pub fn age_at(&self, now: DateTime<Utc>) -> Duration {
        volume_age_at(self.volume_time, now)
    }

    /// Whether a live session looking at this volume may still imply it is
    /// current. See [`classify_feed_age`].
    pub fn freshness_at(&self, now: DateTime<Utc>) -> FeedFreshness {
        classify_feed_age(self.age_at(now))
    }
}

/// How far behind wall clock the newest volume in a realtime feed may fall
/// before a live session must stop implying its picture is current.
///
/// Fifteen minutes, and the margin is deliberate on both sides.
///
/// The floor is the slowest legitimate case. A volume is aged from its START
/// time, so the newest volume time is already a whole volume behind by the
/// moment that volume finishes: VCP 12/212 run about 4.2 minutes, VCP 215
/// about 6, and the clear-air VCP 31/32 about 10 (VCP 35 measured at 7 on
/// KTLX - see [`REALTIME_PREVIOUS_VOLUME_MAX_GAP_MINUTES`]). Add the minute or
/// two between a chunk being written and a listing showing it, and a healthy
/// clear-air site can legitimately sit ~12 minutes behind wall clock. A
/// threshold under that would cry stall at a radar that is working perfectly.
///
/// The ceiling is what the alarm is FOR. On 2026-08-19 the chunks bucket had
/// stopped receiving KUEX: its id set was one contiguous run 1..=931 and the
/// newest chunk anywhere under `KUEX/` was `KUEX/931/20260816-110802-003-I`,
/// LastModified 2026-08-16T11:08:09Z - a three-day-old, three-chunk fragment
/// that the app downloaded and displayed under today's warning polygons
/// without a word. Anything between ~12 minutes and three days is a judgement
/// call; 15 minutes is the smallest round number that clears the slowest real
/// VCP, and every failure this guards against overshoots it by orders of
/// magnitude.
pub const REALTIME_FEED_STALL_AFTER_SECONDS: i64 = 15 * 60;

/// What a live session may claim about the picture it is publishing.
///
/// Two of these are ages and the third is a SOURCE, and that is the whole
/// shape of it. [`classify_feed_age`] answers the age question - may this be
/// presented as current, yes or no - and deliberately still has only those two
/// answers, because a middle "degraded" band would be a third thing to explain
/// on a status line an analyst reads in a glance.
///
/// [`Self::ArchiveFallback`] is not a third age band. It is the session saying
/// "the realtime chunk feed for this radar has stopped and I am publishing the
/// archive bucket instead", which no age can express: the archive picture may
/// be six minutes old and perfectly usable while the thing the app was built
/// to read has been dead for three days. Both facts have to reach the analyst,
/// so both are carried - the variant says which bucket, and the volume time
/// published beside it says how far behind.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FeedFreshness {
    /// The newest volume in the realtime chunk feed is recent enough to show
    /// as live.
    Current,
    /// The chunk feed has stopped keeping up and nothing better is available.
    /// The data is still real and still worth drawing; it must simply never be
    /// labelled as current.
    Stalled,
    /// The chunk feed has stopped and the session has switched to the Level II
    /// archive bucket, which is still receiving this radar. The volume time
    /// published with this is the ARCHIVE's newest, not the chunk feed's.
    ArchiveFallback,
}

impl FeedFreshness {
    /// Whether the realtime chunk feed has stopped keeping up.
    ///
    /// True for [`Self::ArchiveFallback`] as well, and that is the point: this
    /// is the "do not present this as a live chunk feed" bit, and being on the
    /// archive is the loudest possible case of the chunk feed having stopped.
    /// Every caller uses it to decide whether to raise a notice at all;
    /// [`Self::is_archive_fallback`] and [`Self::status_label`] are what change
    /// the words in that notice.
    pub fn is_stalled(self) -> bool {
        matches!(self, Self::Stalled | Self::ArchiveFallback)
    }

    /// Whether the picture is coming from the archive bucket rather than the
    /// realtime chunk feed.
    pub fn is_archive_fallback(self) -> bool {
        matches!(self, Self::ArchiveFallback)
    }

    /// The words for a status line. Lower case, no site and no age: the caller
    /// owns those, because it owns the clock the age is read against.
    pub fn status_label(self) -> &'static str {
        match self {
            Self::Current => "live",
            Self::Stalled => "feed stalled",
            Self::ArchiveFallback => "archive fallback",
        }
    }
}

/// How far behind `now` a volume that started at `volume_time` is.
///
/// Clamped at zero: a radar whose clock runs a few seconds ahead of this
/// machine's would otherwise produce a negative age, and "-3 s old" on a
/// status line reads as a bug in the app rather than as skew in a clock the
/// app does not own.
pub fn volume_age_at(volume_time: DateTime<Utc>, now: DateTime<Utc>) -> Duration {
    (now - volume_time).max(Duration::zero())
}

/// Classify a feed age against [`REALTIME_FEED_STALL_AFTER_SECONDS`].
///
/// An age question, so it has exactly two answers and never returns
/// [`FeedFreshness::ArchiveFallback`] - which source a session is reading is
/// a policy decision made above this function, not something an age can
/// reveal.
pub fn classify_feed_age(age: Duration) -> FeedFreshness {
    if age.num_seconds() >= REALTIME_FEED_STALL_AFTER_SECONDS {
        FeedFreshness::Stalled
    } else {
        FeedFreshness::Current
    }
}

#[derive(Debug, Error)]
pub enum DataSourceError {
    #[error("HTTP request failed: {0}")]
    Http(#[from] reqwest::Error),
    #[error("S3 XML parse failed: {0}")]
    Xml(#[from] quick_xml::DeError),
    #[error("JSON parse failed: {0}")]
    Json(#[from] serde_json::Error),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("no objects found for {bucket}/{prefix}")]
    NoObjects { bucket: String, prefix: String },
    #[error("downloaded {url} size mismatch: expected {expected} bytes, got {actual}")]
    DownloadSizeMismatch {
        url: String,
        expected: u64,
        actual: u64,
    },
    #[error("realtime chunk download worker panicked")]
    DownloadWorkerPanic,
    #[error("download of {site} volume {volume_id} was cancelled")]
    DownloadCancelled { site: String, volume_id: u16 },
    /// The single-object form of [`Self::DownloadCancelled`]. An archive
    /// volume has no volume id - it is one key - so it names the key instead
    /// rather than inventing a zero.
    #[error("download of {key} was cancelled")]
    ObjectDownloadCancelled { key: String },
    #[error(
        "{site} volume {volume_id} at {volume_time} is missing chunk {missing_chunk_id} of {last_chunk_id}"
    )]
    ChunkSetNotContiguous {
        site: String,
        volume_id: u16,
        volume_time: DateTime<Utc>,
        missing_chunk_id: u16,
        last_chunk_id: u16,
    },
}

pub type Result<T> = std::result::Result<T, DataSourceError>;

impl Default for SourcePriority {
    fn default() -> Self {
        Self {
            sources: vec![
                DataSourceKind::LocalFile,
                DataSourceKind::PublicLevel2Archive,
            ],
        }
    }
}

impl RadarSite {
    pub fn new(level2_id: impl Into<String>) -> Self {
        let level2_id = level2_id.into().to_ascii_uppercase();
        Self {
            level2_id,
            name: None,
            latitude_deg: None,
            longitude_deg: None,
        }
    }

    pub fn with_location(
        mut self,
        name: Option<String>,
        latitude_deg: Option<f32>,
        longitude_deg: Option<f32>,
    ) -> Self {
        self.name = name;
        self.latitude_deg = latitude_deg;
        self.longitude_deg = longitude_deg;
        self
    }
}

pub fn fallback_sites() -> Vec<RadarSite> {
    FALLBACK_SITE_IDS
        .iter()
        .map(|id| RadarSite::new(*id))
        .collect()
}

pub fn list_level2_sites_for_date(date: NaiveDate) -> Result<Vec<RadarSite>> {
    let prefix = format!("{:04}/{:02}/{:02}/", date.year(), date.month(), date.day());
    let listing = list_s3(LEVEL2_ARCHIVE_BUCKET, &prefix, Some("/"), None)?;
    let mut sites = listing
        .common_prefixes
        .into_iter()
        .filter_map(|prefix| {
            prefix
                .prefix
                .trim_end_matches('/')
                .rsplit('/')
                .next()
                .map(str::to_owned)
        })
        .filter(|site| !site.is_empty())
        .map(RadarSite::new)
        .collect::<Vec<_>>();
    sites.sort_by(|left, right| left.level2_id.cmp(&right.level2_id));
    sites.dedup_by(|left, right| left.level2_id == right.level2_id);
    Ok(sites)
}

pub fn list_recent_level2_sites(days_back: i64) -> Result<Vec<RadarSite>> {
    let today = Utc::now().date_naive();
    let mut sites_by_id = BTreeMap::<String, RadarSite>::new();
    for offset in 0..=days_back.max(0) {
        let date = today - Duration::days(offset);
        if let Ok(sites) = list_level2_sites_for_date(date) {
            for site in sites {
                sites_by_id.entry(site.level2_id.clone()).or_insert(site);
            }
            if sites_by_id.len() >= MIN_RECENT_LEVEL2_SITE_CATALOG_COUNT {
                break;
            }
        }
    }

    for site in fallback_sites() {
        sites_by_id.entry(site.level2_id.clone()).or_insert(site);
    }

    let mut sites = sites_by_id.into_values().collect::<Vec<_>>();
    sites.sort_by(|left, right| left.level2_id.cmp(&right.level2_id));
    Ok(sites)
}

pub fn fetch_weather_gov_radar_sites() -> Result<Vec<RadarSite>> {
    let client = metadata_http_client();
    let text = client
        .get("https://api.weather.gov/radar/stations")
        .send()?
        .error_for_status()?
        .text()?;
    let collection: WeatherGovFeatureCollection = serde_json::from_str(&text)?;
    let mut sites = collection
        .features
        .into_iter()
        .filter_map(|feature| {
            let id = feature.properties.id?;
            let coordinates = feature.geometry?.coordinates;
            if coordinates.len() < 2 {
                return None;
            }
            Some(RadarSite::new(id).with_location(
                feature.properties.name,
                Some(coordinates[1] as f32),
                Some(coordinates[0] as f32),
            ))
        })
        .collect::<Vec<_>>();
    sites.sort_by(|left, right| left.level2_id.cmp(&right.level2_id));
    sites.dedup_by(|left, right| left.level2_id == right.level2_id);
    Ok(sites)
}

pub fn fetch_text(url: &str) -> Result<String> {
    Ok(metadata_http_client()
        .get(url)
        .send()?
        .error_for_status()?
        .text()?)
}

pub fn fetch_level2_radar_sites(days_back: i64) -> Result<Vec<RadarSite>> {
    let weather_sites = fetch_weather_gov_radar_sites().unwrap_or_default();
    let weather_by_id = weather_sites
        .into_iter()
        .map(|site| (site.level2_id.clone(), site))
        .collect::<BTreeMap<_, _>>();

    let mut sites = list_recent_level2_sites(days_back).unwrap_or_else(|_| fallback_sites());
    for site in &mut sites {
        if let Some(weather_site) = weather_by_id.get(&site.level2_id) {
            site.name = weather_site.name.clone();
            site.latitude_deg = weather_site.latitude_deg;
            site.longitude_deg = weather_site.longitude_deg;
        }
    }
    sites.sort_by(|left, right| left.level2_id.cmp(&right.level2_id));
    sites.dedup_by(|left, right| left.level2_id == right.level2_id);
    Ok(sites)
}

pub fn latest_level2_object(site: &str, days_back: i64) -> Result<S3Object> {
    recent_level2_objects(site, days_back, 1)?
        .into_iter()
        .next()
        .ok_or_else(|| DataSourceError::NoObjects {
            bucket: LEVEL2_ARCHIVE_BUCKET.to_owned(),
            prefix: site.to_owned(),
        })
}

pub fn recent_level2_objects(
    site: &str,
    days_back: i64,
    max_count: usize,
) -> Result<Vec<S3Object>> {
    if max_count == 0 {
        return Ok(Vec::new());
    }

    let site = site.to_ascii_uppercase();
    let today = Utc::now().date_naive();
    let mut recent = Vec::with_capacity(max_count);
    for offset in 0..=days_back.max(0) {
        let date = today - Duration::days(offset);
        let prefix = format!(
            "{:04}/{:02}/{:02}/{}/",
            date.year(),
            date.month(),
            date.day(),
            site
        );
        let mut objects = list_s3(LEVEL2_ARCHIVE_BUCKET, &prefix, None, None)?
            .contents
            .into_iter()
            .filter(|object| object.size > 0 && !object.key.ends_with("_MDM"))
            .collect::<Vec<_>>();
        objects.sort_by(|left, right| left.key.cmp(&right.key));
        objects.reverse();
        for object in objects {
            recent.push(object);
            if recent.len() >= max_count {
                return Ok(recent);
            }
        }
    }
    if recent.is_empty() {
        Err(DataSourceError::NoObjects {
            bucket: LEVEL2_ARCHIVE_BUCKET.to_owned(),
            prefix: site,
        })
    } else {
        Ok(recent)
    }
}

pub fn latest_level2_object_cached(
    site: &str,
    days_back: i64,
    max_age: StdDuration,
) -> Result<LatestObject> {
    let site = site.to_ascii_uppercase();
    let days_back = days_back.max(0);
    let cache_key = LatestObjectCacheKey {
        site: site.clone(),
        days_back,
    };
    if let Ok(cache) = latest_object_cache().lock()
        && let Some(cached) = cache.get(&cache_key)
        && cached.fetched_at.elapsed() <= max_age
    {
        return Ok(LatestObject {
            object: cached.object.clone(),
            cache_hit: true,
        });
    }

    let object = latest_level2_object(&site, days_back)?;
    if let Ok(mut cache) = latest_object_cache().lock() {
        cache.insert(
            cache_key,
            CachedLatestObject {
                object: object.clone(),
                fetched_at: Instant::now(),
            },
        );
    }
    Ok(LatestObject {
        object,
        cache_hit: false,
    })
}

/// The newest volume the chunks bucket is holding for `site`.
///
/// NEWEST IS NOT CURRENT, and a caller that treats the two as the same word
/// will show days-old weather as live. This function answers "what is the most
/// recent thing in the feed"; it cannot answer "is the feed still running",
/// because a feed that stopped three days ago still has a most-recent thing in
/// it. On 2026-08-19 that is exactly what KUEX was: ids 1..=931 with nothing
/// written under the prefix since 2026-08-16T11:08:09Z, so this returned a
/// three-chunk fragment from Saturday and was right to.
///
/// Ask the returned volume [`RealtimeLevel2Volume::freshness_at`] before
/// presenting it as live. Doing that BEFORE the download - it is a field on a
/// value already in hand, not another request - is what lets a caller say
/// "this feed is stalled" while the transfer is still running rather than
/// after a stale volume has landed looking fresh.
///
/// The real cure for a dead prefix is a second source for the same radar (the
/// Level II archive bucket carries KUEX for the same period, and NWS TDS is a
/// third). This function deliberately does not reach for one: choosing between
/// sources is a policy decision that belongs above it.
pub fn latest_realtime_level2_volume(site: &str) -> Result<RealtimeLevel2Volume> {
    let site = site.to_ascii_uppercase();
    let site_prefix = format!("{site}/");
    let active_ids = list_active_realtime_volume_ids(&site)?;

    let Some(volume_id) = latest_realtime_volume_id_from_active_ids(&active_ids) else {
        return Err(DataSourceError::NoObjects {
            bucket: LEVEL2_CHUNKS_BUCKET.to_owned(),
            prefix: site_prefix,
        });
    };

    let candidates = realtime_volume_candidate_ids_from_active_ids(&active_ids);
    let mut best_volume = None;
    let mut first_error = None;
    for candidate_id in candidates {
        match realtime_level2_volume_for_id(&site, candidate_id) {
            Ok(volume) => {
                if best_volume
                    .as_ref()
                    .is_none_or(|best: &RealtimeLevel2Volume| {
                        volume.volume_time > best.volume_time
                            || (volume.volume_time == best.volume_time
                                && volume.chunks.len() > best.chunks.len())
                    })
                {
                    best_volume = Some(volume);
                }
            }
            Err(err) => {
                first_error.get_or_insert(err);
            }
        }
    }

    if let Some(volume) = best_volume {
        return Ok(volume);
    }

    realtime_level2_volume_for_id(&site, volume_id).map_err(|_| {
        first_error.unwrap_or(DataSourceError::NoObjects {
            bucket: LEVEL2_CHUNKS_BUCKET.to_owned(),
            prefix: site_prefix,
        })
    })
}

/// Fetch the newest complete volume that ran *before* the one identified by
/// `current_volume_id` / `current_volume_time`.
///
/// A live session that has only just started holds a single tilt, which is not
/// a volume: the 3D box interpolates a vertical profile per (azimuth, range)
/// and a one-sample profile fills only the beam it came from, and the 2D sweep
/// animation has no previous picture to paint the unswept wedge with. Pulling
/// the volume before the live one closes that gap immediately instead of after
/// a whole VCP.
///
/// The predecessor is found through the ACTIVE id set, never by arithmetic.
/// Realtime volume ids are a wrapping counter that steps 999 -> 1 with no zero
/// (measured: KTLX/999 starts 2026-08-16T08:13:07Z, KTLX/1 at 08:20:09Z,
/// KTLX/2 at 08:27:11Z), so `current - 1` names a directory that does not exist
/// at the wrap; and the bucket keeps expired ids for days, so the id it names
/// may hold a volume from two days ago. Three traps are handled here:
///
/// * the wrap - the walk is nearest-preceding-ACTIVE-id, so a missing or
///   wrapped `current - 1` resolves to whatever really precedes it;
/// * the recycled id - every candidate must start earlier than, and within
///   `REALTIME_PREVIOUS_VOLUME_MAX_GAP_MINUTES` of, the current volume, which
///   is the only thing that can distinguish "one volume back" from "996
///   volumes back" when all you have is a wrapping counter;
/// * the aged-out head - "complete" means the contiguous run `1..=n`, not just
///   an `E` chunk at the end. See [`first_missing_chunk_id`].
pub fn previous_complete_realtime_level2_volume(
    site: &str,
    current_volume_id: u16,
    current_volume_time: DateTime<Utc>,
) -> Result<RealtimeLevel2Volume> {
    let site = site.to_ascii_uppercase();
    let active_ids = list_active_realtime_volume_ids(&site)?;
    let candidate_ids = preceding_realtime_volume_ids_from_active_ids(
        &active_ids,
        current_volume_id,
        REALTIME_PREVIOUS_VOLUME_LOOKBACK,
    );

    let oldest_accepted =
        current_volume_time - Duration::minutes(REALTIME_PREVIOUS_VOLUME_MAX_GAP_MINUTES);
    let mut first_error = None;
    for candidate_id in candidate_ids {
        let groups = match realtime_level2_volume_groups_for_id(&site, candidate_id) {
            Ok(groups) => groups,
            Err(DataSourceError::NoObjects { .. }) => {
                // A directory that has expired between the listing and this
                // call is ordinary; keep walking backwards.
                continue;
            }
            Err(error) => {
                // A dropped link or an unparseable listing is not ordinary, and
                // reporting it as "no objects" would read like the site is off
                // the air. Keep walking - a later candidate may still answer -
                // but report this if none does.
                first_error.get_or_insert(error);
                continue;
            }
        };
        if let Some(volume) =
            select_previous_complete_volume(groups, current_volume_time, oldest_accepted)
        {
            return Ok(volume);
        }
    }

    Err(first_error.unwrap_or(DataSourceError::NoObjects {
        bucket: LEVEL2_CHUNKS_BUCKET.to_owned(),
        prefix: format!("{site}/ before volume {current_volume_id}"),
    }))
}

/// The active volume id immediately preceding `current_volume_id`.
///
/// Membership in `ids` is the only source of truth: the answer is the id with
/// the smallest positive distance walking backwards around the wrapping
/// counter, so a missing `current - 1` resolves to whatever really precedes
/// it, and `current == 1` resolves to 999 rather than underflowing.
pub fn previous_realtime_volume_id_from_active_ids(
    ids: &[u16],
    current_volume_id: u16,
) -> Option<u16> {
    preceding_realtime_volume_ids_from_active_ids(ids, current_volume_id, 1)
        .into_iter()
        .next()
}

/// Fetch one realtime volume by id. When the id directory holds more than one
/// volume (see [`realtime_volume_groups`]) the newest is returned.
pub fn realtime_level2_volume_for_id(site: &str, volume_id: u16) -> Result<RealtimeLevel2Volume> {
    // Keys in the chunks bucket are upper case; a lower-case site would list
    // an empty prefix rather than fail, which is the worse of the two.
    let site = site.to_ascii_uppercase();
    realtime_level2_volume_groups_for_id(&site, volume_id)?
        .pop()
        .ok_or_else(|| DataSourceError::NoObjects {
            bucket: LEVEL2_CHUNKS_BUCKET.to_owned(),
            prefix: format!("{site}/{volume_id}/"),
        })
}

fn realtime_level2_volume_groups_for_id(
    site: &str,
    volume_id: u16,
) -> Result<Vec<RealtimeLevel2Volume>> {
    let volume_prefix = format!("{site}/{volume_id}/");
    let chunks = list_s3_limited(
        LEVEL2_CHUNKS_BUCKET,
        &volume_prefix,
        None,
        None,
        Some(REALTIME_CHUNK_LIST_MAX_KEYS),
    )?
    .contents
    .into_iter()
    .filter(|object| object.size > 0)
    .filter_map(parse_realtime_chunk_object)
    .collect::<Vec<_>>();

    let groups = realtime_volume_groups(site, volume_id, chunks);
    if groups.is_empty() {
        return Err(DataSourceError::NoObjects {
            bucket: LEVEL2_CHUNKS_BUCKET.to_owned(),
            prefix: volume_prefix,
        });
    }
    Ok(groups)
}

fn list_active_realtime_volume_ids(site: &str) -> Result<Vec<u16>> {
    let site_prefix = format!("{site}/");
    let mut ids = list_s3(LEVEL2_CHUNKS_BUCKET, &site_prefix, Some("/"), None)?
        .common_prefixes
        .into_iter()
        .filter_map(|prefix| realtime_volume_id_from_prefix(site, &prefix.prefix))
        .collect::<Vec<_>>();
    ids.sort_unstable();
    ids.dedup();
    Ok(ids)
}

// --- the Level II archive, as the chunk feed's second source ----------------
//
// Every radar this app can draw is uploaded TWICE by different machinery: as a
// stream of growing chunks into `unidata-nexrad-level2-chunks`, and as one
// finished object per volume into `unidata-nexrad-level2` under
// `YYYY/MM/DD/SITE/`. The two fail independently, which is the only reason
// this section exists. Measured 2026-08-19T18:50Z: KUEX's chunk prefix had
// written nothing since `KUEX/931/20260816-110802-003-I` at
// 2026-08-16T11:08:09Z - three days - while `2026/08/19/KUEX/` held 262
// finished volumes, the newest `KUEX20260819_184216_V06` uploaded at 18:47:13Z.
// The radar never stopped scanning. Only one of its two pipes stopped moving,
// and an app that reads one pipe shows a weekend-old storm under today's
// warning polygons.

/// The archive bucket's metadata companion objects.
///
/// Real keys, counted under `2026/08/19/KUEX/` at 18:49Z: 262 volume objects
/// of 6.1-16.7 MB, and 18 `..._V06_MDM` objects of ~720 KB, about one an hour.
/// An `_MDM` sorts immediately AFTER the volume it belongs to, so "the last key
/// in the prefix" returns a metadata blob whenever one has just been written -
/// and at 720 KB it is far too big for a size floor to catch. The suffix is the
/// discriminator, and it is the same one [`recent_level2_objects`] has always
/// used.
const ARCHIVE_METADATA_KEY_SUFFIX: &str = "_MDM";

/// How far back a cold archive listing starts, minutes.
///
/// Keys under a day prefix are chronological, so S3's `start-after` turns
/// "what is newest" from a whole-day listing into a window. Measured against
/// the KUEX prefix above: the whole day was 280 keys / 90,741 bytes / 486 ms,
/// a two-hour window 23 keys / 7,770 bytes / 343 ms - 11.7x less to move, on
/// a fallback that may run for hours.
///
/// Two hours is far longer than any legitimate gap between volumes. The
/// slowest WSR-88D VCP is about 10 minutes, and that same day of KUEX measured
/// volume intervals of min 196 s, max 418 s, mean 258 s. A site that is on the
/// air at all lands inside the window; a site that is not falls through to the
/// whole-day listing, which is the rarer and slower path on purpose.
const ARCHIVE_RECENT_WINDOW_MINUTES: i64 = 120;

/// Pages one archive listing will follow before giving up.
///
/// A day of one site is ~280 keys, comfortably inside a single 1,000-key page.
/// But a truncated listing returns the OLDEST 1,000 keys - exactly the wrong
/// end for "what is newest" - so truncation is followed rather than ignored.
/// Eight pages is 8,000 keys, roughly a month of one site, and a hard stop
/// against a prefix that is not what this code thinks it is.
const ARCHIVE_LISTING_MAX_PAGES: usize = 8;

/// One complete Level II volume sitting in the archive bucket.
///
/// The archive is the chunk feed's slower, steadier twin: the same radar, one
/// finished object per volume instead of a growing chunk set. That makes it
/// both easier (nothing to assemble, nothing to check for gaps) and later: on
/// 2026-08-19 the KUEX object appeared a mean 252 s after the volume STARTED
/// (min 190, max 611), so an archive picture is inherently ~4-9 minutes behind
/// wall clock. Worse than a healthy chunk feed. Infinitely better than a chunk
/// feed that stopped on Saturday.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ArchiveLevel2Volume {
    pub site: String,
    pub object: S3Object,
    /// Volume START time, parsed from the key, so it means exactly what
    /// [`RealtimeLevel2Volume::volume_time`] means and the two can be compared
    /// to decide which source is ahead.
    pub volume_time: DateTime<Utc>,
}

impl ArchiveLevel2Volume {
    pub fn key(&self) -> &str {
        &self.object.key
    }

    pub fn total_size(&self) -> u64 {
        self.object.size
    }

    /// When the bucket received this volume, as opposed to when the radar
    /// started scanning it. The gap between the two IS the archive's latency
    /// and is worth reporting rather than hiding.
    pub fn uploaded_at(&self) -> Option<DateTime<Utc>> {
        self.object.last_modified
    }

    /// How far behind `now` this volume's start time is. See [`volume_age_at`].
    pub fn age_at(&self, now: DateTime<Utc>) -> Duration {
        volume_age_at(self.volume_time, now)
    }

    /// The AGE verdict on this volume - [`FeedFreshness::Current`] or
    /// [`FeedFreshness::Stalled`], never [`FeedFreshness::ArchiveFallback`],
    /// which is a statement about which source a session chose and belongs to
    /// the session.
    pub fn freshness_at(&self, now: DateTime<Utc>) -> FeedFreshness {
        classify_feed_age(self.age_at(now))
    }
}

/// The newest complete volume the archive bucket holds for `site`.
///
/// Fails with [`DataSourceError::NoObjects`] when today and yesterday are both
/// empty for this radar, which for a live session means the archive has
/// nothing to offer either.
pub fn latest_archive_level2_volume(site: &str) -> Result<ArchiveLevel2Volume> {
    let site = site.to_ascii_uppercase();
    archive_level2_volume_newer_than(&site, Utc::now(), None)?.ok_or_else(|| {
        DataSourceError::NoObjects {
            bucket: LEVEL2_ARCHIVE_BUCKET.to_owned(),
            prefix: site,
        }
    })
}

/// The newest archive volume for `site`, or `None` when nothing newer than
/// `known_newest` exists.
///
/// Two questions in one function because they are the same listing asked from
/// two starting points, and a poll loop only ever asks the second:
///
/// * `known_newest: None` is a cold start - "what is the newest thing here" -
///   and pays for the widening walk below.
/// * `known_newest: Some(t)` is the steady state - "has anything appeared
///   since t" - and `start-after` makes that the cheapest question S3 answers.
///   A poll that finds nothing new moves an empty listing envelope, not a day
///   of keys, which is what makes a 30 s cadence affordable for hours.
///
/// THE UTC DAY BOUNDARY. Keys are day-parted by the volume's START time, so at
/// 00:04Z today's prefix legitimately holds nothing and the newest volume in
/// existence is under yesterday's. The walk is today (windowed), then today
/// (whole), then yesterday, and the order matters: anything under today's
/// prefix necessarily started after midnight and is therefore newer than
/// anything under yesterday's, so a hit on today is final and yesterday is
/// only ever consulted when today is genuinely empty.
///
/// With `known_newest` set the widening is skipped entirely: an empty
/// `start-after` listing then means "nothing newer", full stop, and re-listing
/// the whole day to rediscover a volume already in hand would cost 90 KB per
/// poll to learn nothing. The one extra listing it does make is yesterday's,
/// and only while the volume in hand is still yesterday's - see
/// [`archive_listing_plan`], which is where every calendar case lives.
pub fn archive_level2_volume_newer_than(
    site: &str,
    now: DateTime<Utc>,
    known_newest: Option<DateTime<Utc>>,
) -> Result<Option<ArchiveLevel2Volume>> {
    let site = site.to_ascii_uppercase();
    for step in archive_listing_plan(&site, now, known_newest) {
        let objects = list_archive_day(&site, step.date, step.start_after.as_deref())?;
        if let Some(volume) = newest_archive_volume(&site, objects, known_newest) {
            return Ok(Some(volume));
        }
    }
    Ok(None)
}

/// One listing in an archive poll's plan.
#[derive(Clone, Debug, Eq, PartialEq)]
struct ArchiveListingStep {
    date: NaiveDate,
    /// S3's `start-after`, or `None` to list the whole day prefix.
    start_after: Option<String>,
}

/// The listings one archive poll will make, in order, stopping at the first
/// that answers.
///
/// Separated from the requests themselves because every hard case here is a
/// CALENDAR case - midnight, the last volume of a day arriving after midnight,
/// a site quiet for hours - and a calendar case that can only be exercised by
/// waiting for midnight is a case that never gets exercised. This function
/// takes its clock as an argument and returns data, so those cases are unit
/// tests.
///
/// Stopping at the first answer is sound because the plan is ordered
/// newest-prefix-first and a day prefix holds only volumes that STARTED that
/// day: anything under today's prefix is necessarily newer than anything under
/// yesterday's.
fn archive_listing_plan(
    site: &str,
    now: DateTime<Utc>,
    known_newest: Option<DateTime<Utc>>,
) -> Vec<ArchiveListingStep> {
    let today = now.date_naive();
    let Some(known) = known_newest else {
        // Cold. Widen: a two-hour window on today (the cheap question), then
        // the whole of today, then yesterday. The second step is skipped when
        // the window could not be expressed, because the first listing then
        // already WAS the whole day and repeating it would be a wasted
        // request.
        let window_start = now - Duration::minutes(ARCHIVE_RECENT_WINDOW_MINUTES);
        let windowed = archive_start_after_key(site, today, window_start);
        let widen = windowed.is_some();
        let mut plan = vec![ArchiveListingStep {
            date: today,
            start_after: windowed,
        }];
        if widen {
            plan.push(ArchiveListingStep {
                date: today,
                start_after: None,
            });
        }
        plan.push(ArchiveListingStep {
            date: today - Duration::days(1),
            start_after: None,
        });
        return plan;
    };

    // Warm - "anything newer than what I hold?" - which is one listing, and
    // usually an empty one, which is what makes a 30 s cadence affordable for
    // hours.
    let mut plan = vec![ArchiveListingStep {
        date: today,
        start_after: archive_start_after_key(site, today, known),
    }];

    // THE DAY BOUNDARY, from the other side. A volume already in hand from
    // yesterday does not stop yesterday's prefix from receiving one more: a
    // volume that STARTS at 23:58Z is uploaded four to nine minutes later,
    // which is tomorrow. Listing only today would leave that volume
    // undiscoverable for ever, because the day it belongs to is one this walk
    // would never look at again. It costs a second listing only until the
    // first volume of the new day appears, which is minutes.
    let known_day = known.date_naive();
    if known_day < today {
        plan.push(ArchiveListingStep {
            date: known_day,
            start_after: archive_start_after_key(site, known_day, known),
        });
    }
    plan
}

/// `YYYY/MM/DD/SITE/` - the archive bucket's one and only layout.
fn archive_day_prefix(site: &str, date: NaiveDate) -> String {
    format!(
        "{:04}/{:02}/{:02}/{}/",
        date.year(),
        date.month(),
        date.day(),
        site
    )
}

/// The `start-after` key that limits a day listing to volumes that started
/// after `from`, or `None` when no such key exists for this prefix.
///
/// S3 compares `start-after` as a plain string against the keys in the bucket,
/// and archive keys embed their own date - `2026/08/19/KUEX/KUEX20260819_...` -
/// so a key built from a DIFFERENT day would sort outside the prefix entirely
/// and silently return either everything or nothing. `None` means "no window
/// is expressible here, list the prefix", which is the honest answer whenever
/// `from` falls on another day: at 00:30Z a two-hour window starts at 22:30
/// yesterday, and today's whole prefix is half an hour long anyway.
fn archive_start_after_key(site: &str, date: NaiveDate, from: DateTime<Utc>) -> Option<String> {
    if from.date_naive() != date {
        return None;
    }
    Some(format!(
        "{}{}{}",
        archive_day_prefix(site, date),
        site,
        from.format("%Y%m%d_%H%M%S")
    ))
}

/// Every object under one day prefix, following truncation.
fn list_archive_day(
    site: &str,
    date: NaiveDate,
    start_after: Option<&str>,
) -> Result<Vec<S3Object>> {
    let prefix = archive_day_prefix(site, date);
    let mut objects = Vec::new();
    let mut continuation_token: Option<String> = None;
    for _ in 0..ARCHIVE_LISTING_MAX_PAGES {
        let listing = list_s3_request(&S3ListRequest {
            bucket: LEVEL2_ARCHIVE_BUCKET,
            prefix: &prefix,
            continuation_token: continuation_token.as_deref(),
            start_after,
            ..S3ListRequest::default()
        })?;
        objects.extend(listing.contents);
        match listing.next_continuation_token {
            Some(token) => continuation_token = Some(token),
            None => break,
        }
    }
    Ok(objects)
}

/// The newest real volume in a listing, optionally requiring it to beat a
/// volume already in hand.
///
/// Ordered by the time parsed out of the KEY rather than by the key itself or
/// by `LastModified`. The key is the volume's start time and is what the rest
/// of the app compares against; `LastModified` is when the upload finished,
/// which for a late-arriving volume can order two volumes backwards.
fn newest_archive_volume(
    site: &str,
    objects: Vec<S3Object>,
    newer_than: Option<DateTime<Utc>>,
) -> Option<ArchiveLevel2Volume> {
    objects
        .into_iter()
        .filter(is_archive_volume_object)
        .filter_map(|object| {
            let volume_time = parse_level2_object_time_utc(&object.key)?;
            Some(ArchiveLevel2Volume {
                site: site.to_owned(),
                object,
                volume_time,
            })
        })
        .filter(|volume| newer_than.is_none_or(|known| volume.volume_time > known))
        .max_by(|left, right| {
            left.volume_time
                .cmp(&right.volume_time)
                .then_with(|| left.object.key.cmp(&right.object.key))
        })
}

/// Whether one listed object is a volume rather than a metadata companion or a
/// zero-byte placeholder.
fn is_archive_volume_object(object: &S3Object) -> bool {
    object.size > 0 && !object.key.ends_with(ARCHIVE_METADATA_KEY_SUFFIX)
}

/// The volume start time carried by an archive key.
///
/// `2026/08/19/KUEX/KUEX20260819_184216_V06` -> 2026-08-19T18:42:16Z. Handles
/// the compressed historical form (`..._V03.gz`) too, because the same key
/// shape has been in use since the 1990s and an archive browser reaches back
/// that far even though this fallback never looks past yesterday.
pub fn parse_level2_object_time_utc(key: &str) -> Option<DateTime<Utc>> {
    let name = key.rsplit('/').next()?;
    let underscore = name.find('_')?;
    if underscore < 8 || name.len() < underscore + 7 {
        return None;
    }
    let date = &name[underscore - 8..underscore];
    let time = &name[underscore + 1..underscore + 7];
    if !date.bytes().all(|byte| byte.is_ascii_digit())
        || !time.bytes().all(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    let naive = NaiveDate::parse_from_str(date, "%Y%m%d")
        .ok()?
        .and_hms_opt(
            time[0..2].parse().ok()?,
            time[2..4].parse().ok()?,
            time[4..6].parse().ok()?,
        )?;
    Some(DateTime::<Utc>::from_naive_utc_and_offset(naive, Utc))
}

/// Fetch one archive volume into the same cache the realtime path writes to.
pub fn download_archive_volume(
    volume: &ArchiveLevel2Volume,
    cache_dir: &Path,
) -> Result<DownloadedObject> {
    download_archive_volume_cancellable(volume, cache_dir, &|| false)
}

/// [`download_archive_volume`], abandoned mid-transfer when `cancelled` starts
/// returning true.
///
/// Same contract as [`download_realtime_volume_cancellable`] and for the same
/// reason: a whole volume is 6-17 MB, and bytes pulled after the analyst has
/// switched radars are bytes spent on a result that can no longer be
/// installed. The realtime path can only check between chunk batches because
/// its unit of work is a chunk; an archive volume is ONE object, so the check
/// happens inside the copy instead - see [`copy_cancellable`] - which makes
/// this the finer-grained of the two.
///
/// The cache filename is the key's own basename (`KUEX20260819_184216_V06`),
/// distinct from the realtime path's `KUEX20260819_184216_RT931_V06`, so the
/// two sources can hold the same volume time in the same directory without
/// either overwriting the other's file. Both are ordinary files to
/// [`prune_live_cache`], which bounds the directory either way.
pub fn download_archive_volume_cancellable(
    volume: &ArchiveLevel2Volume,
    cache_dir: &Path,
    cancelled: &dyn Fn() -> bool,
) -> Result<DownloadedObject> {
    fs::create_dir_all(cache_dir)?;
    let filename = archive_volume_cache_filename(volume);
    let path = cache_dir.join(&filename);
    let url = format!(
        "https://{}.s3.amazonaws.com/{}",
        LEVEL2_ARCHIVE_BUCKET, volume.object.key
    );

    // Size, not existence: a half-written file from a killed session must be
    // re-fetched rather than decoded. The realtime path makes the same check
    // against its assembled total.
    if path
        .metadata()
        .map(|metadata| metadata.len() == volume.object.size)
        .unwrap_or(false)
    {
        return Ok(DownloadedObject {
            object: volume.object.clone(),
            path,
            url,
            cache_hit: true,
        });
    }

    if cancelled() {
        return Err(DataSourceError::ObjectDownloadCancelled {
            key: volume.object.key.clone(),
        });
    }
    download_s3_object_to_path_cancellable(
        LEVEL2_ARCHIVE_BUCKET,
        &volume.object,
        &path,
        cancelled,
    )?;

    Ok(DownloadedObject {
        object: volume.object.clone(),
        path,
        url,
        cache_hit: false,
    })
}

fn archive_volume_cache_filename(volume: &ArchiveLevel2Volume) -> String {
    volume
        .object
        .key
        .rsplit('/')
        .next()
        .unwrap_or(&volume.object.key)
        .to_owned()
}

pub fn download_realtime_volume(
    volume: &RealtimeLevel2Volume,
    cache_dir: &Path,
) -> Result<DownloadedObject> {
    download_realtime_volume_cancellable(volume, cache_dir, &|| false)
}

/// [`download_realtime_volume`], abandoned between chunk batches when
/// `cancelled` starts returning true.
///
/// A whole volume is 6-13 MB. A background fetch that keeps pulling that after
/// the analyst has switched sites is bandwidth spent on a result that can no
/// longer be installed, so the caller gets a way to stop it. Chunks already
/// written stay in the chunk cache and are reused if the same volume is asked
/// for again.
pub fn download_realtime_volume_cancellable(
    volume: &RealtimeLevel2Volume,
    cache_dir: &Path,
    cancelled: &dyn Fn() -> bool,
) -> Result<DownloadedObject> {
    // Refused before anything is written, because concatenating a gapped chunk
    // set produces a file that decodes into plausible-looking radials under a
    // garbage header rather than failing. See [`first_missing_chunk_id`]; the
    // live poll recovers on its next pass, which is 1.2 s away.
    if let Some(missing_chunk_id) = first_missing_chunk_id(&volume.chunks) {
        return Err(DataSourceError::ChunkSetNotContiguous {
            site: volume.site.clone(),
            volume_id: volume.volume_id,
            volume_time: volume.volume_time,
            missing_chunk_id,
            last_chunk_id: volume.chunks.last().map_or(0, |chunk| chunk.chunk_id),
        });
    }

    fs::create_dir_all(cache_dir)?;
    let filename = realtime_volume_cache_filename(volume);
    let path = cache_dir.join(&filename);
    let url = format!(
        "https://{}.s3.amazonaws.com/{}/{}/",
        LEVEL2_CHUNKS_BUCKET, volume.site, volume.volume_id
    );

    if path
        .metadata()
        .map(|metadata| metadata.len() == volume.total_size)
        .unwrap_or(false)
    {
        discard_chunks_of_complete_volume(volume, cache_dir);
        return Ok(DownloadedObject {
            object: S3Object {
                key: filename,
                size: volume.total_size,
                last_modified: volume
                    .chunks
                    .last()
                    .and_then(|chunk| chunk.object.last_modified),
            },
            path,
            url,
            cache_hit: true,
        });
    }

    let chunk_cache_dir = realtime_chunk_cache_dir(cache_dir, volume);
    fs::create_dir_all(&chunk_cache_dir)?;

    let mut chunk_paths = Vec::with_capacity(volume.chunks.len());
    let mut missing = Vec::new();
    for chunk in &volume.chunks {
        let chunk_filename = chunk
            .object
            .key
            .rsplit('/')
            .next()
            .unwrap_or(&chunk.object.key);
        let chunk_path = chunk_cache_dir.join(chunk_filename);
        let cache_hit = chunk_path
            .metadata()
            .map(|metadata| metadata.len() == chunk.object.size)
            .unwrap_or(false);
        if !cache_hit {
            missing.push((chunk.object.clone(), chunk_path.clone()));
        }
        chunk_paths.push(chunk_path);
    }

    for batch in missing.chunks(tuning::chunk_download_batch()) {
        if cancelled() {
            return Err(DataSourceError::DownloadCancelled {
                site: volume.site.clone(),
                volume_id: volume.volume_id,
            });
        }
        thread::scope(|scope| -> Result<()> {
            let mut workers = Vec::with_capacity(batch.len());
            for (object, path) in batch {
                let object = object.clone();
                let path = path.clone();
                workers.push(scope.spawn(move || {
                    download_s3_object_to_path(LEVEL2_CHUNKS_BUCKET, &object, &path)
                }));
            }
            for worker in workers {
                worker
                    .join()
                    .map_err(|_| DataSourceError::DownloadWorkerPanic)??;
            }
            Ok(())
        })?;
    }

    if let Ok(existing_len) = path.metadata().map(|metadata| metadata.len())
        && let Some(prefix_chunks) = chunk_prefix_count_for_size(volume, existing_len)
        && prefix_chunks > 0
        && prefix_chunks < chunk_paths.len()
    {
        append_realtime_chunks(
            &path,
            &chunk_paths[prefix_chunks..],
            existing_len,
            volume.total_size,
            &url,
        )?;
        discard_chunks_of_complete_volume(volume, cache_dir);
        return Ok(DownloadedObject {
            object: S3Object {
                key: filename,
                size: volume.total_size,
                last_modified: volume
                    .chunks
                    .last()
                    .and_then(|chunk| chunk.object.last_modified),
            },
            path,
            url,
            cache_hit: false,
        });
    }

    let temp_path = path.with_extension("download");
    let mut temp_file = fs::File::create(&temp_path)?;
    for chunk_path in &chunk_paths {
        let mut chunk_file = fs::File::open(chunk_path)?;
        io::copy(&mut chunk_file, &mut temp_file)?;
    }
    drop(temp_file);

    let copied = temp_path.metadata()?.len();
    if copied != volume.total_size {
        let _ = fs::remove_file(&temp_path);
        return Err(DataSourceError::DownloadSizeMismatch {
            url,
            expected: volume.total_size,
            actual: copied,
        });
    }
    if path.exists() {
        fs::remove_file(&path)?;
    }
    fs::rename(&temp_path, &path)?;
    discard_chunks_of_complete_volume(volume, cache_dir);

    Ok(DownloadedObject {
        object: S3Object {
            key: filename,
            size: volume.total_size,
            last_modified: volume
                .chunks
                .last()
                .and_then(|chunk| chunk.object.last_modified),
        },
        path,
        url,
        cache_hit: false,
    })
}

/// Where one realtime volume's individual chunk files are cached while it is
/// still assembling.
fn realtime_chunk_cache_dir(cache_dir: &Path, volume: &RealtimeLevel2Volume) -> PathBuf {
    cache_dir.join(".chunks").join(format!(
        "{}_{}_{:03}",
        volume.site,
        volume.volume_time.format("%Y%m%d_%H%M%S"),
        volume.volume_id
    ))
}

/// Drop the per-chunk copies of a volume whose assembled file is on disk.
///
/// Only for a COMPLETE volume: its file passes the size check at the top of
/// [`download_realtime_volume_cancellable`] on every later request, so the
/// chunks buy nothing - and they were the largest growth term measured in the
/// unbounded cache (540 MB of retained `.chunks/` out of 1,072 MB after ~2
/// days of single-site use). A partial volume keeps its chunks: the file is
/// re-extended from them as the volume grows.
fn discard_chunks_of_complete_volume(volume: &RealtimeLevel2Volume, cache_dir: &Path) {
    if volume.complete {
        let _ = fs::remove_dir_all(realtime_chunk_cache_dir(cache_dir, volume));
    }
}

/// Live-cache budget for a desktop install, bytes.
///
/// Deliberate, not arbitrary: the measured growth is ~0.5 GB/day at
/// single-site use, and BowEcho's identical unbounded cache on the same
/// machine reached 17,506 MB - the proven endpoint of "no budget". 2 GiB keeps
/// several days of multi-site history while staying invisible on a desktop
/// disk; a mobile profile (~256 MB) arrives with the settings work, which is
/// why the budget is a parameter of [`prune_live_cache`] rather than baked in.
pub const DEFAULT_LIVE_CACHE_BUDGET_BYTES: u64 = 2 * 1024 * 1024 * 1024;

/// A prune never touches anything newer than this, milliseconds.
///
/// The newest entries are the volume still assembling on the live poll and a
/// possible backfill mid-download on its own thread; deleting either from
/// under its writer forces a re-download at best. Fifteen minutes clears the
/// slowest WSR-88D volume interval (VCP 31/32, 10 minutes) with margin.
const LIVE_CACHE_PRUNE_MIN_AGE_MILLIS: u64 = 15 * 60 * 1_000;

/// A prune stops at this fraction of the budget rather than exactly at it, so
/// the next volume written does not immediately trigger another prune. Same
/// policy as the basemap tile cache's sweep.
const LIVE_CACHE_PRUNE_TARGET_FRACTION: f64 = 0.9;

/// What one prune did, so a caller (or a test) can assert the bound rather
/// than trusting a comment.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct LiveCachePruneReport {
    pub entries_before: u64,
    pub bytes_before: u64,
    pub entries_removed: u64,
    pub bytes_after: u64,
}

/// One evictable unit of the live cache: an assembled volume file, or one
/// volume's whole `.chunks/` directory. A chunk directory is evicted as a unit
/// because deleting individual chunk files out of it would leave a gapped set
/// that [`first_missing_chunk_id`] then refuses wholesale anyway.
struct LiveCacheEntry {
    path: PathBuf,
    is_directory: bool,
    bytes: u64,
    /// Newest modification time inside the unit, milliseconds since the
    /// epoch, so an actively-growing chunk directory reads as young.
    newest_modified_unix_millis: u64,
}

/// Bound the live Level II cache by deleting the oldest volumes first.
///
/// This cache grew without bound - 1,072 MB in ~2 days measured at single-site
/// dev use, 17.5 GB proven endpoint on the same machine via BowEcho's
/// identical cache pattern - while the in-repo tile cache has been
/// byte-budgeted all along. Same pattern here: walk, total, delete
/// oldest-first (by newest contained mtime) down to
/// [`LIVE_CACHE_PRUNE_TARGET_FRACTION`] of `max_bytes`, and never touch
/// anything younger than [`LIVE_CACHE_PRUNE_MIN_AGE_MILLIS`]. Age doubles as
/// the correctness guard: the units a writer may be mid-way through are by
/// construction the youngest in the directory.
pub fn prune_live_cache(cache_dir: &Path, max_bytes: u64) -> LiveCachePruneReport {
    let now_unix_millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_millis() as u64);
    prune_live_cache_at(cache_dir, max_bytes, now_unix_millis)
}

/// [`prune_live_cache`] against an explicit clock, so the age guard is
/// testable without waiting fifteen minutes.
fn prune_live_cache_at(
    cache_dir: &Path,
    max_bytes: u64,
    now_unix_millis: u64,
) -> LiveCachePruneReport {
    let mut entries = live_cache_entries(cache_dir);
    let bytes_before: u64 = entries.iter().map(|entry| entry.bytes).sum();
    let entries_before = entries.len() as u64;
    if bytes_before <= max_bytes {
        return LiveCachePruneReport {
            entries_before,
            bytes_before,
            entries_removed: 0,
            bytes_after: bytes_before,
        };
    }

    entries.sort_by_key(|entry| entry.newest_modified_unix_millis);
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let target = (max_bytes as f64 * LIVE_CACHE_PRUNE_TARGET_FRACTION) as u64;
    let mut total = bytes_before;
    let mut removed = 0_u64;
    for entry in &entries {
        if total <= target {
            break;
        }
        if now_unix_millis.saturating_sub(entry.newest_modified_unix_millis)
            < LIVE_CACHE_PRUNE_MIN_AGE_MILLIS
        {
            // Sorted oldest-first, so everything from here on is younger
            // still: over budget or not, the young end is never deleted.
            break;
        }
        let deleted = if entry.is_directory {
            fs::remove_dir_all(&entry.path).is_ok()
        } else {
            fs::remove_file(&entry.path).is_ok()
        };
        if deleted {
            total = total.saturating_sub(entry.bytes);
            removed += 1;
        }
    }
    LiveCachePruneReport {
        entries_before,
        bytes_before,
        entries_removed: removed,
        bytes_after: total,
    }
}

fn live_cache_entries(cache_dir: &Path) -> Vec<LiveCacheEntry> {
    let mut entries = Vec::new();
    let Ok(listing) = fs::read_dir(cache_dir) else {
        return entries;
    };
    for entry in listing.flatten() {
        let path = entry.path();
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        if metadata.is_file() {
            entries.push(LiveCacheEntry {
                path,
                is_directory: false,
                bytes: metadata.len(),
                newest_modified_unix_millis: modified_unix_millis(&metadata),
            });
        } else if metadata.is_dir() && path.file_name().is_some_and(|name| name == ".chunks") {
            let Ok(chunk_dirs) = fs::read_dir(&path) else {
                continue;
            };
            for chunk_dir in chunk_dirs.flatten() {
                let dir_path = chunk_dir.path();
                let (bytes, newest_modified_unix_millis) = directory_stats(&dir_path);
                entries.push(LiveCacheEntry {
                    path: dir_path,
                    is_directory: true,
                    bytes,
                    newest_modified_unix_millis,
                });
            }
        }
        // Any other directory is not this cache's to delete.
    }
    entries
}

/// Total bytes under `directory` and the newest mtime in it, recursively. The
/// directory's own mtime participates too, so an empty leftover still ages.
fn directory_stats(directory: &Path) -> (u64, u64) {
    let mut bytes = 0_u64;
    let mut newest = directory
        .metadata()
        .map(|metadata| modified_unix_millis(&metadata))
        .unwrap_or(0);
    if let Ok(listing) = fs::read_dir(directory) {
        for entry in listing.flatten() {
            let Ok(metadata) = entry.metadata() else {
                continue;
            };
            if metadata.is_dir() {
                let (child_bytes, child_newest) = directory_stats(&entry.path());
                bytes += child_bytes;
                newest = newest.max(child_newest);
            } else {
                bytes += metadata.len();
                newest = newest.max(modified_unix_millis(&metadata));
            }
        }
    }
    (bytes, newest)
}

fn modified_unix_millis(metadata: &fs::Metadata) -> u64 {
    metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |duration| duration.as_millis() as u64)
}

pub fn download_object(
    bucket: &str,
    object: S3Object,
    cache_dir: &Path,
) -> Result<DownloadedObject> {
    fs::create_dir_all(cache_dir)?;
    let filename = object.key.rsplit('/').next().unwrap_or(&object.key);
    let path = cache_dir.join(filename);
    let url = format!("https://{bucket}.s3.amazonaws.com/{}", object.key);
    if path
        .metadata()
        .map(|metadata| metadata.len() == object.size)
        .unwrap_or(false)
    {
        return Ok(DownloadedObject {
            object,
            path,
            url,
            cache_hit: true,
        });
    }

    download_s3_object_to_path(bucket, &object, &path)?;
    Ok(DownloadedObject {
        object,
        path,
        url,
        cache_hit: false,
    })
}

pub fn newest_cached_level2_path(cache_dir: &Path) -> Result<Option<PathBuf>> {
    if !cache_dir.exists() {
        return Ok(None);
    }

    let mut newest: Option<(String, PathBuf)> = None;
    for entry in fs::read_dir(cache_dir)? {
        let entry = entry?;
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if name.ends_with(".download") || name.ends_with("_MDM") {
            continue;
        }
        if path.metadata().map(|metadata| metadata.len() == 0)? {
            continue;
        }
        if newest
            .as_ref()
            .is_none_or(|(newest_name, _)| name > newest_name.as_str())
        {
            newest = Some((name.to_owned(), path));
        }
    }

    Ok(newest.map(|(_, path)| path))
}

fn list_s3(
    bucket: &str,
    prefix: &str,
    delimiter: Option<&str>,
    continuation_token: Option<&str>,
) -> Result<S3Listing> {
    list_s3_limited(bucket, prefix, delimiter, continuation_token, None)
}

fn list_s3_limited(
    bucket: &str,
    prefix: &str,
    delimiter: Option<&str>,
    continuation_token: Option<&str>,
    max_keys: Option<usize>,
) -> Result<S3Listing> {
    list_s3_request(&S3ListRequest {
        bucket,
        prefix,
        delimiter,
        continuation_token,
        max_keys,
        start_after: None,
    })
}

/// One ListObjectsV2 call.
///
/// A struct rather than seven positional arguments, because `start-after` and
/// `continuation-token` are both "where to resume" and swapping them at a call
/// site would produce a listing that is wrong rather than one that fails.
#[derive(Clone, Copy, Debug, Default)]
struct S3ListRequest<'a> {
    bucket: &'a str,
    prefix: &'a str,
    delimiter: Option<&'a str>,
    continuation_token: Option<&'a str>,
    max_keys: Option<usize>,
    /// Return only keys that sort strictly after this one. The archive
    /// fallback's whole cost argument rests on it - see
    /// [`archive_start_after_key`].
    start_after: Option<&'a str>,
}

fn list_s3_request(request: &S3ListRequest<'_>) -> Result<S3Listing> {
    let url = format!("https://{}.s3.amazonaws.com/", request.bucket);
    let client = metadata_http_client();
    let mut query = vec![
        ("list-type", "2".to_owned()),
        ("prefix", request.prefix.to_owned()),
    ];
    if let Some(delimiter) = request.delimiter {
        query.push(("delimiter", delimiter.to_owned()));
    }
    if let Some(token) = request.continuation_token {
        query.push(("continuation-token", token.to_owned()));
    }
    if let Some(max_keys) = request.max_keys {
        query.push(("max-keys", max_keys.to_string()));
    }
    if let Some(start_after) = request.start_after {
        query.push(("start-after", start_after.to_owned()));
    }
    let text = client
        .get(url)
        .query(&query)
        .send()?
        .error_for_status()?
        .text()?;
    let parsed: S3ListingXml = quick_xml::de::from_str(&text)?;
    Ok(parsed.into())
}

fn realtime_volume_id_from_prefix(site: &str, prefix: &str) -> Option<u16> {
    let trimmed = prefix.trim_end_matches('/');
    let mut parts = trimmed.split('/');
    let prefix_site = parts.next()?;
    if prefix_site != site {
        return None;
    }
    let volume_id = parts.next()?.parse::<u16>().ok()?;
    if parts.next().is_some() || volume_id >= REALTIME_VOLUME_ID_MODULUS {
        return None;
    }
    Some(volume_id)
}

fn latest_realtime_volume_id_from_active_ids(ids: &[u16]) -> Option<u16> {
    let mut ids = ids.to_vec();
    ids.sort_unstable();
    ids.dedup();
    if ids.is_empty() {
        return None;
    }
    if ids.len() == 1 {
        return ids.first().copied();
    }

    let mut largest_gap = 0u16;
    let mut latest_id = *ids.last()?;
    for (index, current) in ids.iter().copied().enumerate() {
        let next = if index + 1 == ids.len() {
            ids[0] + REALTIME_VOLUME_ID_MODULUS
        } else {
            ids[index + 1]
        };
        let gap = next - current;
        if gap > largest_gap {
            largest_gap = gap;
            latest_id = current;
        }
    }

    if largest_gap <= 1 {
        ids.last().copied()
    } else {
        Some(latest_id)
    }
}

fn realtime_volume_candidate_ids_from_active_ids(ids: &[u16]) -> Vec<u16> {
    let mut ids = ids.to_vec();
    ids.sort_unstable();
    ids.dedup();
    if ids.is_empty() {
        return Vec::new();
    }
    if ids.len() == 1 {
        return ids;
    }

    let mut candidates = Vec::new();
    for (index, current) in ids.iter().copied().enumerate() {
        let next = if index + 1 == ids.len() {
            ids[0] + REALTIME_VOLUME_ID_MODULUS
        } else {
            ids[index + 1]
        };
        if next - current > 1 {
            candidates.push(current);
        }
    }
    if candidates.is_empty() {
        candidates.push(*ids.last().expect("non-empty ids"));
    }
    candidates
}

/// The `max_count` active ids preceding `current_volume_id`, nearest first.
///
/// Distance is measured backwards around the wrapping counter, which is what
/// makes the wrap ordinary rather than a special case: with the counter at 1
/// the nearest preceding active id is 999, and no subtraction ever underflows
/// because the modulus is added before the subtraction. `current_volume_id`
/// itself is never returned.
fn preceding_realtime_volume_ids_from_active_ids(
    ids: &[u16],
    current_volume_id: u16,
    max_count: usize,
) -> Vec<u16> {
    let modulus = u32::from(REALTIME_VOLUME_ID_MODULUS);
    let current = u32::from(current_volume_id) % modulus;
    let mut by_distance = ids
        .iter()
        .copied()
        .filter(|id| *id < REALTIME_VOLUME_ID_MODULUS)
        .map(|id| ((current + modulus - u32::from(id)) % modulus, id))
        .filter(|(distance, _)| *distance > 0)
        .collect::<Vec<_>>();
    by_distance.sort_unstable();
    by_distance.dedup();
    by_distance
        .into_iter()
        .take(max_count)
        .map(|(_, id)| id)
        .collect()
}

/// Split one volume-id directory into the distinct volumes it holds, oldest
/// first.
///
/// A directory is normally one volume. It is two when the id counter wraps
/// back onto a directory the bucket has not expired yet - retention is days
/// (KTLX held ids 1..=680 spanning 2026-08-16 to 2026-08-18 when sampled on
/// 2026-08-18) while the counter cycles in roughly 999 volumes. Every chunk
/// key carries its own volume's start time, so grouping on that time separates
/// the two exactly; concatenating them instead would produce a file that is
/// neither volume.
fn realtime_volume_groups(
    site: &str,
    volume_id: u16,
    chunks: Vec<RealtimeChunkObject>,
) -> Vec<RealtimeLevel2Volume> {
    let mut by_time = BTreeMap::<DateTime<Utc>, Vec<RealtimeChunkObject>>::new();
    for chunk in chunks {
        by_time.entry(chunk.volume_time).or_default().push(chunk);
    }
    by_time
        .into_iter()
        .map(|(volume_time, mut chunks)| {
            chunks.sort_by_key(|chunk| chunk.chunk_id);
            // An `E` chunk on its own is not a whole volume: see
            // [`first_missing_chunk_id`]. The head of a volume can be gone
            // while its tail, including the `E`, is still listed.
            let complete = chunks.last().is_some_and(|chunk| chunk.chunk_type.is_end())
                && first_missing_chunk_id(&chunks).is_none();
            let total_size = chunks.iter().map(|chunk| chunk.object.size).sum();
            RealtimeLevel2Volume {
                site: site.to_owned(),
                volume_id,
                volume_time,
                chunks,
                complete,
                total_size,
            }
        })
        .collect()
}

/// The first chunk id missing from `chunks`, or `None` when they are the
/// contiguous run `1..=n` that a Level II volume is delivered as.
///
/// Chunk 1 carries the 24-byte Volume Header Record and the metadata block, so
/// a set that starts anywhere else concatenates into a file with no header at
/// all - and `decode_volume_from_bytes` does not check for the `AR2V` magic, it
/// reads the first 24 bytes as a header and then hunts for bzip blocks, so such
/// a file decodes into radials carrying a garbage site and time rather than
/// failing. That is the one failure this whole module has to prevent, because
/// it is the one an analyst cannot see.
///
/// This is not hypothetical. The chunks bucket expires individual chunk
/// OBJECTS by age, not whole volume directories, so a directory at the
/// retention edge is left holding only its tail. Measured against the live
/// bucket on 2026-08-18: `KTLX/969` held exactly `20260816-044049-054-I` and
/// `20260816-044049-055-E`; `KEAX/592` held 54..=61; `KEAX/742` held 21..=55;
/// `KRTX/897` held 6..=67; `KAMA/271` held 20..=55; `KAMA/455` held 43..=55.
/// Every one of those ends in an `E` chunk, which is all the completeness test
/// used to look at. Scanning all 642 KTLX directories found no gap in any
/// volume younger than the retention edge, so requiring contiguity never
/// rejects live data.
fn first_missing_chunk_id(chunks: &[RealtimeChunkObject]) -> Option<u16> {
    // An empty set is missing chunk 1 like any other headless set. Saying
    // "contiguous" here would let `download_realtime_volume` write a zero-byte
    // file and call it a volume.
    if chunks.is_empty() {
        return Some(1);
    }
    let mut expected = 1u16;
    for chunk in chunks {
        // Not `!=` against a running max: a repeated chunk id would otherwise
        // pass and be concatenated twice, which corrupts the file just as
        // thoroughly as a gap does.
        if chunk.chunk_id != expected {
            return Some(expected);
        }
        expected = expected.checked_add(1)?;
    }
    None
}

/// Pick the newest volume in `groups` that is a usable predecessor of the
/// volume starting at `current_volume_time`.
///
/// Complete only, because a partial predecessor would reintroduce the hole the
/// backfill exists to close, and strictly inside
/// `oldest_accepted..current_volume_time`, because an id directory that has
/// not expired since the counter last passed it holds a volume that is days
/// old, not minutes.
fn select_previous_complete_volume(
    groups: Vec<RealtimeLevel2Volume>,
    current_volume_time: DateTime<Utc>,
    oldest_accepted: DateTime<Utc>,
) -> Option<RealtimeLevel2Volume> {
    groups.into_iter().rev().find(|volume| {
        volume.complete
            && volume.volume_time < current_volume_time
            && volume.volume_time >= oldest_accepted
    })
}

fn parse_realtime_chunk_object(object: S3Object) -> Option<RealtimeChunkObject> {
    let key = object.key.clone();
    let mut path_parts = key.split('/');
    let site = path_parts.next()?.to_owned();
    let volume_id = path_parts.next()?.parse::<u16>().ok()?;
    let filename = path_parts.next()?;
    if path_parts.next().is_some() || volume_id >= REALTIME_VOLUME_ID_MODULUS {
        return None;
    }

    let mut name_parts = filename.split('-');
    let date = name_parts.next()?;
    let time = name_parts.next()?;
    let chunk_id = name_parts.next()?.parse::<u16>().ok()?;
    let chunk_type = RealtimeChunkType::from_code(name_parts.next()?)?;
    if name_parts.next().is_some() {
        return None;
    }

    let volume_time = NaiveDateTime::parse_from_str(&format!("{date}{time}"), "%Y%m%d%H%M%S")
        .ok()?
        .and_utc();

    Some(RealtimeChunkObject {
        object,
        site,
        volume_id,
        volume_time,
        chunk_id,
        chunk_type,
    })
}

fn realtime_volume_cache_filename(volume: &RealtimeLevel2Volume) -> String {
    format!(
        "{}{}_RT{:03}_V06",
        volume.site,
        volume.volume_time.format("%Y%m%d_%H%M%S"),
        volume.volume_id
    )
}

fn chunk_prefix_count_for_size(volume: &RealtimeLevel2Volume, size: u64) -> Option<usize> {
    if size == 0 {
        return Some(0);
    }

    let mut prefix_size = 0u64;
    for (index, chunk) in volume.chunks.iter().enumerate() {
        prefix_size = prefix_size.checked_add(chunk.object.size)?;
        if prefix_size == size {
            return Some(index + 1);
        }
        if prefix_size > size {
            return None;
        }
    }

    None
}

fn append_realtime_chunks(
    path: &Path,
    chunk_paths: &[PathBuf],
    expected_existing: u64,
    expected_total: u64,
    url: &str,
) -> Result<()> {
    let mut output = fs::OpenOptions::new().append(true).open(path)?;
    for chunk_path in chunk_paths {
        let mut chunk_file = fs::File::open(chunk_path)?;
        io::copy(&mut chunk_file, &mut output)?;
    }
    drop(output);

    let actual = path.metadata()?.len();
    if actual != expected_total {
        return Err(DataSourceError::DownloadSizeMismatch {
            url: url.to_owned(),
            expected: expected_total,
            actual,
        });
    }
    if actual < expected_existing {
        return Err(DataSourceError::DownloadSizeMismatch {
            url: url.to_owned(),
            expected: expected_existing,
            actual,
        });
    }
    Ok(())
}

/// Fetch one immutable S3 object, retrying the transport failures that a
/// pooled HTTPS connection produces under load.
///
/// Measured, not anticipated: pulling four volumes back to back while the live
/// poll listed the same prefix - which is exactly the traffic shape the
/// previous-volume backfill introduces - failed on
/// `https://…/KTLX/689/20260818-190055-001-S` with
/// `hyper::Error(IncompleteMessage)`, a connection S3 had already closed and
/// the pool handed out anyway. Without a retry that single chunk loses a whole
/// 10 MB volume; the live poll would recover on its next pass 1.2 s later, but
/// the backfill gets one attempt per session and would simply never appear.
///
/// Repeating a GET on an immutable chunk object is safe. Timeouts are NOT
/// retried, because a link that is actually dead should fail in one timeout
/// rather than three.
fn download_s3_object_to_path(bucket: &str, object: &S3Object, path: &Path) -> Result<()> {
    download_s3_object_to_path_cancellable(bucket, object, path, &|| false)
}

/// [`download_s3_object_to_path`], abandoned mid-body when `cancelled` starts
/// returning true.
///
/// A cancellation is not retriable - see [`is_retriable_download_error`] - so
/// it leaves the loop on the first pass rather than making two more requests
/// for a result nobody is waiting for.
fn download_s3_object_to_path_cancellable(
    bucket: &str,
    object: &S3Object,
    path: &Path,
    cancelled: &dyn Fn() -> bool,
) -> Result<()> {
    let url = format!("https://{bucket}.s3.amazonaws.com/{}", object.key);
    // Read once, so one object's whole retry sequence runs under one policy
    // even if the analyst moves the slider while it is in flight.
    let attempts = tuning::download_attempts();
    let backoff = tuning::retry_backoff();
    for attempt in 1..=attempts {
        match download_s3_object_attempt(&url, object, path, cancelled) {
            Ok(()) => return Ok(()),
            Err(error) => {
                if attempt == attempts || !is_retriable_download_error(&error) {
                    return Err(error);
                }
                eprintln!("retrying {url} after attempt {attempt}: {error}");
                thread::sleep(backoff);
            }
        }
    }
    unreachable!("the loop returns on its last attempt")
}

/// Whether repeating the request could plausibly succeed. A 404 or a full disk
/// will not fix itself; a dropped connection, a truncated body or a 5xx will.
fn is_retriable_download_error(error: &DataSourceError) -> bool {
    match error {
        // A body that ended early is the same fault as a dropped connection,
        // it just happened to close cleanly.
        DataSourceError::DownloadSizeMismatch { .. } => true,
        // Asked for, and asking again would be worse.
        DataSourceError::ObjectDownloadCancelled { .. }
        | DataSourceError::DownloadCancelled { .. } => false,
        DataSourceError::Http(http) => {
            if http.is_timeout() {
                return false;
            }
            match http.status() {
                Some(status) => {
                    status.is_server_error()
                        || status == reqwest::StatusCode::REQUEST_TIMEOUT
                        || status == reqwest::StatusCode::TOO_MANY_REQUESTS
                }
                // No status at all means the failure was below HTTP: connect,
                // TLS, or a connection closed mid-response.
                None => true,
            }
        }
        _ => false,
    }
}

fn download_s3_object_attempt(
    url: &str,
    object: &S3Object,
    path: &Path,
    cancelled: &dyn Fn() -> bool,
) -> Result<()> {
    let url = url.to_owned();
    let mut response = download_http_client()
        .get(&url)
        .send()?
        .error_for_status()?;
    let temp_path = path.with_extension("download");
    let mut temp_file = fs::File::create(&temp_path)?;
    let copied = copy_cancellable(&mut response, &mut temp_file, cancelled)?;
    drop(temp_file);
    let Some(copied) = copied else {
        // The partial file goes: a `.download` of the wrong length is exactly
        // what the size check above is there to reject, and leaving it would
        // hand the next attempt a decision it does not need to make.
        let _ = fs::remove_file(&temp_path);
        return Err(DataSourceError::ObjectDownloadCancelled {
            key: object.key.clone(),
        });
    };
    if copied != object.size {
        let _ = fs::remove_file(&temp_path);
        return Err(DataSourceError::DownloadSizeMismatch {
            url,
            expected: object.size,
            actual: copied,
        });
    }
    if path.exists() {
        fs::remove_file(path)?;
    }
    fs::rename(&temp_path, path)?;
    Ok(())
}

/// How much of a body one [`copy_cancellable`] block moves.
///
/// The trade is check frequency against syscall count. 64 KiB against an 11 MB
/// archive volume is ~170 checks, so a cancellation lands within a few
/// milliseconds of being raised, while the check itself - one relaxed atomic
/// load in the caller's closure - is nothing beside the socket read that
/// precedes it.
const CANCELLABLE_COPY_BLOCK_BYTES: usize = 64 * 1024;

/// [`io::copy`] with a cancellation check per block.
///
/// `Ok(None)` means the copy was abandoned; the caller owns what to do with
/// the partial output, because only it knows where that output lives.
fn copy_cancellable(
    reader: &mut dyn io::Read,
    writer: &mut dyn io::Write,
    cancelled: &dyn Fn() -> bool,
) -> Result<Option<u64>> {
    let mut buffer = vec![0_u8; CANCELLABLE_COPY_BLOCK_BYTES];
    let mut copied = 0_u64;
    loop {
        if cancelled() {
            return Ok(None);
        }
        let read = match reader.read(&mut buffer) {
            Ok(read) => read,
            // What `io::copy` does, and for the same reason: an interrupted
            // read moved no bytes and is not a failure.
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error.into()),
        };
        if read == 0 {
            return Ok(Some(copied));
        }
        writer.write_all(&buffer[..read])?;
        copied += read as u64;
    }
}

fn metadata_http_client() -> reqwest::blocking::Client {
    static CLIENT: OnceLock<reqwest::blocking::Client> = OnceLock::new();
    CLIENT
        .get_or_init(|| {
            build_http_client(HTTP_METADATA_TIMEOUT)
                .expect("metadata HTTP client should be constructible")
        })
        .clone()
}

fn download_http_client() -> reqwest::blocking::Client {
    static CLIENT: OnceLock<reqwest::blocking::Client> = OnceLock::new();
    CLIENT
        .get_or_init(|| {
            build_http_client(HTTP_DOWNLOAD_TIMEOUT)
                .expect("download HTTP client should be constructible")
        })
        .clone()
}

fn build_http_client(timeout: StdDuration) -> Result<reqwest::blocking::Client> {
    Ok(reqwest::blocking::Client::builder()
        .user_agent(HTTP_USER_AGENT)
        .connect_timeout(HTTP_CONNECT_TIMEOUT)
        .timeout(timeout)
        .build()?)
}

fn latest_object_cache() -> &'static Mutex<BTreeMap<LatestObjectCacheKey, CachedLatestObject>> {
    static CACHE: OnceLock<Mutex<BTreeMap<LatestObjectCacheKey, CachedLatestObject>>> =
        OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(BTreeMap::new()))
}

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
struct LatestObjectCacheKey {
    site: String,
    days_back: i64,
}

#[derive(Clone, Debug)]
struct CachedLatestObject {
    object: S3Object,
    fetched_at: Instant,
}

#[derive(Debug, Deserialize)]
struct S3ListingXml {
    #[serde(rename = "Contents", default)]
    contents: Vec<S3ObjectXml>,
    #[serde(rename = "CommonPrefixes", default)]
    common_prefixes: Vec<CommonPrefixXml>,
    #[serde(rename = "NextContinuationToken", default)]
    next_continuation_token: Option<String>,
}

impl From<S3ListingXml> for S3Listing {
    fn from(value: S3ListingXml) -> Self {
        Self {
            contents: value.contents.into_iter().map(Into::into).collect(),
            common_prefixes: value.common_prefixes.into_iter().map(Into::into).collect(),
            next_continuation_token: value.next_continuation_token,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct CommonPrefix {
    prefix: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct S3Listing {
    contents: Vec<S3Object>,
    common_prefixes: Vec<CommonPrefix>,
    /// Present exactly when the listing was truncated. Ignoring it would hand
    /// back the OLDEST page of a large prefix, which for "what is newest" is
    /// the wrong end - see [`ARCHIVE_LISTING_MAX_PAGES`].
    next_continuation_token: Option<String>,
}

#[derive(Debug, Deserialize)]
struct WeatherGovFeatureCollection {
    features: Vec<WeatherGovFeature>,
}

#[derive(Debug, Deserialize)]
struct WeatherGovFeature {
    geometry: Option<WeatherGovGeometry>,
    properties: WeatherGovProperties,
}

#[derive(Debug, Deserialize)]
struct WeatherGovGeometry {
    coordinates: Vec<f64>,
}

#[derive(Debug, Deserialize)]
struct WeatherGovProperties {
    id: Option<String>,
    name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct S3ObjectXml {
    #[serde(rename = "Key")]
    key: String,
    #[serde(rename = "LastModified")]
    last_modified: Option<String>,
    #[serde(rename = "Size")]
    size: u64,
}

#[derive(Debug, Deserialize)]
struct CommonPrefixXml {
    #[serde(rename = "Prefix")]
    prefix: String,
}

impl From<S3ObjectXml> for S3Object {
    fn from(value: S3ObjectXml) -> Self {
        Self {
            key: value.key,
            size: value.size,
            last_modified: value
                .last_modified
                .as_deref()
                .and_then(parse_s3_last_modified),
        }
    }
}

fn parse_s3_last_modified(value: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|time| time.with_timezone(&Utc))
}

impl From<CommonPrefixXml> for CommonPrefix {
    fn from(value: CommonPrefixXml) -> Self {
        Self {
            prefix: value.prefix,
        }
    }
}

const FALLBACK_SITE_IDS: &[&str] = &[
    "KABR", "KABX", "KAKQ", "KAMA", "KAMX", "KAPX", "KARX", "KATX", "KBBX", "KBGM", "KBHX", "KBIS",
    "KBLX", "KBMX", "KBOX", "KBRO", "KBUF", "KBYX", "KCAE", "KCBW", "KCBX", "KCCX", "KCLE", "KCLX",
    "KCRP", "KCXX", "KCYS", "KDAX", "KDDC", "KDFX", "KDGX", "KDIX", "KDLH", "KDMX", "KDOX", "KDTX",
    "KDVN", "KDYX", "KEAX", "KEMX", "KENX", "KEOX", "KEPZ", "KESX", "KEVX", "KEWX", "KEYX", "KFCX",
    "KFDR", "KFDX", "KFFC", "KFSD", "KFSX", "KFTG", "KFWS", "KGGW", "KGJX", "KGLD", "KGRB", "KGRK",
    "KGRR", "KGSP", "KGWX", "KGYX", "KHDX", "KHGX", "KHNX", "KHPX", "KHTX", "KICT", "KICX", "KILN",
    "KILX", "KIND", "KINX", "KIWA", "KIWX", "KJAX", "KJGX", "KJKL", "KLBB", "KLCH", "KLGX", "KLNX",
    "KLOT", "KLRX", "KLSX", "KLTX", "KLVX", "KLWX", "KLZK", "KMAF", "KMAX", "KMBX", "KMHX", "KMKX",
    "KMLB", "KMOB", "KMPX", "KMQT", "KMRX", "KMSX", "KMTX", "KMUX", "KMVX", "KMXX", "KNKX", "KNQA",
    "KOAX", "KOHX", "KOKX", "KOTX", "KPAH", "KPBZ", "KPDT", "KPOE", "KPUX", "KRAX", "KRGX", "KRIW",
    "KRLX", "KRTX", "KSFX", "KSGF", "KSHV", "KSJT", "KSOX", "KSRX", "KTBW", "KTFX", "KTLH", "KTLX",
    "KTWX", "KTYX", "KUDX", "KUEX", "KVAX", "KVBX", "KVNX", "KVTX", "KVWX", "KYUX", "PABC", "PACG",
    "PAEC", "PAHG", "PAIH", "PAKC", "PAPD", "PHKI", "PHMO", "PHWA", "RKJK", "RKSG", "TADW", "TATL",
    "TBNA", "TBOS", "TCLT", "TCMH", "TCVG", "TDAL", "TDAY", "TDCA", "TDEN", "TDFW", "TDTW", "TEWR",
    "TFLL", "THOU", "TIAD", "TIAH", "TIDS", "TJFK", "TJUA", "TLAS", "TLVE", "TMCI", "TMCO", "TMDW",
    "TMEM", "TMIA", "TMKE", "TMSP", "TMSY", "TOKC", "TORD", "TPBI", "TPHL", "TPHX", "TPIT", "TRDU",
    "TSDF", "TSJU", "TSLC", "TSTL", "TTPA", "TTUL",
];

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{SecondsFormat, TimeZone};

    #[test]
    fn site_can_carry_location() {
        let site = RadarSite::new("KTLX").with_location(
            Some("Norman".to_owned()),
            Some(35.333),
            Some(-97.278),
        );
        assert_eq!(site.name.as_deref(), Some("Norman"));
        assert_eq!(site.latitude_deg, Some(35.333));
    }

    #[test]
    fn fallback_has_many_sites() {
        assert!(fallback_sites().len() > 150);
    }

    #[test]
    fn newest_cached_level2_path_ignores_partial_empty_and_mdm_files() {
        let dir =
            std::env::temp_dir().join(format!("genericradar-cache-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("test cache dir");

        fs::write(dir.join("KTLX20260607_180000_V06"), b"old").expect("old cache file");
        fs::write(dir.join("KTLX20260607_181000_V06.download"), b"partial")
            .expect("partial cache file");
        fs::write(dir.join("KTLX20260607_182000_MDM"), b"mdm").expect("mdm cache file");
        fs::write(dir.join("KTLX20260607_183000_V06"), []).expect("empty cache file");
        fs::write(dir.join("KTLX20260607_184000_V06"), b"new").expect("new cache file");

        let newest = newest_cached_level2_path(&dir)
            .expect("cache scan")
            .expect("newest cache file");

        assert_eq!(
            newest.file_name().and_then(|name| name.to_str()),
            Some("KTLX20260607_184000_V06")
        );

        fs::remove_dir_all(&dir).expect("clean test cache dir");
    }

    #[test]
    fn realtime_latest_volume_id_handles_wraparound_window() {
        let wrapped_ids = [998, 999, 1, 2, 3];
        assert_eq!(
            latest_realtime_volume_id_from_active_ids(&wrapped_ids),
            Some(3)
        );

        let contiguous_ids = (102..=628).collect::<Vec<_>>();
        assert_eq!(
            latest_realtime_volume_id_from_active_ids(&contiguous_ids),
            Some(628)
        );
    }

    #[test]
    fn realtime_volume_candidates_include_each_active_run_end() {
        let wrapped_ids = [998, 999, 1, 2, 3];
        assert_eq!(
            realtime_volume_candidate_ids_from_active_ids(&wrapped_ids),
            vec![3, 999]
        );

        let kama_like_split_ids = [1, 2, 3, 73, 74, 75, 205, 206, 559];
        assert_eq!(
            realtime_volume_candidate_ids_from_active_ids(&kama_like_split_ids),
            vec![3, 75, 206, 559]
        );

        let contiguous_ids = (102..=628).collect::<Vec<_>>();
        assert_eq!(
            realtime_volume_candidate_ids_from_active_ids(&contiguous_ids),
            vec![628]
        );
    }

    #[test]
    fn previous_realtime_volume_id_wraps_through_the_active_set() {
        // The shape KTLX really had, listed on 2026-08-18T18:40Z: 642 active
        // ids in the runs 1..=2, 10..=29, 97..=684 and 969..=999.
        //
        // Those runs are ONE pass of the counter, not two cycles: 969 starts at
        // 2026-08-16T04:40:49Z, 999 at 08:13:07Z, 1 at 08:20:09Z, 2 at
        // 08:27:11Z and 684 at 2026-08-18T18:25:41Z. So the counter really does
        // step 999 -> 1 with no zero in between, and the whole set spans two
        // and a half days - the gaps are volumes the bucket has expired or the
        // radar never sent, not a second cycle.
        let mut ktlx_ids = (1..=2u16).collect::<Vec<_>>();
        ktlx_ids.extend(10..=29u16);
        ktlx_ids.extend(97..=684u16);
        ktlx_ids.extend(969..=999u16);

        assert_eq!(
            previous_realtime_volume_id_from_active_ids(&ktlx_ids, 684),
            Some(683)
        );
        // The wrap. `current - 1` would name 0 (absent, and one underflow away
        // from 65535); the newest run boundary would name 684, a live volume.
        // The counter really goes 999 -> 1, so 999 is the answer, and the real
        // bucket agrees: KTLX/999 starts 7m02s before KTLX/1.
        assert_eq!(
            previous_realtime_volume_id_from_active_ids(&ktlx_ids, 1),
            Some(999)
        );
        // Every id at a run boundary, walked both ways. 97 is the oldest id the
        // bucket still holds, so its predecessor is the previous run's end (29,
        // which is 2026-08-16T11:37Z - eight hours earlier, which only the time
        // guard in `select_previous_complete_volume` can reject).
        assert_eq!(
            previous_realtime_volume_id_from_active_ids(&ktlx_ids, 97),
            Some(29)
        );
        assert_eq!(
            previous_realtime_volume_id_from_active_ids(&ktlx_ids, 10),
            Some(2)
        );
        assert_eq!(
            previous_realtime_volume_id_from_active_ids(&ktlx_ids, 2),
            Some(1)
        );
        // And the case a wrapping counter cannot resolve on its own: walking
        // backwards from 969 the nearest active id is 684, which is two days
        // LATER in time. An id alone cannot tell "285 volumes ago" from "715
        // volumes ahead", which is why `select_previous_complete_volume`
        // demands the candidate's own start time be earlier and recent.
        assert_eq!(
            previous_realtime_volume_id_from_active_ids(&ktlx_ids, 969),
            Some(684)
        );

        // Nearest preceding ACTIVE id, not `current - 1`: backwards from 3 the
        // distances are 900 -> 103 and 7 -> 996, so 900 wins.
        let sparse = [3u16, 7, 900];
        assert_eq!(
            previous_realtime_volume_id_from_active_ids(&sparse, 7),
            Some(3)
        );
        assert_eq!(
            previous_realtime_volume_id_from_active_ids(&sparse, 3),
            Some(900)
        );

        // Nothing precedes the only active volume, and nothing precedes
        // nothing.
        assert_eq!(previous_realtime_volume_id_from_active_ids(&[42], 42), None);
        assert_eq!(previous_realtime_volume_id_from_active_ids(&[], 42), None);
    }

    /// Every element of a set that straddles the wrap, and the same set with
    /// ids removed the way the bucket really ages them out.
    ///
    /// The counter runs 1..=999 and steps 999 -> 1 with no zero (confirmed
    /// against the live bucket: KTLX/999 = 2026-08-16T08:13:07Z, KTLX/1 =
    /// 08:20:09Z, KTLX/2 = 08:27:11Z). The distances below are therefore taken
    /// modulo 1000, which counts a value the counter never emits; that is
    /// harmless because 0 is never a member, so it can never be chosen, and the
    /// ORDER of the real candidates is unaffected.
    #[test]
    fn previous_volume_id_is_right_for_every_element_across_the_wrap() {
        // Time order of this set is 998, 999, 1, 2.
        let straddling = [998u16, 999, 1, 2];
        // 2 <- 1: distance 1.
        assert_eq!(
            previous_realtime_volume_id_from_active_ids(&straddling, 2),
            Some(1)
        );
        // 1 <- 999: distance (1 + 1000 - 999) % 1000 = 2, beating 998 at 3.
        assert_eq!(
            previous_realtime_volume_id_from_active_ids(&straddling, 1),
            Some(999)
        );
        // 999 <- 998: distance 1.
        assert_eq!(
            previous_realtime_volume_id_from_active_ids(&straddling, 999),
            Some(998)
        );
        // 998 is the OLDEST member, so nothing in the set really precedes it.
        // The walk answers 2 (distance 996) because that is the nearest id
        // going backwards; the whole answer is a volume 996 steps back, which
        // in wall-clock terms is ahead. `select_previous_complete_volume`'s
        // time bound is what turns that into "no predecessor", and the ignored
        // real-feed test below shows it doing so.
        assert_eq!(
            previous_realtime_volume_id_from_active_ids(&straddling, 998),
            Some(2)
        );

        // The same set with ids aged out. 999 gone: 1's predecessor becomes
        // 998 (distance 3) rather than an absent 999 or an underflowed 0.
        assert_eq!(
            previous_realtime_volume_id_from_active_ids(&[998, 1, 2], 1),
            Some(998)
        );
        // 1 gone: 2's predecessor becomes 999 (distance 3), not 1.
        assert_eq!(
            previous_realtime_volume_id_from_active_ids(&[998, 999, 2], 2),
            Some(999)
        );
        // Both gone: 2 falls back across the wrap to 998, distance 4.
        assert_eq!(
            previous_realtime_volume_id_from_active_ids(&[998, 2], 2),
            Some(998)
        );
        // The current id absent from the set is the same walk: nothing about
        // the answer depends on the counter's own directory still existing.
        assert_eq!(
            previous_realtime_volume_id_from_active_ids(&[998, 999], 1),
            Some(999)
        );

        // A radar that just came up has one directory and no predecessor, and
        // an off-air site has none at all. Neither may panic or list anything.
        assert_eq!(previous_realtime_volume_id_from_active_ids(&[7], 7), None);
        assert_eq!(previous_realtime_volume_id_from_active_ids(&[], 7), None);
        // Out-of-range ids from a malformed prefix are ignored rather than
        // wrapped into a real id.
        assert_eq!(
            previous_realtime_volume_id_from_active_ids(&[1000, 1001, 5], 7),
            Some(5)
        );
    }

    #[test]
    fn preceding_realtime_volume_ids_walk_backwards_nearest_first() {
        // Backwards distances from 2: 1 -> 1, 999 -> 3, 998 -> 4, 997 -> 5,
        // 4 -> 998, 3 -> 999. The lookback therefore crosses the wrap before
        // it ever reaches the ids ahead of the counter.
        let ids = [1u16, 2, 3, 4, 997, 998, 999];
        assert_eq!(
            preceding_realtime_volume_ids_from_active_ids(&ids, 2, 4),
            vec![1, 999, 998, 997]
        );
        assert_eq!(
            preceding_realtime_volume_ids_from_active_ids(&ids, 2, 1),
            vec![1]
        );
        assert!(preceding_realtime_volume_ids_from_active_ids(&ids, 2, 0).is_empty());

        // A radar that has just come back on the air has one directory. The
        // real lookback is `REALTIME_PREVIOUS_VOLUME_LOOKBACK`, and it must
        // produce nothing to list rather than four listings that all miss: a
        // backfill that cannot happen must cost no requests at all.
        assert!(
            preceding_realtime_volume_ids_from_active_ids(
                &[689],
                689,
                REALTIME_PREVIOUS_VOLUME_LOOKBACK
            )
            .is_empty()
        );
        // And when the predecessors have aged out, the walk offers only what is
        // really there - four asked for, two available.
        assert_eq!(
            preceding_realtime_volume_ids_from_active_ids(
                &[680, 689],
                689,
                REALTIME_PREVIOUS_VOLUME_LOOKBACK
            ),
            vec![680]
        );
    }

    #[test]
    fn realtime_volume_groups_split_a_recycled_volume_id() {
        let old_time = Utc.with_ymd_and_hms(2026, 8, 16, 8, 20, 9).unwrap();
        let new_time = Utc.with_ymd_and_hms(2026, 8, 18, 17, 57, 28).unwrap();
        let chunks = vec![
            test_chunk(1, new_time, 2, RealtimeChunkType::Intermediate, 20),
            test_chunk(1, old_time, 1, RealtimeChunkType::Start, 4),
            test_chunk(1, new_time, 1, RealtimeChunkType::Start, 10),
            test_chunk(1, old_time, 2, RealtimeChunkType::End, 6),
        ];

        let groups = realtime_volume_groups("KTLX", 1, chunks);

        assert_eq!(groups.len(), 2);
        // Oldest first, and each group carries only its own chunks: the two
        // days between them must not end up concatenated into one file.
        assert_eq!(groups[0].volume_time, old_time);
        assert_eq!(groups[0].total_size, 10);
        assert!(groups[0].complete);
        assert_eq!(groups[1].volume_time, new_time);
        assert_eq!(groups[1].total_size, 30);
        assert!(!groups[1].complete);
        assert_eq!(
            groups[1]
                .chunks
                .iter()
                .map(|chunk| chunk.chunk_id)
                .collect::<Vec<_>>(),
            vec![1, 2]
        );
    }

    /// A chunk set whose head has been aged out is not a complete volume, even
    /// though its last chunk is an `E`.
    ///
    /// Every case here is a directory that really existed in the chunks bucket
    /// on 2026-08-18; the chunk ids are copied from the listings.
    #[test]
    fn a_head_truncated_chunk_set_is_not_a_complete_volume() {
        let volume_time = Utc.with_ymd_and_hms(2026, 8, 16, 4, 40, 49).unwrap();

        // KTLX/969 held exactly these two objects and nothing else.
        let ktlx_969 = realtime_volume_groups(
            "KTLX",
            969,
            vec![
                test_chunk(
                    969,
                    volume_time,
                    54,
                    RealtimeChunkType::Intermediate,
                    71_000,
                ),
                test_chunk(969, volume_time, 55, RealtimeChunkType::End, 12_000),
            ],
        );
        assert_eq!(ktlx_969.len(), 1);
        assert!(
            !ktlx_969[0].complete,
            "chunks 54..=55 are the tail of a volume, not a volume"
        );
        assert_eq!(first_missing_chunk_id(&ktlx_969[0].chunks), Some(1));

        // KAMA/455 held 43..=55, KEAX/592 held 54..=61, KRTX/897 held 6..=67.
        for (site, volume_id, first, last) in [
            ("KAMA", 455u16, 43u16, 55u16),
            ("KEAX", 592, 54, 61),
            ("KRTX", 897, 6, 67),
        ] {
            let chunks = (first..=last)
                .map(|chunk_id| {
                    let chunk_type = if chunk_id == last {
                        RealtimeChunkType::End
                    } else {
                        RealtimeChunkType::Intermediate
                    };
                    test_chunk(volume_id, volume_time, chunk_id, chunk_type, 60_000)
                })
                .collect::<Vec<_>>();
            let groups = realtime_volume_groups(site, volume_id, chunks);
            assert!(
                !groups[0].complete,
                "{site}/{volume_id} starts at chunk {first}, so it has no volume header"
            );
        }

        // A gap in the middle is just as fatal, and is what a zero-byte chunk
        // (dropped by the size filter in the listing) leaves behind.
        let gapped = realtime_volume_groups(
            "KTLX",
            683,
            vec![
                test_chunk(683, volume_time, 1, RealtimeChunkType::Start, 10),
                test_chunk(683, volume_time, 2, RealtimeChunkType::Intermediate, 10),
                test_chunk(683, volume_time, 4, RealtimeChunkType::End, 10),
            ],
        );
        assert!(!gapped[0].complete);
        assert_eq!(first_missing_chunk_id(&gapped[0].chunks), Some(3));

        // The whole run still is one.
        let whole = realtime_volume_groups(
            "KTLX",
            683,
            vec![
                test_chunk(683, volume_time, 1, RealtimeChunkType::Start, 10),
                test_chunk(683, volume_time, 2, RealtimeChunkType::Intermediate, 10),
                test_chunk(683, volume_time, 3, RealtimeChunkType::End, 10),
            ],
        );
        assert!(whole[0].complete);
        assert_eq!(first_missing_chunk_id(&whole[0].chunks), None);

        // A repeated chunk id would be concatenated twice, so it is a gap by
        // another name: after the second 2 the run can no longer reach 3.
        let duplicated = [
            test_chunk(683, volume_time, 1, RealtimeChunkType::Start, 10),
            test_chunk(683, volume_time, 2, RealtimeChunkType::Intermediate, 10),
            test_chunk(683, volume_time, 2, RealtimeChunkType::Intermediate, 10),
            test_chunk(683, volume_time, 3, RealtimeChunkType::End, 10),
        ];
        assert_eq!(first_missing_chunk_id(&duplicated), Some(3));

        // And an empty set is missing chunk 1, not "contiguous", so it can
        // never be written out as a zero-byte volume.
        assert_eq!(first_missing_chunk_id(&[]), Some(1));
    }

    /// A gapped chunk set must be refused before any bytes are written, so it
    /// cannot leave a file that later decodes as a real volume.
    #[test]
    fn assembling_a_gapped_chunk_set_is_refused_and_writes_nothing() {
        let dir = unique_test_dir("refuse-gapped");
        let volume_time = Utc.with_ymd_and_hms(2026, 8, 16, 4, 40, 49).unwrap();
        let volume = RealtimeLevel2Volume {
            site: "KTLX".to_owned(),
            volume_id: 969,
            volume_time,
            chunks: vec![
                test_chunk(
                    969,
                    volume_time,
                    54,
                    RealtimeChunkType::Intermediate,
                    71_000,
                ),
                test_chunk(969, volume_time, 55, RealtimeChunkType::End, 12_000),
            ],
            complete: true, // as the old grouping would have reported it
            total_size: 83_000,
        };

        let error = download_realtime_volume(&volume, &dir)
            .expect_err("a headless chunk set must not be assembled");
        assert!(
            matches!(
                error,
                DataSourceError::ChunkSetNotContiguous {
                    missing_chunk_id: 1,
                    last_chunk_id: 55,
                    ..
                }
            ),
            "unexpected error: {error}"
        );
        // Refused before `create_dir_all`, so not even the cache directory is
        // brought into existence by a volume that can never be assembled.
        assert!(
            !dir.exists(),
            "{} should not have been created",
            dir.display()
        );
    }

    /// A file the cache already holds is not fetched again, and the check is
    /// made before any network call - this test runs with no network at all.
    #[test]
    fn a_cached_realtime_volume_is_reported_as_a_cache_hit_without_downloading() {
        let dir = unique_test_dir("cache-hit");
        fs::create_dir_all(&dir).expect("test cache dir");
        let volume = test_realtime_volume_with_sizes(&[4, 6, 10]);

        // The bytes do not matter, only that the assembled file is already the
        // exact size of the volume: that is the whole cache test.
        let cached = dir.join(realtime_volume_cache_filename(&volume));
        fs::write(&cached, vec![0u8; 20]).expect("pre-populated cache file");

        let downloaded =
            download_realtime_volume(&volume, &dir).expect("cached volume resolves offline");
        assert!(downloaded.cache_hit);
        assert_eq!(downloaded.path, cached);
        assert_eq!(downloaded.object.size, 20);
        // No chunk cache was created, which is the proof that no chunk was
        // requested: the chunk directory is made only on the download path.
        assert!(!dir.join(".chunks").exists());

        fs::remove_dir_all(&dir).expect("clean cache-hit test dir");
    }

    /// A cache directory that cannot be created is an error, not a panic, and
    /// leaves nothing behind.
    #[test]
    fn an_unusable_cache_directory_is_an_error_rather_than_a_panic() {
        let dir = unique_test_dir("bad-cache");
        fs::create_dir_all(&dir).expect("test parent dir");
        // A plain file standing where the cache directory should be. Portable,
        // and it exercises the same `create_dir_all` failure a read-only or
        // permission-denied directory produces.
        let blocked = dir.join("not-a-directory");
        fs::write(&blocked, b"occupied").expect("blocking file");

        let volume = test_realtime_volume_with_sizes(&[4, 6, 10]);
        let error = download_realtime_volume(&volume, &blocked)
            .expect_err("a file is not a cache directory");
        assert!(
            matches!(error, DataSourceError::Io(_)),
            "unexpected error: {error}"
        );
        assert_eq!(
            fs::read(&blocked).expect("blocking file survives"),
            b"occupied"
        );

        fs::remove_dir_all(&dir).expect("clean bad-cache test dir");
    }

    #[test]
    fn previous_volume_selection_rejects_stale_and_partial_candidates() {
        let current_time = Utc.with_ymd_and_hms(2026, 8, 18, 17, 57, 28).unwrap();
        let oldest_accepted =
            current_time - Duration::minutes(REALTIME_PREVIOUS_VOLUME_MAX_GAP_MINUTES);
        let stale = test_group(Utc.with_ymd_and_hms(2026, 8, 16, 8, 20, 9).unwrap(), true);
        let partial = test_group(
            Utc.with_ymd_and_hms(2026, 8, 18, 17, 50, 26).unwrap(),
            false,
        );
        let previous = test_group(Utc.with_ymd_and_hms(2026, 8, 18, 17, 50, 26).unwrap(), true);
        let ahead = test_group(Utc.with_ymd_and_hms(2026, 8, 18, 18, 4, 30).unwrap(), true);

        // A recycled id holding a two-day-old volume is not the predecessor.
        assert!(
            select_previous_complete_volume(vec![stale.clone()], current_time, oldest_accepted)
                .is_none()
        );
        // Neither is a volume that never finished ...
        assert!(
            select_previous_complete_volume(vec![partial.clone()], current_time, oldest_accepted)
                .is_none()
        );
        // ... nor one that starts after the volume we are backfilling behind.
        assert!(
            select_previous_complete_volume(vec![ahead.clone()], current_time, oldest_accepted)
                .is_none()
        );

        let chosen = select_previous_complete_volume(
            vec![stale, partial, previous.clone(), ahead],
            current_time,
            oldest_accepted,
        )
        .expect("recent complete predecessor is selected");
        assert_eq!(chosen.volume_time, previous.volume_time);
    }

    /// The wrap, the staleness guard and the predecessor walk checked against
    /// the bucket rather than against a fixture.
    ///
    /// ```text
    /// cargo test --release -p data_source -- --ignored --nocapture \
    ///     the_real_feed_agrees_which_volume_precedes_the_live_one
    /// ```
    ///
    /// `RADAR_LIVE_SITE` picks the site (default KTLX).
    #[test]
    #[ignore = "lists the real NEXRAD chunks bucket"]
    fn the_real_feed_agrees_which_volume_precedes_the_live_one() {
        let site = std::env::var("RADAR_LIVE_SITE").unwrap_or_else(|_| "KTLX".to_owned());
        let active = list_active_realtime_volume_ids(&site).expect("active id listing");
        assert!(!active.is_empty(), "{site} has no volumes in the bucket");
        println!("{site}: {} active ids", active.len());

        let live = latest_realtime_level2_volume(&site).expect("live volume");
        let previous =
            previous_complete_realtime_level2_volume(&site, live.volume_id, live.volume_time)
                .expect("previous complete volume");
        let gap = live.volume_time - previous.volume_time;
        println!(
            "live id {:>3} at {} ({} chunk(s), complete {})",
            live.volume_id,
            live.volume_time.to_rfc3339(),
            live.chunks.len(),
            live.complete
        );
        println!(
            "prev id {:>3} at {} ({} chunk(s), complete {}), {} s earlier",
            previous.volume_id,
            previous.volume_time.to_rfc3339(),
            previous.chunks.len(),
            previous.complete,
            gap.num_seconds()
        );

        // The predecessor is the nearest ACTIVE id walking backwards, not
        // `live - 1`: at the wrap `live - 1` names 0, which the counter never
        // emits, and after an expiry it names a directory that is gone.
        let expected = previous_realtime_volume_id_from_active_ids(&active, live.volume_id);
        let naive = live.volume_id.wrapping_sub(1);
        println!("nearest preceding active id = {expected:?}, naive live-1 = {naive}");
        if let Some(expected) = expected
            && expected == previous.volume_id
        {
            // The ordinary case: the immediate predecessor was usable.
        } else {
            // The walk skipped past ids that were expired, incomplete or stale.
            // Whatever it landed on still has to satisfy every invariant below.
            println!("walked past {expected:?} to {}", previous.volume_id);
        }

        assert!(previous.complete, "a backfill must be a whole volume");
        assert_eq!(
            first_missing_chunk_id(&previous.chunks),
            None,
            "a complete volume is the contiguous run 1..=n"
        );
        assert!(
            previous.volume_time < live.volume_time,
            "the predecessor must start earlier, whatever its id says"
        );
        assert!(
            gap <= Duration::minutes(REALTIME_PREVIOUS_VOLUME_MAX_GAP_MINUTES),
            "{} minutes is not the previous volume, it is a recycled id",
            gap.num_minutes()
        );
        assert_ne!(previous.volume_id, live.volume_id);
    }

    /// The bucket really does hold volume directories whose head has been aged
    /// out, and they must not be reported as complete or assembled.
    ///
    /// ```text
    /// cargo test --release -p data_source -- --ignored --nocapture \
    ///     head_truncated_directories_in_the_real_bucket_are_refused
    /// ```
    #[test]
    #[ignore = "lists the real NEXRAD chunks bucket"]
    fn head_truncated_directories_in_the_real_bucket_are_refused() {
        let site = std::env::var("RADAR_LIVE_SITE").unwrap_or_else(|_| "KTLX".to_owned());
        let active = list_active_realtime_volume_ids(&site).expect("active id listing");
        let dir = unique_test_dir("truncated-refusal");

        // The truncation lives at the retention edge, and because the counter
        // wraps, the oldest volumes by TIME are not the lowest ids: they are
        // the first ids of each contiguous run, which is where expiry has been
        // eating. On 2026-08-18 KTLX's runs began at 1, 10, 97 and 969, and the
        // head-truncated directory was 969 - the run start with the HIGHEST id.
        let run_starts = active
            .iter()
            .copied()
            .enumerate()
            .filter(|(index, id)| *index == 0 || active[index - 1] + 1 != *id)
            .map(|(_, id)| id)
            .collect::<Vec<_>>();
        // Deduplicated: runs can start within ten ids of each other, and
        // visiting a directory twice would double-count it in the total below.
        let mut probe_ids = run_starts
            .iter()
            .flat_map(|start| {
                (*start..)
                    .take(10)
                    .filter(|id| active.binary_search(id).is_ok())
            })
            .collect::<Vec<_>>();
        probe_ids.sort_unstable();
        probe_ids.dedup();
        println!(
            "{site} runs start at {run_starts:?}; probing {} ids",
            probe_ids.len()
        );

        let mut found = 0usize;
        for volume_id in probe_ids {
            let Ok(groups) = realtime_level2_volume_groups_for_id(&site, volume_id) else {
                continue;
            };
            for group in groups {
                let Some(missing) = first_missing_chunk_id(&group.chunks) else {
                    continue;
                };
                found += 1;
                println!(
                    "{site}/{volume_id} at {}: chunks {}..={} ({} of them), missing {missing}, last type {}",
                    group.volume_time.to_rfc3339(),
                    group.chunks.first().expect("non-empty group").chunk_id,
                    group.chunks.last().expect("non-empty group").chunk_id,
                    group.chunks.len(),
                    group
                        .chunks
                        .last()
                        .expect("non-empty group")
                        .chunk_type
                        .label()
                );
                assert!(
                    !group.complete,
                    "a gapped chunk set is not a complete volume even when it ends in E"
                );
                let error = download_realtime_volume(&group, &dir)
                    .expect_err("a gapped chunk set must not be assembled");
                assert!(
                    matches!(error, DataSourceError::ChunkSetNotContiguous { .. }),
                    "unexpected error: {error}"
                );
            }
        }
        println!("{found} head-truncated group(s) at the {site} retention edge");
        assert!(
            !dir.exists(),
            "nothing may be written for a volume that cannot be assembled"
        );
    }

    /// The backfill must not slow the live poll. Measured, not argued: the poll
    /// is timed on its own and then timed again while a whole 10 MB volume is
    /// being pulled on another thread through the same two shared
    /// `reqwest::blocking::Client`s.
    ///
    /// ```text
    /// cargo test --release -p data_source -- --ignored --nocapture \
    ///     a_backfill_does_not_slow_the_live_poll
    /// ```
    #[test]
    #[ignore = "downloads a whole volume from the real NEXRAD chunks bucket"]
    fn a_backfill_does_not_slow_the_live_poll() {
        /// The live worker's cadence. A poll that stays well inside this cannot
        /// make the session miss a chunk.
        const LIVE_POLL_INTERVAL_MS: u128 = 1_200;

        let site = std::env::var("RADAR_LIVE_SITE").unwrap_or_else(|_| "KTLX".to_owned());
        let live_cache = unique_test_dir("poll-latency-live");
        let backfill_cache = unique_test_dir("poll-latency-backfill");

        // One warm-up so the connection pool and the DNS answer are not
        // charged to the first sample.
        let live = latest_realtime_level2_volume(&site).expect("warm-up listing");
        let _ = download_realtime_volume(&live, &live_cache).expect("warm-up download");

        let poll_once = |cache: &Path| -> u128 {
            let started = Instant::now();
            let volume = latest_realtime_level2_volume(&site).expect("live listing");
            let _ = download_realtime_volume(&volume, cache).expect("live download");
            started.elapsed().as_millis()
        };

        let idle = (0..10).map(|_| poll_once(&live_cache)).collect::<Vec<_>>();

        // A cold cache directory, so the backfill really moves the bytes.
        let previous =
            previous_complete_realtime_level2_volume(&site, live.volume_id, live.volume_time)
                .expect("previous complete volume");
        let backfill_bytes = previous.total_size;
        let backfill_dir = backfill_cache.clone();
        // One volume transfers in about 1.5 s, which is barely three polls, so
        // the transfer is repeated into fresh cache directories to hold the
        // link busy long enough for the poll distribution to mean something.
        const BACKFILL_REPEATS: usize = 4;
        let backfill = thread::spawn(move || {
            let started = Instant::now();
            let mut cache_hits = 0usize;
            for repeat in 0..BACKFILL_REPEATS {
                let downloaded =
                    download_realtime_volume(&previous, &backfill_dir.join(repeat.to_string()))
                        .expect("backfill download");
                if downloaded.cache_hit {
                    cache_hits += 1;
                }
            }
            (started.elapsed().as_millis(), cache_hits)
        });

        let mut loaded = Vec::new();
        while !backfill.is_finished() {
            loaded.push(poll_once(&live_cache));
        }
        let (backfill_ms, cache_hits) = backfill.join().expect("backfill thread");

        let median = |samples: &[u128]| {
            let mut sorted = samples.to_vec();
            sorted.sort_unstable();
            sorted[sorted.len() / 2]
        };
        println!(
            "backfill: {BACKFILL_REPEATS} x {:.1} MiB in {backfill_ms} ms ({cache_hits} cache hit(s))",
            backfill_bytes as f64 / (1_024.0 * 1_024.0)
        );
        // Every repeat used a cold cache directory, so this really was
        // sustained transfer and not a metadata check.
        assert_eq!(cache_hits, 0);
        println!(
            "poll idle       : n={} median {} ms max {} ms  {idle:?}",
            idle.len(),
            median(&idle),
            idle.iter().max().copied().unwrap_or_default()
        );
        println!(
            "poll under load : n={} median {} ms max {} ms  {loaded:?}",
            loaded.len(),
            median(&loaded),
            loaded.iter().max().copied().unwrap_or_default()
        );

        assert!(
            !loaded.is_empty(),
            "the backfill finished before a poll ran"
        );
        // The requirement is not "identical" - it is a shared link, and one
        // number off a public bucket is noise. The requirement is that the poll
        // still fits inside the interval it is run on, so the live tilt keeps
        // arriving while the backfill transfers.
        assert!(
            median(&loaded) < LIVE_POLL_INTERVAL_MS,
            "median poll under load was {} ms, which does not fit in the {LIVE_POLL_INTERVAL_MS} ms live cadence",
            median(&loaded)
        );

        let _ = fs::remove_dir_all(&live_cache);
        let _ = fs::remove_dir_all(&backfill_cache);
    }

    #[test]
    fn realtime_chunk_key_parser_extracts_volume_metadata() {
        let chunk = parse_realtime_chunk_object(S3Object {
            key: "KGGW/628/20260608-002828-025-I".to_owned(),
            size: 129_481,
            last_modified: None,
        })
        .expect("valid realtime chunk key");

        assert_eq!(chunk.site, "KGGW");
        assert_eq!(chunk.volume_id, 628);
        assert_eq!(chunk.chunk_id, 25);
        assert_eq!(chunk.chunk_type, RealtimeChunkType::Intermediate);
        assert_eq!(chunk.volume_time.to_rfc3339(), "2026-06-08T00:28:28+00:00");
    }

    #[test]
    fn s3_last_modified_parser_handles_aws_timestamp() {
        let parsed =
            parse_s3_last_modified("2026-06-08T22:23:33.000Z").expect("S3 LastModified parses");

        assert_eq!(parsed.to_rfc3339(), "2026-06-08T22:23:33+00:00");
    }

    #[test]
    fn realtime_chunk_prefix_size_accepts_only_chunk_boundaries() {
        let volume = test_realtime_volume_with_sizes(&[4, 6, 10]);

        assert_eq!(chunk_prefix_count_for_size(&volume, 0), Some(0));
        assert_eq!(chunk_prefix_count_for_size(&volume, 4), Some(1));
        assert_eq!(chunk_prefix_count_for_size(&volume, 10), Some(2));
        assert_eq!(chunk_prefix_count_for_size(&volume, 20), Some(3));
        assert_eq!(chunk_prefix_count_for_size(&volume, 5), None);
        assert_eq!(chunk_prefix_count_for_size(&volume, 21), None);
    }

    #[test]
    fn only_transport_failures_are_worth_repeating() {
        // A body that stopped early is the retriable case that does not look
        // like a network error at the type level.
        assert!(is_retriable_download_error(
            &DataSourceError::DownloadSizeMismatch {
                url: "test://chunk".to_owned(),
                expected: 10,
                actual: 4,
            }
        ));
        // A full or read-only disk will not fix itself, and neither will a
        // cancelled session or a chunk set with a hole in it. Retrying those
        // would spend the live worker's time on a settled answer.
        assert!(!is_retriable_download_error(&DataSourceError::Io(
            io::Error::from(io::ErrorKind::PermissionDenied)
        )));
        assert!(!is_retriable_download_error(&DataSourceError::NoObjects {
            bucket: LEVEL2_CHUNKS_BUCKET.to_owned(),
            prefix: "KTLX/1/".to_owned(),
        }));
        assert!(!is_retriable_download_error(
            &DataSourceError::DownloadCancelled {
                site: "KTLX".to_owned(),
                volume_id: 683,
            }
        ));
        assert!(!is_retriable_download_error(
            &DataSourceError::ChunkSetNotContiguous {
                site: "KTLX".to_owned(),
                volume_id: 969,
                volume_time: Utc.with_ymd_and_hms(2026, 8, 16, 4, 40, 49).unwrap(),
                missing_chunk_id: 1,
                last_chunk_id: 55,
            }
        ));
    }

    /// A chunk that is genuinely gone must fail on the first attempt, not after
    /// three. Uses a key that cannot exist rather than a fixture, because the
    /// shape of S3's 404 is the thing under test.
    #[test]
    #[ignore = "asks the real NEXRAD chunks bucket for a key that does not exist"]
    fn a_missing_chunk_object_is_not_retried() {
        let dir = unique_test_dir("missing-chunk");
        fs::create_dir_all(&dir).expect("test dir");
        let object = S3Object {
            key: "KTLX/1/29260818-000000-001-S".to_owned(),
            size: 1_024,
            last_modified: None,
        };

        let started = Instant::now();
        let error = download_s3_object_to_path(LEVEL2_CHUNKS_BUCKET, &object, &dir.join("chunk"))
            .expect_err("a key from the year 2926 does not exist");
        let elapsed = started.elapsed();
        println!("404 rejected in {} ms: {error}", elapsed.as_millis());

        assert!(
            !is_retriable_download_error(&error),
            "a 404 must not be retried: {error}"
        );
        assert!(
            !dir.join("chunk").exists() && !dir.join("chunk.download").exists(),
            "a failed fetch must leave no file behind"
        );

        fs::remove_dir_all(&dir).expect("clean missing-chunk test dir");
    }

    #[test]
    fn realtime_append_adds_only_missing_chunk_bytes() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system time")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "radar-rs-append-test-{}-{unique}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("test dir");

        let assembled = dir.join("assembled");
        let chunk_two = dir.join("002-I");
        let chunk_three = dir.join("003-E");
        fs::write(&assembled, b"aaaa").expect("existing prefix");
        fs::write(&chunk_two, b"bb").expect("chunk two");
        fs::write(&chunk_three, b"cccc").expect("chunk three");

        append_realtime_chunks(
            &assembled,
            &[chunk_two, chunk_three],
            4,
            10,
            "test://chunks",
        )
        .expect("append missing chunks");

        assert_eq!(
            fs::read(&assembled).expect("assembled bytes"),
            b"aaaabbcccc"
        );
        fs::remove_dir_all(&dir).expect("clean append test dir");
    }

    /// A directory name no other test or run can collide with, so these tests
    /// stay correct when the suite runs in parallel or twice at once.
    fn unique_test_dir(label: &str) -> PathBuf {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system time")
            .as_nanos();
        let dir =
            std::env::temp_dir().join(format!("radar-rs-{label}-{}-{unique}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    fn test_chunk(
        volume_id: u16,
        volume_time: DateTime<Utc>,
        chunk_id: u16,
        chunk_type: RealtimeChunkType,
        size: u64,
    ) -> RealtimeChunkObject {
        let code = match chunk_type {
            RealtimeChunkType::Start => "S",
            RealtimeChunkType::Intermediate => "I",
            RealtimeChunkType::End => "E",
        };
        RealtimeChunkObject {
            object: S3Object {
                key: format!(
                    "KTLX/{volume_id}/{}-{chunk_id:03}-{code}",
                    volume_time.format("%Y%m%d-%H%M%S")
                ),
                size,
                last_modified: None,
            },
            site: "KTLX".to_owned(),
            volume_id,
            volume_time,
            chunk_id,
            chunk_type,
        }
    }

    fn test_group(volume_time: DateTime<Utc>, complete: bool) -> RealtimeLevel2Volume {
        let chunk_type = if complete {
            RealtimeChunkType::End
        } else {
            RealtimeChunkType::Intermediate
        };
        RealtimeLevel2Volume {
            site: "KTLX".to_owned(),
            volume_id: 679,
            volume_time,
            chunks: vec![test_chunk(679, volume_time, 1, chunk_type, 8)],
            complete,
            total_size: 8,
        }
    }

    fn test_realtime_volume_with_sizes(sizes: &[u64]) -> RealtimeLevel2Volume {
        let volume_time = Utc.with_ymd_and_hms(2026, 6, 8, 0, 0, 0).unwrap();
        let chunks = sizes
            .iter()
            .enumerate()
            .map(|(index, size)| {
                let chunk_id = u16::try_from(index + 1).expect("test chunk id");
                let chunk_type = if index == 0 {
                    RealtimeChunkType::Start
                } else if index + 1 == sizes.len() {
                    RealtimeChunkType::End
                } else {
                    RealtimeChunkType::Intermediate
                };
                RealtimeChunkObject {
                    object: S3Object {
                        key: format!("KTLX/1/20260608-000000-{chunk_id:03}-I"),
                        size: *size,
                        last_modified: None,
                    },
                    site: "KTLX".to_owned(),
                    volume_id: 1,
                    volume_time,
                    chunk_id,
                    chunk_type,
                }
            })
            .collect::<Vec<_>>();
        RealtimeLevel2Volume {
            site: "KTLX".to_owned(),
            volume_id: 1,
            volume_time,
            total_size: sizes.iter().sum(),
            complete: chunks.last().is_some_and(|chunk| chunk.chunk_type.is_end()),
            chunks,
        }
    }

    // --- feed staleness -----------------------------------------------------
    //
    // The values here are the two real feeds this was diagnosed against on
    // 2026-08-19, not invented times. KUEX had stopped: its chunk prefix held
    // one contiguous id run 1..=931 and the newest object anywhere under it was
    // `KUEX/931/20260816-110802-003-I`, LastModified 2026-08-16T11:08:09Z.
    // KOAX on the same machine at the same moment was publishing normally, with
    // `KOAX20260819_162446_RT680_V06` in the live cache.

    /// The volume time of the last thing KUEX ever published to the chunks
    /// bucket, read off the key `KUEX/931/20260816-110802-003-I`.
    fn kuex_last_volume_time() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 8, 16, 11, 8, 2).unwrap()
    }

    /// The volume time of the KOAX volume the same session had just fetched.
    fn koax_live_volume_time() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 8, 19, 16, 24, 46).unwrap()
    }

    /// Wall clock at the moment that KOAX volume landed in the live cache -
    /// the file's mtime, 09:27 local. Both feeds are judged at the same
    /// instant, which is the whole point: one had just published, the other had
    /// not published for three days.
    fn observed_now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 8, 19, 16, 27, 0).unwrap()
    }

    fn feed_volume(site: &str, volume_time: DateTime<Utc>) -> RealtimeLevel2Volume {
        RealtimeLevel2Volume {
            site: site.to_owned(),
            volume_id: 931,
            volume_time,
            chunks: Vec::new(),
            complete: false,
            total_size: 0,
        }
    }

    #[test]
    fn the_stalled_kuex_feed_and_the_live_koax_feed_classify_differently() {
        let now = observed_now();

        let kuex = feed_volume("KUEX", kuex_last_volume_time());
        assert_eq!(kuex.freshness_at(now), FeedFreshness::Stalled);
        // Three days and change, which is what the analyst was shown as live.
        assert_eq!(kuex.age_at(now).num_days(), 3);

        let koax = feed_volume("KOAX", koax_live_volume_time());
        assert_eq!(koax.freshness_at(now), FeedFreshness::Current);
        assert_eq!(koax.age_at(now).num_minutes(), 2);
    }

    /// The threshold has to sit above the slowest healthy VCP and far below a
    /// dead prefix. Both edges are pinned so a later tweak has to be deliberate.
    #[test]
    fn the_stall_threshold_clears_a_clear_air_vcp_and_catches_a_dead_prefix() {
        assert_eq!(REALTIME_FEED_STALL_AFTER_SECONDS, 900);

        // A clear-air VCP 31/32 volume takes about 10 minutes, and the age is
        // measured from its start, so a healthy site legitimately sits this far
        // behind. It must not be called stalled.
        assert_eq!(
            classify_feed_age(Duration::minutes(10)),
            FeedFreshness::Current
        );
        // Plus a couple of minutes of publication and listing latency.
        assert_eq!(
            classify_feed_age(Duration::minutes(12)),
            FeedFreshness::Current
        );

        // The edge itself, from both sides.
        assert_eq!(
            classify_feed_age(Duration::seconds(REALTIME_FEED_STALL_AFTER_SECONDS - 1)),
            FeedFreshness::Current
        );
        assert_eq!(
            classify_feed_age(Duration::seconds(REALTIME_FEED_STALL_AFTER_SECONDS)),
            FeedFreshness::Stalled
        );

        // And the case that started this.
        assert_eq!(classify_feed_age(Duration::days(3)), FeedFreshness::Stalled);
        assert!(classify_feed_age(Duration::days(3)).is_stalled());
    }

    /// A radar clock a little ahead of this machine's must not produce a
    /// negative age, which would read on a status line as a bug in the app.
    #[test]
    fn a_volume_time_ahead_of_wall_clock_ages_to_zero_rather_than_negative() {
        let now = observed_now();
        let ahead = now + Duration::seconds(4);
        assert_eq!(volume_age_at(ahead, now), Duration::zero());
        assert_eq!(
            classify_feed_age(volume_age_at(ahead, now)),
            FeedFreshness::Current
        );
    }

    // --- the bounded live cache (review §2.9) -------------------------------

    fn unique_cache_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "radar-workstation-live-cache-{label}-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("temp cache dir");
        dir
    }

    /// Seed every chunk file of `volume` into its chunk cache directory, so
    /// the download assembles entirely from disk and no test touches AWS.
    fn seed_chunks(cache_dir: &Path, volume: &RealtimeLevel2Volume, fill: u8) {
        let chunk_dir = realtime_chunk_cache_dir(cache_dir, volume);
        fs::create_dir_all(&chunk_dir).expect("chunk dir");
        for chunk in &volume.chunks {
            let filename = chunk
                .object
                .key
                .rsplit('/')
                .next()
                .expect("chunk keys carry a filename");
            let body = vec![fill; usize::try_from(chunk.object.size).expect("test size")];
            fs::write(chunk_dir.join(filename), body).expect("chunk file");
        }
    }

    #[test]
    fn assembling_a_complete_volume_discards_its_chunk_copies() {
        let cache_dir = unique_cache_dir("complete");
        let volume = test_realtime_volume_with_sizes(&[600, 400, 250]);
        assert!(volume.complete);
        seed_chunks(&cache_dir, &volume, 7);

        let downloaded =
            download_realtime_volume(&volume, &cache_dir).expect("assembles from seeded chunks");
        assert!(!downloaded.cache_hit);
        assert_eq!(downloaded.path.metadata().expect("assembled").len(), 1_250);
        assert!(
            !realtime_chunk_cache_dir(&cache_dir, &volume).exists(),
            "a complete volume's chunk copies were retained - the measured 540 MB growth term"
        );
        // The next request is a cache hit off the assembled file alone.
        assert!(
            download_realtime_volume(&volume, &cache_dir)
                .expect("cache hit")
                .cache_hit
        );
        let _ = fs::remove_dir_all(&cache_dir);
    }

    #[test]
    fn a_partial_volume_keeps_its_chunks_until_it_completes() {
        let cache_dir = unique_cache_dir("partial");
        let mut partial = test_realtime_volume_with_sizes(&[600, 400]);
        // Still assembling on the radar.
        partial.complete = false;
        seed_chunks(&cache_dir, &partial, 3);
        let downloaded = download_realtime_volume(&partial, &cache_dir).expect("assembles");
        assert_eq!(downloaded.path.metadata().expect("assembled").len(), 1_000);
        assert!(
            realtime_chunk_cache_dir(&cache_dir, &partial).exists(),
            "a growing volume needs its chunk copies for the next append"
        );

        // One more chunk completes it: the file is extended from the chunk
        // cache, and only then are the copies discarded.
        let complete = test_realtime_volume_with_sizes(&[600, 400, 250]);
        seed_chunks(&cache_dir, &complete, 3);
        let downloaded = download_realtime_volume(&complete, &cache_dir).expect("appends");
        assert_eq!(downloaded.path.metadata().expect("extended").len(), 1_250);
        assert!(!realtime_chunk_cache_dir(&cache_dir, &complete).exists());
        let _ = fs::remove_dir_all(&cache_dir);
    }

    #[test]
    fn the_prune_deletes_oldest_first_and_reports_the_bound() {
        let cache_dir = unique_cache_dir("prune-order");
        // Oldest to newest, with sleeps so the mtimes order on disk; the
        // retained chunk directory sits in the middle era and counts as one
        // unit.
        let names = [
            "KAAA_20260819_010000_001_V06",
            "KAAA_20260819_011000_002_V06",
            "KAAA_20260819_012000_003_V06",
        ];
        fs::write(cache_dir.join(names[0]), vec![0_u8; 1_000]).expect("volume file");
        std::thread::sleep(StdDuration::from_millis(60));
        let chunk_dir = cache_dir.join(".chunks").join("KAAA_20260819_010500_009");
        fs::create_dir_all(&chunk_dir).expect("chunk dir");
        fs::write(chunk_dir.join("chunk-001"), vec![0_u8; 500]).expect("chunk file");
        for name in &names[1..] {
            std::thread::sleep(StdDuration::from_millis(60));
            fs::write(cache_dir.join(name), vec![0_u8; 1_000]).expect("volume file");
        }

        // An hour from now every entry clears the age guard, so this tests
        // ordering and the bound alone. First a budget whose 90% target ONE
        // eviction satisfies: 3,500 bytes against 3,000 gives a 2,700 target,
        // so only the OLDEST unit - the first volume file - may go. The
        // directory listing yields the younger `.chunks` entry before any
        // volume file (`.` collates first), so a walk that skipped the
        // oldest-first sort would evict the chunk directory here instead and
        // still land under target: the survivor set, not just the byte
        // count, is what pins the order.
        let now_millis = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("the clock is after 1970")
            .as_millis() as u64
            + 3_600_000;
        let report = prune_live_cache_at(&cache_dir, 3_000, now_millis);
        assert_eq!(
            report,
            LiveCachePruneReport {
                entries_before: 4,
                bytes_before: 3_500,
                entries_removed: 1,
                bytes_after: 2_500,
            }
        );
        assert!(
            !cache_dir.join(names[0]).exists(),
            "the oldest volume must go first"
        );
        assert!(
            chunk_dir.exists(),
            "eviction was not oldest-first: the younger chunk directory went before the oldest file"
        );

        // Tighter: 2,500 bytes against 2,000 gives a 1,800 target, so the
        // chunk directory - now the oldest unit - and the middle volume go
        // and only the newest survives.
        let report = prune_live_cache_at(&cache_dir, 2_000, now_millis);
        assert_eq!(
            report,
            LiveCachePruneReport {
                entries_before: 3,
                bytes_before: 2_500,
                entries_removed: 2,
                bytes_after: 1_000,
            }
        );
        assert!(!chunk_dir.exists(), "a stale chunk directory is evictable");
        assert!(!cache_dir.join(names[1]).exists());
        assert!(cache_dir.join(names[2]).exists());
        let _ = fs::remove_dir_all(&cache_dir);
    }

    /// The prune against a COPY of a real live cache, so the numbers in the
    /// budget rationale stay measured rather than argued.
    ///
    /// Ignored because it needs a real cache on disk: copy one with
    /// timestamps preserved (`cp -rp`) and point `RADAR_LIVE_CACHE_COPY` at
    /// the copy - it deletes from whatever it is pointed at. Run with:
    ///
    /// ```text
    /// cargo test --release -p data_source -- --ignored --nocapture \
    ///     prunes_a_real_live_cache_copy
    /// ```
    #[test]
    #[ignore = "set RADAR_LIVE_CACHE_COPY to a disposable copy of a real live cache"]
    fn prunes_a_real_live_cache_copy() {
        let root = PathBuf::from(
            std::env::var("RADAR_LIVE_CACHE_COPY").expect("set RADAR_LIVE_CACHE_COPY"),
        );
        let before = live_cache_entries(&root);
        let bytes_before: u64 = before.iter().map(|entry| entry.bytes).sum();
        let newest = before
            .iter()
            .map(|entry| entry.newest_modified_unix_millis)
            .max()
            .expect("a populated cache");
        println!(
            "before: {} entries, {:.1} MiB",
            before.len(),
            bytes_before as f64 / (1024.0 * 1024.0)
        );

        // A deliberately small budget so the prune has real work to do.
        let budget = bytes_before / 4;
        let report = prune_live_cache(&root, budget);
        println!("{report:?}");
        assert_eq!(report.bytes_before, bytes_before);
        assert!(
            report.bytes_after <= budget,
            "still over budget: {} > {budget}",
            report.bytes_after
        );
        // The newest volume survives - it is what the analyst is looking at.
        let after = live_cache_entries(&root);
        assert!(
            after
                .iter()
                .any(|entry| entry.newest_modified_unix_millis == newest),
            "the prune deleted the newest volume"
        );
        let survivors_oldest = after
            .iter()
            .map(|entry| entry.newest_modified_unix_millis)
            .min()
            .expect("survivors");
        let victims_newest = before
            .iter()
            .filter(|entry| !after.iter().any(|kept| kept.path == entry.path))
            .map(|entry| entry.newest_modified_unix_millis)
            .max()
            .unwrap_or(0);
        assert!(
            victims_newest <= survivors_oldest,
            "eviction was not oldest-first: deleted {victims_newest} kept {survivors_oldest}"
        );
    }

    #[test]
    fn the_prune_never_touches_the_young_end_or_an_in_budget_cache() {
        let cache_dir = unique_cache_dir("prune-age");
        let name = "KAAA_20260819_010000_001_V06";
        fs::write(cache_dir.join(name), vec![0_u8; 4_000]).expect("volume file");

        // Over budget, but everything here was written moments ago - and the
        // youngest entries are the ones a writer may be mid-way through, so
        // the age guard holds even over budget.
        let report = prune_live_cache(&cache_dir, 1_000);
        assert_eq!(report.entries_removed, 0);
        assert!(cache_dir.join(name).exists());

        // Under budget the prune is a measured no-op.
        assert_eq!(
            prune_live_cache(&cache_dir, 100_000),
            LiveCachePruneReport {
                entries_before: 1,
                bytes_before: 4_000,
                entries_removed: 0,
                bytes_after: 4_000,
            }
        );
        let _ = fs::remove_dir_all(&cache_dir);
    }

    // --- the archive fallback -----------------------------------------------
    //
    // Every fixture below is a key that really existed in
    // `unidata-nexrad-level2` under `2026/08/19/KUEX/` when this was written,
    // taken from the listing rather than invented: 262 `_V06` volumes of
    // 6.1-16.7 MB and 18 `_V06_MDM` companions of ~720 KB.

    fn archive_object(key: &str, size: u64) -> S3Object {
        S3Object {
            key: key.to_owned(),
            size,
            last_modified: None,
        }
    }

    fn at(rfc3339: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(rfc3339)
            .expect("a fixed instant")
            .with_timezone(&Utc)
    }

    #[test]
    fn archive_keys_carry_their_volume_start_time() {
        assert_eq!(
            parse_level2_object_time_utc("2026/08/19/KUEX/KUEX20260819_184216_V06")
                .expect("a real key")
                .to_rfc3339_opts(SecondsFormat::Secs, true),
            "2026-08-19T18:42:16Z"
        );
        // The metadata companion parses too - it is rejected by suffix, not by
        // being unreadable, and a test that leaned on a parse failure would be
        // testing the wrong guard.
        assert_eq!(
            parse_level2_object_time_utc("2026/08/19/KUEX/KUEX20260819_005712_V06_MDM")
                .expect("a real MDM key")
                .to_rfc3339_opts(SecondsFormat::Secs, true),
            "2026-08-19T00:57:12Z"
        );
        // The historical compressed form, for the archive browser rather than
        // for this fallback.
        assert_eq!(
            parse_level2_object_time_utc("2011/04/27/KBMX/KBMX20110427_221510_V03.gz")
                .expect("a compressed key")
                .to_rfc3339_opts(SecondsFormat::Secs, true),
            "2011-04-27T22:15:10Z"
        );
        assert!(parse_level2_object_time_utc("bad-key").is_none());
        assert!(parse_level2_object_time_utc("2026/08/19/KUEX/").is_none());
    }

    /// The `_MDM` filter, against the exact trap it exists for: a metadata
    /// companion sorts AFTER the volume it belongs to, so for the seven
    /// minutes an hour when one is the last key written, "newest key wins"
    /// returns 720 KB of metadata instead of a volume.
    #[test]
    fn the_newest_archive_volume_is_never_a_metadata_companion() {
        let objects = vec![
            archive_object("2026/08/19/KUEX/KUEX20260819_182848_V06", 16_330_036),
            archive_object("2026/08/19/KUEX/KUEX20260819_183532_V06", 16_294_377),
            archive_object("2026/08/19/KUEX/KUEX20260819_184216_V06", 11_382_988),
            archive_object("2026/08/19/KUEX/KUEX20260819_184216_V06_MDM", 721_898),
            // A zero-byte key is the other thing a listing can hand back.
            archive_object("2026/08/19/KUEX/KUEX20260819_184900_V06", 0),
        ];
        let newest = newest_archive_volume("KUEX", objects, None).expect("a volume");
        assert_eq!(newest.key(), "2026/08/19/KUEX/KUEX20260819_184216_V06");
        assert_eq!(newest.total_size(), 11_382_988);
        assert_eq!(
            newest
                .volume_time
                .to_rfc3339_opts(SecondsFormat::Secs, true),
            "2026-08-19T18:42:16Z"
        );
        assert_eq!(newest.site, "KUEX");
    }

    /// The steady-state question a stalled session asks every 30 s: "anything
    /// newer than what I already have?"
    #[test]
    fn a_listing_with_nothing_newer_answers_none() {
        let objects = vec![
            archive_object("2026/08/19/KUEX/KUEX20260819_183532_V06", 16_294_377),
            archive_object("2026/08/19/KUEX/KUEX20260819_184216_V06", 11_382_988),
        ];
        let known = at("2026-08-19T18:42:16Z");
        assert!(newest_archive_volume("KUEX", objects.clone(), Some(known)).is_none());

        let older = at("2026-08-19T18:35:32Z");
        assert_eq!(
            newest_archive_volume("KUEX", objects, Some(older))
                .expect("the next volume")
                .key(),
            "2026/08/19/KUEX/KUEX20260819_184216_V06"
        );
    }

    /// THE UTC DAY BOUNDARY, which is where a day-parted bucket bites.
    ///
    /// At 00:04Z the newest volume in existence is usually under YESTERDAY's
    /// prefix, and the two-hour window that makes a routine listing cheap
    /// starts on yesterday's date - so the `start-after` key it would build
    /// carries the wrong date and would sort outside the prefix. `None` is the
    /// only correct answer there, and it is what sends the walk on to the
    /// whole-day and then the previous-day listing.
    #[test]
    fn the_listing_window_gives_up_at_the_day_boundary_instead_of_guessing() {
        // New year's morning, four minutes in: the day, the month and the year
        // all roll over between the window's start and `now`.
        let now = at("2026-01-01T00:04:00Z");
        let today = now.date_naive();
        let window_start = now - Duration::minutes(ARCHIVE_RECENT_WINDOW_MINUTES);
        assert_eq!(
            window_start.to_rfc3339_opts(SecondsFormat::Secs, true),
            "2025-12-31T22:04:00Z"
        );
        assert_eq!(archive_day_prefix("KUEX", today), "2026/01/01/KUEX/");
        assert_eq!(
            archive_day_prefix("KUEX", today - Duration::days(1)),
            "2025/12/31/KUEX/"
        );
        assert_eq!(
            archive_start_after_key("KUEX", today, window_start),
            None,
            "a window that starts on another day cannot be expressed here"
        );

        // Later the same day the window is expressible again, and this is the
        // ordering claim the whole cost argument rests on: the key sorts after
        // everything older than the window and before everything inside it.
        let now = at("2026-01-01T18:49:00Z");
        let window_start = now - Duration::minutes(ARCHIVE_RECENT_WINDOW_MINUTES);
        let start_after =
            archive_start_after_key("KUEX", today, window_start).expect("same-day window");
        assert_eq!(start_after, "2026/01/01/KUEX/KUEX20260101_164900");
        assert!(start_after.as_str() > "2026/01/01/KUEX/KUEX20260101_164859_V06");
        assert!(start_after.as_str() < "2026/01/01/KUEX/KUEX20260101_164901_V06");
        // And an `_MDM` inside the window still comes back from S3 - the
        // window is not the filter, `is_archive_volume_object` is.
        assert!(start_after.as_str() < "2026/01/01/KUEX/KUEX20260101_184216_V06_MDM");
    }

    /// The steady-state poll is ONE listing, and that is the number the
    /// fallback's cadence argument is built on: a stall lasting hours must not
    /// cost a day of keys every 30 s.
    #[test]
    fn a_warm_archive_poll_lists_exactly_one_prefix() {
        let plan = archive_listing_plan(
            "KUEX",
            at("2026-08-19T18:49:00Z"),
            Some(at("2026-08-19T18:42:16Z")),
        );
        assert_eq!(
            plan,
            vec![ArchiveListingStep {
                date: NaiveDate::from_ymd_opt(2026, 8, 19).expect("a real date"),
                start_after: Some("2026/08/19/KUEX/KUEX20260819_184216".to_owned()),
            }]
        );
    }

    /// A cold poll in the middle of the day widens at most twice, so the worst
    /// entry into the fallback is three listings and not a walk back through
    /// the archive.
    #[test]
    fn a_cold_archive_poll_widens_at_most_twice() {
        let plan = archive_listing_plan("KUEX", at("2026-08-19T18:49:00Z"), None);
        let today = NaiveDate::from_ymd_opt(2026, 8, 19).expect("a real date");
        assert_eq!(
            plan,
            vec![
                ArchiveListingStep {
                    date: today,
                    start_after: Some("2026/08/19/KUEX/KUEX20260819_164900".to_owned()),
                },
                ArchiveListingStep {
                    date: today,
                    start_after: None,
                },
                ArchiveListingStep {
                    date: NaiveDate::from_ymd_opt(2026, 8, 18).expect("a real date"),
                    start_after: None,
                },
            ]
        );
    }

    /// MIDNIGHT, COLD. Four minutes into a new day today's prefix holds
    /// nothing, and the two-hour window cannot be expressed against it, so the
    /// widening step collapses and yesterday is reached in two listings rather
    /// than three.
    #[test]
    fn a_cold_archive_poll_after_midnight_reaches_yesterday() {
        let plan = archive_listing_plan("KUEX", at("2026-01-01T00:04:00Z"), None);
        assert_eq!(
            plan,
            vec![
                ArchiveListingStep {
                    date: NaiveDate::from_ymd_opt(2026, 1, 1).expect("a real date"),
                    start_after: None,
                },
                ArchiveListingStep {
                    date: NaiveDate::from_ymd_opt(2025, 12, 31).expect("a real date"),
                    start_after: None,
                },
            ],
            "the year rolls over too"
        );
    }

    /// MIDNIGHT, WARM - the case a today-only poll loses data on.
    ///
    /// A volume that STARTS at 23:58Z is uploaded four to nine minutes later,
    /// which is tomorrow. A session holding yesterday's 23:52 volume and
    /// listing only today's prefix would never see it: today is empty for the
    /// first minutes of the day, and by the time today answers, yesterday is a
    /// prefix this walk never looks at again. So yesterday stays in the plan -
    /// with a `start-after`, so it costs an envelope and not a day - until the
    /// volume in hand is one of today's.
    #[test]
    fn a_warm_archive_poll_after_midnight_still_watches_yesterday() {
        let plan = archive_listing_plan(
            "KUEX",
            at("2026-01-01T00:04:00Z"),
            Some(at("2025-12-31T23:52:10Z")),
        );
        assert_eq!(
            plan,
            vec![
                ArchiveListingStep {
                    date: NaiveDate::from_ymd_opt(2026, 1, 1).expect("a real date"),
                    start_after: None,
                },
                ArchiveListingStep {
                    date: NaiveDate::from_ymd_opt(2025, 12, 31).expect("a real date"),
                    start_after: Some("2025/12/31/KUEX/KUEX20251231_235210".to_owned()),
                },
            ]
        );

        // And the moment a volume from the new day is in hand, yesterday drops
        // out again: one listing per poll for the rest of the day.
        let plan = archive_listing_plan(
            "KUEX",
            at("2026-01-01T00:12:00Z"),
            Some(at("2026-01-01T00:03:40Z")),
        );
        assert_eq!(plan.len(), 1);
        assert_eq!(
            plan[0].date,
            NaiveDate::from_ymd_opt(2026, 1, 1).expect("a real date")
        );
    }

    #[test]
    fn a_days_prefix_is_zero_padded() {
        assert_eq!(
            archive_day_prefix(
                "KOAX",
                NaiveDate::from_ymd_opt(2026, 8, 3).expect("a real date")
            ),
            "2026/08/03/KOAX/"
        );
    }

    #[test]
    fn cancellable_copy_stops_when_asked_and_copies_everything_when_not() {
        let source = vec![7_u8; CANCELLABLE_COPY_BLOCK_BYTES * 3 + 11];

        let mut sink = Vec::new();
        let copied = copy_cancellable(&mut source.as_slice(), &mut sink, &|| false)
            .expect("an uninterrupted copy");
        assert_eq!(copied, Some(source.len() as u64));
        assert_eq!(sink.len(), source.len());

        // Cancelled before the first read: nothing moves at all.
        let mut sink = Vec::new();
        assert_eq!(
            copy_cancellable(&mut source.as_slice(), &mut sink, &|| true).expect("a clean stop"),
            None
        );
        assert!(sink.is_empty());

        // Cancelled after the first block: the abandon is bounded by the block
        // size rather than by the length of the body.
        let mut sink = Vec::new();
        let blocks = std::cell::Cell::new(0_u32);
        assert_eq!(
            copy_cancellable(&mut source.as_slice(), &mut sink, &|| {
                let seen = blocks.get();
                blocks.set(seen + 1);
                seen > 0
            })
            .expect("a mid-body stop"),
            None
        );
        assert_eq!(sink.len(), CANCELLABLE_COPY_BLOCK_BYTES);
    }

    /// A cached archive volume costs no request, which is what keeps a
    /// re-entered fallback (or a session restart) from paying for the same
    /// 11 MB twice.
    #[test]
    fn a_cached_archive_volume_is_a_cache_hit_without_a_request() {
        let cache_dir = unique_cache_dir("archive-cache-hit");
        let volume = ArchiveLevel2Volume {
            site: "KUEX".to_owned(),
            object: archive_object("2026/08/19/KUEX/KUEX20260819_184216_V06", 4_096),
            volume_time: at("2026-08-19T18:42:16Z"),
        };
        fs::write(cache_dir.join("KUEX20260819_184216_V06"), vec![0_u8; 4_096])
            .expect("a cached volume file");

        let downloaded = download_archive_volume(&volume, &cache_dir).expect("cache hit");
        assert!(downloaded.cache_hit);
        assert_eq!(
            downloaded.path.file_name().and_then(|name| name.to_str()),
            Some("KUEX20260819_184216_V06"),
            "the archive file is named for its key, not for a realtime volume id"
        );
        assert_eq!(
            downloaded.url,
            "https://unidata-nexrad-level2.s3.amazonaws.com/2026/08/19/KUEX/KUEX20260819_184216_V06"
        );

        // A file of the WRONG size is not a cache hit: a half-written volume
        // decodes into garbage rather than failing.
        fs::write(cache_dir.join("KUEX20260819_184216_V06"), vec![0_u8; 4_000])
            .expect("a truncated volume file");
        let err = download_archive_volume_cancellable(&volume, &cache_dir, &|| true)
            .expect_err("a truncated cache entry must be re-fetched, and this fetch is cancelled");
        assert!(
            matches!(err, DataSourceError::ObjectDownloadCancelled { ref key }
                if key == "2026/08/19/KUEX/KUEX20260819_184216_V06"),
            "unexpected error: {err}"
        );

        let _ = fs::remove_dir_all(&cache_dir);
    }

    /// PROVE IT ON THE REAL BUCKET. Asks both sources for the same radars and
    /// prints what each says, so "the archive is current while the chunks feed
    /// is dead" is a measurement rather than a claim.
    ///
    /// Ignored because it needs the network. `RADAR_LIVE_SITES` overrides the
    /// list. Run it with:
    ///
    /// ```text
    /// cargo test --release -p data_source -- --ignored --nocapture \
    ///     the_archive_answers_for_a_radar_whose_chunk_feed_has_stopped
    /// ```
    #[test]
    #[ignore = "lists both real NEXRAD buckets"]
    fn the_archive_answers_for_a_radar_whose_chunk_feed_has_stopped() {
        let sites = std::env::var("RADAR_LIVE_SITES").unwrap_or_else(|_| "KUEX,KOAX".to_owned());
        for site in sites.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            let now = Utc::now();
            let chunks = latest_realtime_level2_volume(site);
            let archive = latest_archive_level2_volume(site);

            match &chunks {
                Ok(volume) => println!(
                    "{site} chunks   id {:>3} at {} · {:>4} s old · {:?}",
                    volume.volume_id,
                    volume
                        .volume_time
                        .to_rfc3339_opts(SecondsFormat::Secs, true),
                    volume.age_at(now).num_seconds(),
                    volume.freshness_at(now),
                ),
                Err(error) => println!("{site} chunks   unavailable: {error}"),
            }
            match &archive {
                Ok(volume) => println!(
                    "{site} archive  {} at {} · {:>4} s old · {:?} · {:.1} MiB · uploaded {}",
                    volume.key().rsplit('/').next().unwrap_or_default(),
                    volume
                        .volume_time
                        .to_rfc3339_opts(SecondsFormat::Secs, true),
                    volume.age_at(now).num_seconds(),
                    volume.freshness_at(now),
                    volume.total_size() as f64 / (1_024.0 * 1_024.0),
                    volume
                        .uploaded_at()
                        .map(|at| at.to_rfc3339_opts(SecondsFormat::Secs, true))
                        .unwrap_or_else(|| "unknown".to_owned()),
                ),
                Err(error) => println!("{site} archive  unavailable: {error}"),
            }

            let archive = archive.unwrap_or_else(|error| panic!("{site} archive: {error}"));
            assert!(
                archive.age_at(now).num_seconds() >= 0,
                "an archive volume cannot start in the future"
            );
            if let Ok(chunks) = chunks
                && chunks.freshness_at(now).is_stalled()
            {
                // NOT a strict `>`. A radar that is genuinely off the air
                // stops both pipes on the same scan, so the two times being
                // EQUAL is the correct, expected reading for a down site -
                // and the fallback declining to switch is the correct
                // response to it. What must never happen is the archive
                // sitting BEHIND the feed it is offered as a repair for.
                assert!(
                    archive.volume_time >= chunks.volume_time,
                    "{site}: the archive is behind the stalled chunk feed"
                );
                let lead = (archive.volume_time - chunks.volume_time).num_seconds();
                if lead > 0 {
                    println!("{site} FALLBACK EARNED: archive is {lead} s ahead of the chunk feed");
                } else {
                    println!(
                        "{site} RADAR IS DOWN: both pipes stopped on the same scan, \
                         so there is nothing to fall back TO"
                    );
                }
            }
            println!();
        }
    }

    /// THE DAY BOUNDARY, ON THE REAL BUCKET.
    ///
    /// At 00:03Z today's prefix is empty and the newest volume in existence is
    /// under yesterday's - a shape that cannot be waited for in a test, but
    /// can be borrowed: TOMORROW's prefix is empty for exactly the same reason
    /// that today's is empty just after midnight. So the walk is asked to find
    /// a volume as of tomorrow 00:03Z, and the only way it can answer is by
    /// reaching back a day, with a `start-after` key built from a volume it
    /// already holds.
    ///
    /// Ignored because it needs the network. Run it with:
    ///
    /// ```text
    /// cargo test --release -p data_source -- --ignored --nocapture \
    ///     the_day_boundary_walk_reaches_yesterday_on_the_real_bucket
    /// ```
    #[test]
    #[ignore = "lists the real Level II archive bucket across a day boundary"]
    fn the_day_boundary_walk_reaches_yesterday_on_the_real_bucket() {
        let site = std::env::var("RADAR_LIVE_SITE").unwrap_or_else(|_| "KUEX".to_owned());
        let newest = latest_archive_level2_volume(&site).expect("a volume today");
        // Something real and earlier the same day, so the `start-after` key is
        // built from a volume time the bucket actually contains.
        let known = newest.volume_time - Duration::minutes(30);

        let next_midnight = (newest.volume_time.date_naive() + Duration::days(1))
            .and_hms_opt(0, 3, 0)
            .expect("a real instant");
        let as_of = DateTime::<Utc>::from_naive_utc_and_offset(next_midnight, Utc);
        assert_eq!(
            archive_listing_plan(&site, as_of, Some(known)).len(),
            2,
            "the plan must list the empty new day AND the day the volume came from"
        );

        let found = archive_level2_volume_newer_than(&site, as_of, Some(known))
            .expect("the walk")
            .expect("a volume from the previous day");
        println!(
            "{site} as of {}: today's prefix is empty, walk found {}",
            as_of.to_rfc3339_opts(SecondsFormat::Secs, true),
            found.key()
        );
        assert_eq!(
            found.key(),
            newest.key(),
            "the walk must find the same newest volume the ordinary path finds"
        );
        assert!(found.volume_time > known);
    }

    /// THE `_MDM` TRAP, ON REAL KEYS.
    ///
    /// A metadata companion sorts immediately after the volume it belongs to,
    /// so for the minutes after one is written, the last key under the prefix
    /// is 720 KB of metadata rather than a 6-17 MB volume. This takes the real
    /// listing and cuts it at exactly that moment.
    #[test]
    #[ignore = "lists the real Level II archive bucket"]
    fn a_real_listing_cut_at_a_metadata_companion_still_answers_with_a_volume() {
        let site = std::env::var("RADAR_LIVE_SITE").unwrap_or_else(|_| "KUEX".to_owned());
        let today = Utc::now().date_naive();
        let objects = list_archive_day(&site, today, None).expect("today's prefix");
        let cut = objects
            .iter()
            .rposition(|object| object.key.ends_with(ARCHIVE_METADATA_KEY_SUFFIX))
            .unwrap_or_else(|| {
                panic!("{site}: no _MDM companion under today's prefix - nothing to trap")
            });
        let truncated = objects[..=cut].to_vec();
        let last = truncated.last().expect("the cut key").clone();
        println!(
            "{site} listing cut after {} ({} bytes)",
            last.key, last.size
        );
        assert!(last.key.ends_with(ARCHIVE_METADATA_KEY_SUFFIX));

        let newest = newest_archive_volume(&site, truncated, None).expect("a volume");
        println!("{site} newest volume chosen: {}", newest.key());
        assert!(
            !newest.key().ends_with(ARCHIVE_METADATA_KEY_SUFFIX),
            "the fallback would have downloaded a metadata blob as a radar volume"
        );
        assert_eq!(
            newest.volume_time,
            parse_level2_object_time_utc(&last.key).expect("the companion's own time"),
            "the volume the companion belongs to is the newest one, and it is what wins"
        );
    }

    /// CANCELLATION MID-TRANSFER, ON A REAL 10 MB OBJECT.
    ///
    /// The unit test for [`copy_cancellable`] proves the loop; this proves the
    /// contract end to end against the bucket: an analyst who switches radar
    /// during an archive fetch stops paying for it within one 64 KiB block,
    /// and nothing half-written is left where a later poll could mistake it
    /// for a cached volume.
    #[test]
    #[ignore = "starts a real archive download and abandons it"]
    fn a_real_archive_download_stops_when_the_session_ends() {
        let site = std::env::var("RADAR_LIVE_SITE").unwrap_or_else(|_| "KUEX".to_owned());
        let volume = latest_archive_level2_volume(&site).expect("a volume today");
        let cache_dir = unique_cache_dir("archive-cancel");
        let filename = archive_volume_cache_filename(&volume);

        let blocks = std::sync::atomic::AtomicUsize::new(0);
        let started = std::time::Instant::now();
        let error = download_archive_volume_cancellable(&volume, &cache_dir, &|| {
            blocks.fetch_add(1, std::sync::atomic::Ordering::Relaxed) >= 3
        })
        .expect_err("the transfer must be abandoned");
        let elapsed = started.elapsed();

        println!(
            "{site} abandoned {} ({:.1} MiB) after {} blocks in {:.0} ms: {error}",
            volume.key(),
            volume.total_size() as f64 / (1_024.0 * 1_024.0),
            blocks.load(std::sync::atomic::Ordering::Relaxed),
            elapsed.as_secs_f64() * 1_000.0,
        );
        assert!(
            matches!(error, DataSourceError::ObjectDownloadCancelled { .. }),
            "unexpected error: {error}"
        );
        assert!(
            blocks.load(std::sync::atomic::Ordering::Relaxed) >= 4,
            "the copy has to have actually started moving the body"
        );
        assert!(
            !cache_dir.join(&filename).exists(),
            "a cancelled transfer must not leave a volume file"
        );
        assert!(
            !cache_dir
                .join(&filename)
                .with_extension("download")
                .exists(),
            "nor a partial one for the next attempt to trip over"
        );

        let _ = fs::remove_dir_all(&cache_dir);
    }
}

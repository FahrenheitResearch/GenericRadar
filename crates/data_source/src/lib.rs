//! Public radar data-source helpers.

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
const REALTIME_CHUNK_DOWNLOAD_BATCH: usize = 8;
/// Total attempts per S3 object. See [`download_s3_object_to_path`].
const S3_OBJECT_DOWNLOAD_ATTEMPTS: usize = 3;
const S3_OBJECT_DOWNLOAD_RETRY_DELAY: StdDuration = StdDuration::from_millis(150);
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

    for batch in missing.chunks(REALTIME_CHUNK_DOWNLOAD_BATCH) {
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
    let url = format!("https://{bucket}.s3.amazonaws.com/");
    let client = metadata_http_client();
    let mut query = vec![("list-type", "2".to_owned()), ("prefix", prefix.to_owned())];
    if let Some(delimiter) = delimiter {
        query.push(("delimiter", delimiter.to_owned()));
    }
    if let Some(token) = continuation_token {
        query.push(("continuation-token", token.to_owned()));
    }
    if let Some(max_keys) = max_keys {
        query.push(("max-keys", max_keys.to_string()));
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
    let url = format!("https://{bucket}.s3.amazonaws.com/{}", object.key);
    for attempt in 1..=S3_OBJECT_DOWNLOAD_ATTEMPTS {
        match download_s3_object_attempt(&url, object, path) {
            Ok(()) => return Ok(()),
            Err(error) => {
                if attempt == S3_OBJECT_DOWNLOAD_ATTEMPTS || !is_retriable_download_error(&error) {
                    return Err(error);
                }
                eprintln!("retrying {url} after attempt {attempt}: {error}");
                thread::sleep(S3_OBJECT_DOWNLOAD_RETRY_DELAY);
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

fn download_s3_object_attempt(url: &str, object: &S3Object, path: &Path) -> Result<()> {
    let url = url.to_owned();
    let mut response = download_http_client()
        .get(&url)
        .send()?
        .error_for_status()?;
    let temp_path = path.with_extension("download");
    let mut temp_file = fs::File::create(&temp_path)?;
    let copied = io::copy(&mut response, &mut temp_file)?;
    drop(temp_file);
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
}

impl From<S3ListingXml> for S3Listing {
    fn from(value: S3ListingXml) -> Self {
        Self {
            contents: value.contents.into_iter().map(Into::into).collect(),
            common_prefixes: value.common_prefixes.into_iter().map(Into::into).collect(),
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
    use chrono::TimeZone;

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
        let dir = std::env::temp_dir().join(format!(
            "genericradar-cache-test-{}",
            std::process::id()
        ));
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
}

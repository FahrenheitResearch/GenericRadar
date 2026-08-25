//! Honest, time-aware surface observations from public operational providers.
//!
//! The Aviation Weather Center publishes a compact, complete global METAR
//! cache. Iowa Environmental Mesonet supplies historical ASOS reports and
//! independent state road-weather / data-collection networks. All fetching
//! here is blocking and belongs on a background worker, never a UI thread.

use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::sync::Mutex;

use chrono::{DateTime, Datelike, NaiveDateTime, Timelike, Utc};

const AWC_METAR_CACHE_URL: &str = "https://aviationweather.gov/data/cache/metars.cache.csv.gz";
const AWC_METAR_API_URL: &str = "https://aviationweather.gov/api/data/metar";
const IEM_ASOS_URL: &str = "https://mesonet.agron.iastate.edu/cgi-bin/request/asos.py";
const IEM_CURRENTS_URL: &str = "https://mesonet.agron.iastate.edu/api/1/currents.json";
const HISTORY_RETENTION_HOURS: i64 = 12;
const MAX_FRAME_OB_AGE_MINUTES: i64 = 90;
const CONUS_STATES: [&str; 49] = [
    "AL", "AZ", "AR", "CA", "CO", "CT", "DE", "FL", "GA", "ID", "IL", "IN", "IA", "KS", "KY", "LA",
    "ME", "MD", "MA", "MI", "MN", "MS", "MO", "MT", "NE", "NV", "NH", "NJ", "NM", "NY", "NC", "ND",
    "OH", "OK", "OR", "PA", "RI", "SC", "SD", "TN", "TX", "UT", "VT", "VA", "WA", "WV", "WI", "WY",
    "DC",
];

/// Sky-cover categories in increasing opacity, with obscured distinct from
/// overcast: visibility into the cloud column is itself useful information.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub enum SkyCover {
    Clear,
    Few,
    Scattered,
    Broken,
    Overcast,
    Obscured,
}

impl SkyCover {
    pub fn oktas(self) -> u8 {
        match self {
            Self::Clear => 0,
            Self::Few => 2,
            Self::Scattered => 4,
            Self::Broken => 7,
            Self::Overcast | Self::Obscured => 8,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Clear => "Clear",
            Self::Few => "Few",
            Self::Scattered => "Scattered",
            Self::Broken => "Broken",
            Self::Overcast => "Overcast",
            Self::Obscured => "Obscured",
        }
    }

    fn from_code(value: &str) -> Option<Self> {
        let value = value.trim().to_ascii_uppercase();
        if matches!(value.as_str(), "CLR" | "SKC" | "NSC" | "NCD" | "CAVOK") {
            Some(Self::Clear)
        } else if value.starts_with("FEW") {
            Some(Self::Few)
        } else if value.starts_with("SCT") {
            Some(Self::Scattered)
        } else if value.starts_with("BKN") {
            Some(Self::Broken)
        } else if value.starts_with("OVC") {
            Some(Self::Overcast)
        } else if value.starts_with("OVX") || value.starts_with("VV") {
            Some(Self::Obscured)
        } else {
            None
        }
    }

    fn is_ceiling(self) -> bool {
        matches!(self, Self::Broken | Self::Overcast | Self::Obscured)
    }
}

/// One reported station observation. Missing measurements remain `None`;
/// they are never inferred from another variable or silently zero-filled.
#[derive(Clone, Debug, PartialEq)]
pub struct Observation {
    pub station_id: String,
    pub time_utc: DateTime<Utc>,
    pub lat: f32,
    pub lon: f32,
    pub temp_c: Option<f32>,
    pub dewpoint_c: Option<f32>,
    pub wind_dir_deg: Option<f32>,
    pub wind_speed_kt: Option<f32>,
    pub wind_gust_kt: Option<f32>,
    pub altim_in_hg: Option<f32>,
    pub mslp_hpa: Option<f32>,
    pub precip_1h_in: Option<f32>,
    pub visibility_sm: Option<f32>,
    pub ceiling_ft_agl: Option<f32>,
    pub sky_cover: Option<SkyCover>,
    pub present_weather: Option<String>,
    pub raw_metar: Option<String>,
    pub network: String,
    pub elevation_m: Option<f32>,
    /// Station-model channels reported, used for deterministic decluttering.
    pub completeness: u8,
}

impl Observation {
    fn measure_completeness(&mut self) {
        self.completeness = self.temp_c.is_some() as u8
            + self.dewpoint_c.is_some() as u8
            + self.wind_speed_kt.is_some() as u8
            + self.wind_dir_deg.is_some() as u8
            + self.altim_in_hg.is_some() as u8
            + self.sky_cover.is_some() as u8
            + self.present_weather.is_some() as u8;
    }
}

/// Fetch every currently reporting worldwide METAR station in one compact
/// gzipped request, typically a few hundred kilobytes over the wire.
pub fn fetch_current_observations() -> Result<Vec<Observation>, String> {
    let response = crate::download_http_client()
        .get(AWC_METAR_CACHE_URL)
        .send()
        .and_then(reqwest::blocking::Response::error_for_status)
        .map_err(|error| format!("Aviation Weather Center METAR request failed: {error}"))?;
    let bytes = response
        .bytes()
        .map_err(|error| format!("Aviation Weather Center METAR body failed: {error}"))?;
    let mut text = String::new();
    flate2::read::GzDecoder::new(bytes.as_ref())
        .read_to_string(&mut text)
        .map_err(|error| format!("Aviation Weather Center METAR gzip decode failed: {error}"))?;
    let observations = parse_metar_cache(&text)?;
    if observations.is_empty() {
        return Err("Aviation Weather Center returned no usable station observations".to_owned());
    }
    Ok(observations)
}

/// Fetch actual historical observations for one station. IEM supplies a
/// richer archive; recent requests fall back to AWC's station-history API.
pub fn fetch_station_history(station: &str, hours: u8) -> Result<Vec<Observation>, String> {
    let station = normalize_station_id(station)?;
    let hours = hours.clamp(1, 72);
    match fetch_iem_station_history(&station, hours, None) {
        Ok(observations) if !observations.is_empty() => Ok(observations),
        Ok(_) if hours > 24 => Err(format!(
            "Iowa Environmental Mesonet returned no reports for the requested {hours} hours; \
             the Aviation Weather Center fallback only supports 24 hours"
        )),
        Ok(_) => fetch_awc_station_history(&station, hours),
        Err(iem_error) if hours > 24 => Err(format!(
            "{iem_error}; the Aviation Weather Center fallback only supports 24 hours, \
             not the requested {hours} hours"
        )),
        Err(iem_error) => fetch_awc_station_history(&station, hours)
            .map_err(|awc_error| format!("{iem_error}; AWC fallback: {awc_error}")),
    }
}

/// Fetch a station's history ending at a historical radar-frame time.
pub fn fetch_station_history_at(
    station: &str,
    when: DateTime<Utc>,
    hours: u8,
) -> Result<Vec<Observation>, String> {
    let station = normalize_station_id(station)?;
    fetch_iem_station_history(&station, hours.clamp(1, 72), Some(when))
}

fn fetch_iem_station_history(
    station: &str,
    hours: u8,
    ending_at: Option<DateTime<Utc>>,
) -> Result<Vec<Observation>, String> {
    let window = if let Some(end) = ending_at {
        let start = end - chrono::Duration::hours(i64::from(hours));
        format!(
            "year1={}&month1={}&day1={}&hour1={}&minute1={}\
             &year2={}&month2={}&day2={}&hour2={}&minute2={}",
            start.year(),
            start.month(),
            start.day(),
            start.hour(),
            start.minute(),
            end.year(),
            end.month(),
            end.day(),
            end.hour(),
            end.minute(),
        )
    } else {
        format!("hours={hours}")
    };
    let url = format!(
        "{IEM_ASOS_URL}?station={station}\
         &data=tmpf&data=dwpf&data=drct&data=sknt&data=gust&data=alti\
         &data=mslp&data=p01i&data=vsby&data=wxcodes\
         &data=skyc1&data=skyc2&data=skyc3&data=skyc4\
         &data=skyl1&data=skyl2&data=skyl3&data=skyl4&data=metar\
         &{window}&tz=Etc%2FUTC&format=onlycomma&latlon=yes&elev=yes\
         &missing=empty&trace=0.0001&report_type=3&report_type=4"
    );
    let text = http_text(&url, "Iowa Environmental Mesonet station history")?;
    let mut observations = parse_iem_asos_csv(&text)?;
    observations.sort_unstable_by_key(|observation| observation.time_utc);
    Ok(observations)
}

fn fetch_awc_station_history(station: &str, hours: u8) -> Result<Vec<Observation>, String> {
    let hours = hours.clamp(1, 24);
    let url = format!("{AWC_METAR_API_URL}?ids={station}&format=json&hours={hours}");
    let text = http_text(&url, "Aviation Weather Center station history")?;
    let value = serde_json::from_str::<serde_json::Value>(&text)
        .map_err(|error| format!("Aviation Weather Center history JSON invalid: {error}"))?;
    let rows = value
        .as_array()
        .ok_or_else(|| "Aviation Weather Center history did not contain observations".to_owned())?;
    let mut observations: Vec<_> = rows.iter().filter_map(parse_awc_json_observation).collect();
    observations.sort_unstable_by_key(|observation| observation.time_utc);
    if observations.is_empty() {
        return Err(format!("no historical observations found for {station}"));
    }
    Ok(observations)
}

/// Fetch every CONUS state RWIS and DCP mesonet network. This is explicitly
/// opt-in: 98 bounded requests are distributed across eight worker threads.
pub fn fetch_mesonet_observations() -> Vec<Observation> {
    let networks: Vec<_> = CONUS_STATES
        .iter()
        .flat_map(|state| [format!("{state}_RWIS"), format!("{state}_DCP")])
        .collect();
    fetch_mesonet_networks(&networks)
}

/// Fetch caller-selected state mesonet networks with bounded concurrency.
pub fn fetch_mesonet_networks(networks: &[String]) -> Vec<Observation> {
    let networks: Vec<_> = networks
        .iter()
        .filter(|network| {
            !network.is_empty()
                && network.len() <= 20
                && network
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
        })
        .collect();
    let queue = Mutex::new(networks.into_iter());
    let results = Mutex::new(Vec::new());
    std::thread::scope(|scope| {
        for _ in 0..8 {
            scope.spawn(|| {
                loop {
                    let Some(network) = queue.lock().ok().and_then(|mut queue| queue.next()) else {
                        break;
                    };
                    if let Ok(observations) = fetch_mesonet_network(network)
                        && let Ok(mut results) = results.lock()
                    {
                        results.extend(observations);
                    }
                }
            });
        }
    });
    results.into_inner().unwrap_or_default()
}

fn fetch_mesonet_network(network: &str) -> Result<Vec<Observation>, String> {
    let url = format!("{IEM_CURRENTS_URL}?network={network}");
    let text = http_text(&url, "Iowa Environmental Mesonet current observations")?;
    let value = serde_json::from_str::<serde_json::Value>(&text)
        .map_err(|error| format!("IEM network {network} JSON invalid: {error}"))?;
    Ok(parse_iem_currents(&value, network))
}

fn http_text(url: &str, provider: &str) -> Result<String, String> {
    crate::download_http_client()
        .get(url)
        .send()
        .and_then(reqwest::blocking::Response::error_for_status)
        .map_err(|error| format!("{provider} request failed: {error}"))?
        .text()
        .map_err(|error| format!("{provider} response failed: {error}"))
}

fn normalize_station_id(station: &str) -> Result<String, String> {
    let station = station.trim().to_ascii_uppercase();
    if !(3..=8).contains(&station.len())
        || !station.bytes().all(|byte| byte.is_ascii_alphanumeric())
    {
        return Err(format!("invalid station identifier: {station:?}"));
    }
    Ok(station)
}

fn parse_metar_cache(text: &str) -> Result<Vec<Observation>, String> {
    let mut lines = text.lines();
    let header = lines
        .find(|line| line.starts_with("raw_text,"))
        .ok_or_else(|| "Aviation Weather Center METAR cache header not found".to_owned())?;
    let columns: Vec<_> = header.split(',').collect();
    Ok(lines
        .filter_map(|line| parse_metar_cache_row(&columns, line))
        .collect())
}

fn parse_metar_cache_row(columns: &[&str], line: &str) -> Option<Observation> {
    let fields = split_csv(line);
    let get = |name| csv_field(columns, &fields, name);
    let numeric = |name| get(name).and_then(parse_finite);
    let lat = numeric("latitude")?;
    let lon = numeric("longitude")?;
    if !valid_coordinates(lat, lon) || (lat + 99.99).abs() < 0.001 {
        return None;
    }
    let station_id = normalize_station_id(get("station_id")?).ok()?;
    let time_utc = get("observation_time")
        .and_then(|value| DateTime::parse_from_rfc3339(value).ok())?
        .with_timezone(&Utc);
    let raw_metar = get("raw_text").map(str::to_owned);
    let mut observation = Observation {
        station_id,
        time_utc,
        lat,
        lon,
        temp_c: numeric("temp_c").filter(|value| (-90.0..=60.0).contains(value)),
        dewpoint_c: numeric("dewpoint_c").filter(|value| (-100.0..=45.0).contains(value)),
        wind_dir_deg: numeric("wind_dir_degrees").filter(|value| (0.0..=360.0).contains(value)),
        wind_speed_kt: numeric("wind_speed_kt").filter(|value| (0.0..250.0).contains(value)),
        wind_gust_kt: numeric("wind_gust_kt").filter(|value| (0.0..250.0).contains(value)),
        altim_in_hg: numeric("altim_in_hg").filter(|value| (25.0..=33.0).contains(value)),
        mslp_hpa: numeric("sea_level_pressure_mb").filter(|value| (800.0..=1100.0).contains(value)),
        precip_1h_in: get("precip_in")
            .and_then(parse_precip)
            .filter(|value| (0.0..=30.0).contains(value)),
        visibility_sm: get("visibility_statute_mi")
            .and_then(parse_visibility)
            .filter(|value| (0.0..=200.0).contains(value)),
        ceiling_ft_agl: metar_cache_ceiling(columns, &fields),
        sky_cover: metar_cache_sky_cover(columns, &fields)
            .or_else(|| raw_metar.as_deref().and_then(sky_cover_from_raw)),
        present_weather: get("wx_string")
            .map(str::to_owned)
            .or_else(|| raw_metar.as_deref().and_then(weather_from_raw)),
        raw_metar,
        network: "METAR".to_owned(),
        elevation_m: numeric("elevation_m").filter(|value| (-430.0..=9000.0).contains(value)),
        completeness: 0,
    };
    observation.measure_completeness();
    (observation.completeness > 0).then_some(observation)
}

fn parse_iem_asos_csv(text: &str) -> Result<Vec<Observation>, String> {
    let mut lines = text.lines().filter(|line| !line.trim().is_empty());
    let header = lines
        .find(|line| line.starts_with("station,"))
        .ok_or_else(|| "Iowa Environmental Mesonet ASOS CSV header not found".to_owned())?;
    let columns: Vec<_> = header.split(',').collect();
    Ok(lines
        .filter_map(|line| parse_iem_asos_row(&columns, line))
        .collect())
}

fn parse_iem_asos_row(columns: &[&str], line: &str) -> Option<Observation> {
    let fields = split_csv(line);
    let get = |name| csv_field(columns, &fields, name);
    let numeric = |name| get(name).and_then(parse_finite);
    let lat = numeric("lat")?;
    let lon = numeric("lon")?;
    if !valid_coordinates(lat, lon) {
        return None;
    }
    let time_utc = get("valid").and_then(parse_iem_timestamp)?;
    let raw_metar = get("metar").map(str::to_owned);
    let station_id = station_id_from_raw(raw_metar.as_deref(), get("station")?);
    let mut observation = Observation {
        station_id,
        time_utc,
        lat,
        lon,
        temp_c: numeric("tmpf")
            .map(fahrenheit_to_celsius)
            .filter(|value| (-90.0..=60.0).contains(value)),
        dewpoint_c: numeric("dwpf")
            .map(fahrenheit_to_celsius)
            .filter(|value| (-100.0..=45.0).contains(value)),
        wind_dir_deg: numeric("drct").filter(|value| (0.0..=360.0).contains(value)),
        wind_speed_kt: numeric("sknt").filter(|value| (0.0..250.0).contains(value)),
        wind_gust_kt: numeric("gust").filter(|value| (0.0..250.0).contains(value)),
        altim_in_hg: numeric("alti").filter(|value| (25.0..=33.0).contains(value)),
        mslp_hpa: numeric("mslp").filter(|value| (800.0..=1100.0).contains(value)),
        precip_1h_in: get("p01i")
            .and_then(parse_precip)
            .filter(|value| (0.0..=30.0).contains(value)),
        visibility_sm: get("vsby")
            .and_then(parse_visibility)
            .filter(|value| (0.0..=200.0).contains(value)),
        ceiling_ft_agl: iem_ceiling(columns, &fields),
        sky_cover: iem_sky_cover(columns, &fields)
            .or_else(|| raw_metar.as_deref().and_then(sky_cover_from_raw)),
        present_weather: get("wxcodes")
            .map(str::to_owned)
            .or_else(|| raw_metar.as_deref().and_then(weather_from_raw)),
        raw_metar,
        network: "METAR".to_owned(),
        elevation_m: numeric("elevation").filter(|value| (-430.0..=9000.0).contains(value)),
        completeness: 0,
    };
    observation.measure_completeness();
    (observation.completeness > 0).then_some(observation)
}

fn parse_awc_json_observation(row: &serde_json::Value) -> Option<Observation> {
    let string = |name| row.get(name).and_then(serde_json::Value::as_str);
    let number = |name| {
        row.get(name)
            .and_then(serde_json::Value::as_f64)
            .map(|value| value as f32)
            .filter(|value| value.is_finite())
    };
    let lat = number("lat")?;
    let lon = number("lon")?;
    if !valid_coordinates(lat, lon) {
        return None;
    }
    let time_utc = string("reportTime")
        .or_else(|| string("receiptTime"))
        .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
        .map(|time| time.with_timezone(&Utc))
        .or_else(|| {
            row.get("obsTime")
                .and_then(serde_json::Value::as_i64)
                .and_then(DateTime::<Utc>::from_timestamp_secs)
        })?;
    let raw_metar = string("rawOb").map(str::to_owned);
    let sky_cover = row
        .get("clouds")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|cloud| cloud.get("cover").and_then(serde_json::Value::as_str))
        .filter_map(SkyCover::from_code)
        .max()
        .or_else(|| raw_metar.as_deref().and_then(sky_cover_from_raw));
    let ceiling_ft_agl = row
        .get("clouds")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|cloud| {
            let cover = SkyCover::from_code(cloud.get("cover")?.as_str()?)?;
            cover
                .is_ceiling()
                .then(|| cloud.get("base")?.as_f64().map(|value| value as f32))?
        })
        .filter(|value| (0.0..=50_000.0).contains(value))
        .min_by(f32::total_cmp);
    let mut observation = Observation {
        station_id: normalize_station_id(string("icaoId")?).ok()?,
        time_utc,
        lat,
        lon,
        temp_c: number("temp").filter(|value| (-90.0..=60.0).contains(value)),
        dewpoint_c: number("dewp").filter(|value| (-100.0..=45.0).contains(value)),
        wind_dir_deg: number("wdir").filter(|value| (0.0..=360.0).contains(value)),
        wind_speed_kt: number("wspd").filter(|value| (0.0..250.0).contains(value)),
        wind_gust_kt: number("wgst").filter(|value| (0.0..250.0).contains(value)),
        altim_in_hg: number("altim")
            .map(|value| {
                if value > 100.0 {
                    value / 33.863_89
                } else {
                    value
                }
            })
            .filter(|value| (25.0..=33.0).contains(value)),
        mslp_hpa: number("slp").filter(|value| (800.0..=1100.0).contains(value)),
        precip_1h_in: number("precip").filter(|value| (0.0..=30.0).contains(value)),
        visibility_sm: number("visib")
            .or_else(|| string("visib").and_then(parse_visibility))
            .filter(|value| (0.0..=200.0).contains(value)),
        ceiling_ft_agl,
        sky_cover,
        present_weather: string("wxString")
            .map(str::to_owned)
            .or_else(|| raw_metar.as_deref().and_then(weather_from_raw)),
        raw_metar,
        network: "METAR".to_owned(),
        elevation_m: number("elev").filter(|value| (-430.0..=9000.0).contains(value)),
        completeness: 0,
    };
    observation.measure_completeness();
    (observation.completeness > 0).then_some(observation)
}

fn parse_iem_currents(value: &serde_json::Value, network: &str) -> Vec<Observation> {
    let Some(rows) = value.get("data").and_then(serde_json::Value::as_array) else {
        return Vec::new();
    };
    rows.iter()
        .filter_map(|row| parse_iem_current_observation(row, network))
        .collect()
}

fn parse_iem_current_observation(row: &serde_json::Value, network: &str) -> Option<Observation> {
    let numeric = |name| {
        row.get(name)
            .and_then(serde_json::Value::as_f64)
            .map(|value| value as f32)
            .filter(|value| value.is_finite())
    };
    let lat = numeric("lat")?;
    let lon = numeric("lon")?;
    if !valid_coordinates(lat, lon) {
        return None;
    }
    let station_id = row
        .get("station")
        .and_then(serde_json::Value::as_str)
        .and_then(|station| normalize_station_id(station).ok())?;
    let time_utc = row
        .get("utc_valid")
        .and_then(serde_json::Value::as_str)
        .and_then(parse_iem_timestamp)?;
    let mut sky_cover = None;
    let mut ceiling_ft_agl = None;
    for layer in 1..=4 {
        let Some(cover) = row
            .get(format!("skyc{layer}"))
            .and_then(serde_json::Value::as_str)
            .and_then(SkyCover::from_code)
        else {
            continue;
        };
        sky_cover = Some(sky_cover.map_or(cover, |previous: SkyCover| previous.max(cover)));
        if cover.is_ceiling()
            && let Some(base) = row
                .get(format!("skyl{layer}"))
                .and_then(serde_json::Value::as_f64)
                .map(|value| value as f32)
                .filter(|value| value.is_finite() && (0.0..=50_000.0).contains(value))
        {
            ceiling_ft_agl = Some(ceiling_ft_agl.map_or(base, |previous: f32| previous.min(base)));
        }
    }
    let mut observation = Observation {
        station_id,
        time_utc,
        lat,
        lon,
        temp_c: numeric("tmpf")
            .map(fahrenheit_to_celsius)
            .filter(|value| (-90.0..=60.0).contains(value)),
        dewpoint_c: numeric("dwpf")
            .map(fahrenheit_to_celsius)
            .filter(|value| (-100.0..=45.0).contains(value)),
        wind_dir_deg: numeric("drct").filter(|value| (0.0..=360.0).contains(value)),
        wind_speed_kt: numeric("sknt").filter(|value| (0.0..250.0).contains(value)),
        wind_gust_kt: numeric("gust").filter(|value| (0.0..250.0).contains(value)),
        altim_in_hg: numeric("alti").filter(|value| (25.0..=33.0).contains(value)),
        mslp_hpa: numeric("mslp").filter(|value| (800.0..=1100.0).contains(value)),
        precip_1h_in: numeric("p01i").filter(|value| (0.0..=30.0).contains(value)),
        visibility_sm: numeric("vsby").filter(|value| (0.0..=200.0).contains(value)),
        ceiling_ft_agl,
        sky_cover,
        present_weather: row
            .get("wxcodes")
            .and_then(serde_json::Value::as_str)
            .filter(|value| !value.trim().is_empty())
            .map(str::to_owned),
        raw_metar: None,
        network: network.to_owned(),
        elevation_m: numeric("elevation").filter(|value| (-430.0..=9000.0).contains(value)),
        completeness: 0,
    };
    observation.measure_completeness();
    (observation.completeness > 0).then_some(observation)
}

fn split_csv(line: &str) -> Vec<&str> {
    let mut fields = Vec::with_capacity(48);
    let mut start = 0;
    let mut in_quotes = false;
    for (index, byte) in line.bytes().enumerate() {
        match byte {
            b'"' => in_quotes = !in_quotes,
            b',' if !in_quotes => {
                fields.push(line[start..index].trim_matches('"'));
                start = index + 1;
            }
            _ => {}
        }
    }
    fields.push(line[start..].trim_matches('"'));
    fields
}

fn csv_field<'a>(columns: &[&str], fields: &[&'a str], name: &str) -> Option<&'a str> {
    let index = columns.iter().position(|column| *column == name)?;
    fields
        .get(index)
        .copied()
        .map(str::trim)
        .filter(|value| !value.is_empty() && *value != "M")
}

fn parse_finite(value: &str) -> Option<f32> {
    value.parse::<f32>().ok().filter(|value| value.is_finite())
}

fn valid_coordinates(lat: f32, lon: f32) -> bool {
    lat.is_finite()
        && lon.is_finite()
        && (-90.0..=90.0).contains(&lat)
        && (-180.0..=180.0).contains(&lon)
}

fn fahrenheit_to_celsius(value: f32) -> f32 {
    (value - 32.0) * (5.0 / 9.0)
}

fn parse_precip(value: &str) -> Option<f32> {
    if value.trim().eq_ignore_ascii_case("T") {
        Some(0.0001)
    } else {
        parse_finite(value.trim())
    }
}

fn parse_visibility(value: &str) -> Option<f32> {
    parse_finite(value.trim().trim_end_matches('+'))
}

fn parse_iem_timestamp(value: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|time| time.with_timezone(&Utc))
        .or_else(|| {
            let value = value.trim_end_matches('Z');
            ["%Y-%m-%d %H:%M:%S", "%Y-%m-%d %H:%M", "%Y-%m-%dT%H:%M:%S"]
                .iter()
                .find_map(|format| NaiveDateTime::parse_from_str(value, format).ok())
                .map(|time| DateTime::<Utc>::from_naive_utc_and_offset(time, Utc))
        })
}

fn station_id_from_raw(raw: Option<&str>, fallback: &str) -> String {
    let mut tokens = raw.unwrap_or_default().split_whitespace();
    let mut candidate = tokens.next().unwrap_or(fallback);
    if matches!(candidate, "METAR" | "SPECI" | "COR") {
        candidate = tokens.next().unwrap_or(fallback);
    }
    if candidate == "COR" {
        candidate = tokens.next().unwrap_or(fallback);
    }
    normalize_station_id(candidate).unwrap_or_else(|_| fallback.trim().to_ascii_uppercase())
}

fn metar_cache_sky_cover(columns: &[&str], fields: &[&str]) -> Option<SkyCover> {
    columns
        .iter()
        .enumerate()
        .filter(|(_, column)| **column == "sky_cover")
        .filter_map(|(index, _)| fields.get(index))
        .filter_map(|cover| SkyCover::from_code(cover))
        .max()
}

fn metar_cache_ceiling(columns: &[&str], fields: &[&str]) -> Option<f32> {
    columns
        .iter()
        .enumerate()
        .filter(|(_, column)| **column == "sky_cover")
        .filter_map(|(index, _)| {
            let cover = SkyCover::from_code(fields.get(index)?)?;
            if !cover.is_ceiling() {
                return None;
            }
            columns
                .get(index + 1)
                .filter(|column| **column == "cloud_base_ft_agl")
                .and_then(|_| fields.get(index + 1))
                .and_then(|value| parse_finite(value))
                .or_else(|| csv_field(columns, fields, "vert_vis_ft").and_then(parse_finite))
        })
        .filter(|value| (0.0..=50_000.0).contains(value))
        .min_by(f32::total_cmp)
}

fn iem_sky_cover(columns: &[&str], fields: &[&str]) -> Option<SkyCover> {
    (1..=4)
        .filter_map(|layer| csv_field(columns, fields, &format!("skyc{layer}")))
        .filter_map(SkyCover::from_code)
        .max()
}

fn iem_ceiling(columns: &[&str], fields: &[&str]) -> Option<f32> {
    (1..=4)
        .filter_map(|layer| {
            let cover = SkyCover::from_code(csv_field(columns, fields, &format!("skyc{layer}"))?)?;
            if !cover.is_ceiling() {
                return None;
            }
            csv_field(columns, fields, &format!("skyl{layer}")).and_then(parse_finite)
        })
        .filter(|value| (0.0..=50_000.0).contains(value))
        .min_by(f32::total_cmp)
}

fn sky_cover_from_raw(raw: &str) -> Option<SkyCover> {
    raw.split_whitespace().filter_map(SkyCover::from_code).max()
}

fn weather_from_raw(raw: &str) -> Option<String> {
    let mut weather = Vec::new();
    for token in raw.split_whitespace() {
        let value = token.trim_start_matches(['+', '-']);
        if value.len() < 2
            || value.len() > 8
            || !value.bytes().all(|byte| byte.is_ascii_uppercase())
        {
            continue;
        }
        let has_phenomenon = [
            "DZ", "RA", "SN", "SG", "IC", "PL", "GR", "GS", "UP", "BR", "FG", "FU", "VA", "DU",
            "SA", "HZ", "PY", "SQ", "FC", "SS", "DS", "TS",
        ]
        .iter()
        .any(|phenomenon| value.ends_with(phenomenon));
        if has_phenomenon && !matches!(value, "NOSIG" | "SPECI" | "METAR") {
            weather.push(token);
            if weather.len() == 2 {
                break;
            }
        }
    }
    (!weather.is_empty()).then(|| weather.join(" "))
}

/// Time-sorted observations keyed by station. Radar loops select the latest
/// report valid *at* the frame time, so future observations never leak into a
/// historical radar frame.
#[derive(Default)]
pub struct ObservationPool {
    by_station: HashMap<String, Vec<Observation>>,
}

impl ObservationPool {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn merge(&mut self, fetched: Vec<Observation>) {
        if fetched.is_empty() {
            return;
        }
        let newest = fetched
            .iter()
            .map(|observation| observation.time_utc)
            .max()
            .unwrap_or_else(Utc::now);
        let cutoff = newest - chrono::Duration::hours(HISTORY_RETENTION_HOURS);
        let mut touched = HashSet::new();
        for observation in fetched {
            if observation.time_utc < cutoff {
                continue;
            }
            let station_id = observation.station_id.clone();
            let series = self.by_station.entry(station_id.clone()).or_default();
            match series.binary_search_by_key(&observation.time_utc, |entry| entry.time_utc) {
                Ok(index) if observation.completeness > series[index].completeness => {
                    series[index] = observation;
                }
                Ok(_) => {}
                Err(index) => series.insert(index, observation),
            }
            touched.insert(station_id);
        }
        for station_id in touched {
            if let Some(series) = self.by_station.get_mut(&station_id) {
                let keep_from = series.partition_point(|observation| observation.time_utc < cutoff);
                series.drain(..keep_from);
            }
        }
        self.by_station.retain(|_, series| !series.is_empty());
    }

    pub fn station_count(&self) -> usize {
        self.by_station.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_station.is_empty()
    }

    pub fn observation_at(&self, station: &str, frame_time: DateTime<Utc>) -> Option<&Observation> {
        let series = self.by_station.get(station)?;
        let next = series.partition_point(|observation| observation.time_utc <= frame_time);
        let observation = series.get(next.checked_sub(1)?)?;
        (frame_time - observation.time_utc <= chrono::Duration::minutes(MAX_FRAME_OB_AGE_MINUTES))
            .then_some(observation)
    }

    pub fn frame_observations(
        &self,
        frame_time: DateTime<Utc>,
    ) -> impl Iterator<Item = &Observation> {
        self.by_station
            .keys()
            .filter_map(move |station| self.observation_at(station, frame_time))
    }

    pub fn station_series(
        &self,
        station: &str,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
    ) -> Vec<&Observation> {
        let Some(series) = self.by_station.get(station) else {
            return Vec::new();
        };
        let first = series.partition_point(|observation| observation.time_utc < start);
        let last = series.partition_point(|observation| observation.time_utc <= end);
        series[first..last].iter().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    const AWC_HEADER: &str = "raw_text,station_id,observation_time,latitude,longitude,temp_c,dewpoint_c,wind_dir_degrees,wind_speed_kt,wind_gust_kt,visibility_statute_mi,altim_in_hg,sea_level_pressure_mb,wx_string,sky_cover,cloud_base_ft_agl,sky_cover,cloud_base_ft_agl,precip_in,vert_vis_ft,elevation_m";

    fn awc_fixture() -> String {
        format!(
            "ignored preamble\n{AWC_HEADER}\n\"KOUN 251745Z 04012G18KT 10SM -TSRA SCT020 BKN035, RMK TEST\",KOUN,2026-08-25T17:45:00Z,35.2435,-97.4708,25,17,40,12,18,10+,29.82,1010.2,-TSRA,SCT,2000,BKN,3500,T,,357\n"
        )
    }

    #[test]
    fn metar_cache_decodes_cloud_layers_weather_and_trace_without_fabrication() {
        let observations = parse_metar_cache(&awc_fixture()).expect("valid cache");
        assert_eq!(observations.len(), 1);
        let observation = &observations[0];
        assert_eq!(observation.station_id, "KOUN");
        assert_eq!(observation.sky_cover, Some(SkyCover::Broken));
        assert_eq!(observation.ceiling_ft_agl, Some(3500.0));
        assert_eq!(observation.present_weather.as_deref(), Some("-TSRA"));
        assert_eq!(observation.visibility_sm, Some(10.0));
        assert_eq!(observation.precip_1h_in, Some(0.0001));
        assert!(
            observation
                .raw_metar
                .as_ref()
                .is_some_and(|raw| raw.contains(", RMK"))
        );
    }

    #[test]
    fn missing_timestamps_and_impossible_coordinates_are_refused() {
        let invalid = awc_fixture().replace("2026-08-25T17:45:00Z", "");
        assert!(
            parse_metar_cache(&invalid)
                .expect("valid header")
                .is_empty()
        );
        let invalid = awc_fixture().replace("35.2435", "-99.99");
        assert!(
            parse_metar_cache(&invalid)
                .expect("valid header")
                .is_empty()
        );
    }

    #[test]
    fn iem_history_restores_icao_id_converts_units_and_decodes_sky() {
        let csv = "station,valid,lon,lat,elevation,tmpf,dwpf,drct,sknt,gust,alti,mslp,p01i,vsby,wxcodes,skyc1,skyc2,skyc3,skyc4,skyl1,skyl2,skyl3,skyl4,metar\nOUN,2026-08-25 17:45,-97.4708,35.2435,357,77,62.6,40,12,18,29.82,1010.2,T,10,-RA,SCT,BKN,,,2000,3500,,,KOUN 251745Z 04012G18KT 10SM -RA SCT020 BKN035\n";
        let observations = parse_iem_asos_csv(csv).expect("IEM history");
        let observation = &observations[0];
        assert_eq!(observation.station_id, "KOUN");
        assert!((observation.temp_c.unwrap() - 25.0).abs() < 0.01);
        assert!((observation.dewpoint_c.unwrap() - 17.0).abs() < 0.01);
        assert_eq!(observation.sky_cover, Some(SkyCover::Broken));
        assert_eq!(observation.ceiling_ft_agl, Some(3500.0));
        assert_eq!(observation.present_weather.as_deref(), Some("-RA"));
        assert_eq!(observation.precip_1h_in, Some(0.0001));
    }

    #[test]
    fn mesonet_json_requires_real_timestamp_and_preserves_provider_network() {
        let value = serde_json::json!({"data":[{"station":"OKCD","utc_valid":"2026-08-25T17:45:00Z","lat":35.2,"lon":-97.4,"tmpf":77.0,"dwpf":62.6,"drct":40.0,"sknt":12.0,"skyc1":"OVC","skyl1":900.0},{"station":"BAD1","lat":35.2,"lon":-97.4,"tmpf":77.0}]});
        let observations = parse_iem_currents(&value, "OK_DCP");
        assert_eq!(observations.len(), 1);
        assert_eq!(observations[0].network, "OK_DCP");
        assert_eq!(observations[0].sky_cover, Some(SkyCover::Overcast));
        assert_eq!(observations[0].ceiling_ft_agl, Some(900.0));
    }

    #[test]
    fn awc_json_fallback_decodes_current_api_shapes() {
        let row = serde_json::json!({"icaoId":"KDEN","reportTime":"2026-08-25T17:45:00.000Z","lat":39.85,"lon":-104.65,"temp":25.0,"dewp":8.0,"wdir":270,"wspd":12,"altim":1013.25,"visib":"10+","rawOb":"KDEN 251745Z 27012KT 10SM BKN035","clouds":[{"cover":"BKN","base":3500}]});
        let observation = parse_awc_json_observation(&row).expect("AWC station report");
        assert_eq!(observation.sky_cover, Some(SkyCover::Broken));
        assert_eq!(observation.ceiling_ft_agl, Some(3500.0));
        assert!((observation.altim_in_hg.unwrap() - 29.92).abs() < 0.02);
    }

    #[test]
    fn pool_selects_only_reports_known_at_frame_and_rejects_stale_reports() {
        let mut pool = ObservationPool::new();
        let mut observation = parse_metar_cache(&awc_fixture()).unwrap().remove(0);
        let earlier = observation.time_utc;
        observation.temp_c = Some(26.0);
        observation.time_utc += chrono::Duration::minutes(30);
        pool.merge(vec![
            parse_metar_cache(&awc_fixture()).unwrap().remove(0),
            observation,
        ]);
        assert_eq!(pool.station_count(), 1);
        assert_eq!(
            pool.observation_at("KOUN", earlier).unwrap().temp_c,
            Some(25.0)
        );
        assert_eq!(
            pool.observation_at("KOUN", earlier + chrono::Duration::minutes(31))
                .unwrap()
                .temp_c,
            Some(26.0)
        );
        assert!(
            pool.observation_at("KOUN", earlier + chrono::Duration::hours(3))
                .is_none()
        );
        assert_eq!(
            pool.station_series("KOUN", earlier, earlier + chrono::Duration::hours(1))
                .len(),
            2
        );
    }

    #[test]
    fn duplicate_reports_keep_richer_measurements_and_reject_url_injection() {
        let mut pool = ObservationPool::new();
        let sparse = parse_metar_cache(&awc_fixture()).unwrap().remove(0);
        let mut rich = sparse.clone();
        rich.wind_gust_kt = Some(28.0);
        rich.completeness += 1;
        pool.merge(vec![sparse.clone(), rich]);
        pool.merge(vec![sparse]);
        assert_eq!(
            pool.observation_at(
                "KOUN",
                Utc.with_ymd_and_hms(2026, 8, 25, 17, 50, 0).unwrap()
            )
            .unwrap()
            .wind_gust_kt,
            Some(28.0)
        );
        assert!(normalize_station_id("KOUN&hours=999").is_err());
    }

    #[test]
    fn raw_weather_and_cloud_parsers_do_not_guess_clear_weather() {
        assert_eq!(
            sky_cover_from_raw("KOUN 251745Z 10SM CLR"),
            Some(SkyCover::Clear)
        );
        assert_eq!(sky_cover_from_raw("KOUN 251745Z 10SM"), None);
        assert_eq!(
            weather_from_raw("KOUN 251745Z 10SM -TSRA BR BKN035"),
            Some("-TSRA BR".into())
        );
        assert_eq!(weather_from_raw("KOUN 251745Z 10SM CLR"), None);
    }

    #[test]
    #[ignore = "requires live public Aviation Weather Center access"]
    fn live_global_metar_cache_contains_real_reports() {
        let observations = fetch_current_observations().expect("live AWC METAR cache");
        println!("live global METAR observations: {}", observations.len());
        assert!(observations.len() > 1000);
        assert!(
            observations
                .iter()
                .any(|observation| observation.sky_cover.is_some())
        );
    }

    #[test]
    #[ignore = "requires live public IEM/AWC station history access"]
    fn live_station_history_contains_actual_timestamps() {
        let observations = fetch_station_history("KDEN", 6).expect("live Denver station history");
        println!(
            "live KDEN historical station reports: {}",
            observations.len()
        );
        assert!(!observations.is_empty());
        assert!(
            observations
                .iter()
                .all(|observation| observation.station_id == "KDEN")
        );
    }
}

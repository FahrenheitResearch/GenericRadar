//! Radar site directory.
//!
//! Fetches the public station list once per run and caches it on disk, so the
//! site markers are available offline after the first successful fetch. The
//! fetch happens on its own thread; the egui thread only drains the result.

use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver};
use std::thread;

use data_source::RadarSite;
use eframe::egui;

/// A site with a known position, which is all the overlay needs.
#[derive(Clone, Debug, PartialEq)]
pub struct LocatedSite {
    pub id: String,
    pub name: Option<String>,
    pub latitude_deg: f64,
    pub longitude_deg: f64,
}

pub struct SitesService {
    receiver: Receiver<Vec<LocatedSite>>,
}

impl SitesService {
    /// Start the lookup. Cached sites are delivered immediately if present;
    /// a refreshed list follows when the network answers.
    pub fn new(context: egui::Context) -> Self {
        let (sender, receiver) = mpsc::channel();
        let _worker = thread::Builder::new()
            .name("radar-site-directory".to_owned())
            .spawn(move || {
                let cache = cache_path();
                if let Some(sites) = read_cache(cache.as_deref())
                    && !sites.is_empty()
                    && sender.send(sites).is_ok()
                {
                    context.request_repaint();
                }
                match data_source::fetch_weather_gov_radar_sites() {
                    Ok(fetched) => {
                        let sites = located(fetched);
                        if sites.is_empty() {
                            return;
                        }
                        write_cache(cache.as_deref(), &sites);
                        if sender.send(sites).is_ok() {
                            context.request_repaint();
                        }
                    }
                    Err(error) => {
                        // Offline is normal and not worth interrupting the
                        // analyst over; the cache, if any, already went out.
                        eprintln!("radar site directory unavailable: {error}");
                    }
                }
            })
            .expect("failed to start radar site directory worker");
        Self { receiver }
    }

    pub fn try_recv(&self) -> Option<Vec<LocatedSite>> {
        self.receiver.try_recv().ok()
    }
}

fn located(sites: Vec<RadarSite>) -> Vec<LocatedSite> {
    sites
        .into_iter()
        .filter_map(|site| {
            Some(LocatedSite {
                id: site.level2_id,
                name: site.name,
                latitude_deg: f64::from(site.latitude_deg?),
                longitude_deg: f64::from(site.longitude_deg?),
            })
        })
        .collect()
}

fn cache_path() -> Option<PathBuf> {
    let base = if let Some(path) = std::env::var_os("LOCALAPPDATA") {
        PathBuf::from(path)
            .join("FahrenheitResearch")
            .join("RadarWorkstation")
            .join("cache")
    } else {
        PathBuf::from(std::env::var_os("XDG_CACHE_HOME")?).join("radar-workstation")
    };
    Some(base.join("radar-sites.tsv"))
}

/// Tab-separated, one site per line. A hand-readable format keeps a third-party
/// serialisation dependency out of the workstation for four fields.
fn read_cache(path: Option<&std::path::Path>) -> Option<Vec<LocatedSite>> {
    let text = std::fs::read_to_string(path?).ok()?;
    Some(parse_cache(&text))
}

fn parse_cache(text: &str) -> Vec<LocatedSite> {
    text.lines()
        .filter_map(|line| {
            let mut fields = line.split('\t');
            let id = fields.next()?.trim();
            if id.is_empty() {
                return None;
            }
            let latitude_deg = fields.next()?.trim().parse().ok()?;
            let longitude_deg = fields.next()?.trim().parse().ok()?;
            let name = fields.next().map(str::trim).filter(|name| !name.is_empty());
            Some(LocatedSite {
                id: id.to_owned(),
                name: name.map(str::to_owned),
                latitude_deg,
                longitude_deg,
            })
        })
        .collect()
}

fn write_cache(path: Option<&std::path::Path>, sites: &[LocatedSite]) {
    let Some(path) = path else {
        return;
    };
    let mut text = String::with_capacity(sites.len() * 40);
    for site in sites {
        text.push_str(&format!(
            "{}\t{}\t{}\t{}\n",
            site.id,
            site.latitude_deg,
            site.longitude_deg,
            site.name.as_deref().unwrap_or_default()
        ));
    }
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    // Write beside the target and rename, so a crash cannot leave a half file.
    let temporary = path.with_extension("tsv.partial");
    if std::fs::write(&temporary, text).is_ok() {
        let _ = std::fs::rename(&temporary, path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_cache_round_trips_through_its_text_form() {
        let sites = vec![
            LocatedSite {
                id: "KTLX".to_owned(),
                name: Some("Oklahoma City".to_owned()),
                latitude_deg: 35.3333,
                longitude_deg: -97.2778,
            },
            LocatedSite {
                id: "KRTX".to_owned(),
                name: None,
                latitude_deg: 45.715,
                longitude_deg: -122.965,
            },
        ];
        let mut text = String::new();
        for site in &sites {
            text.push_str(&format!(
                "{}\t{}\t{}\t{}\n",
                site.id,
                site.latitude_deg,
                site.longitude_deg,
                site.name.as_deref().unwrap_or_default()
            ));
        }
        assert_eq!(parse_cache(&text), sites);
    }

    #[test]
    fn malformed_cache_lines_are_skipped_not_fatal() {
        let text = "KTLX\t35.3333\t-97.2778\tOklahoma City\n\
                    broken line\n\
                    \t1\t2\tno id\n\
                    KRTX\tnot-a-number\t-122.965\t\n\
                    KAKQ\t36.984\t-77.0074\t\n";
        let sites = parse_cache(text);
        assert_eq!(sites.len(), 2);
        assert_eq!(sites[0].id, "KTLX");
        assert_eq!(sites[1].id, "KAKQ");
        assert_eq!(sites[1].name, None);
    }

    #[test]
    fn sites_without_coordinates_are_dropped() {
        let sites = located(vec![
            RadarSite::new("KTLX").with_location(None, Some(35.3333), Some(-97.2778)),
            RadarSite::new("NOPE"),
        ]);
        assert_eq!(sites.len(), 1);
        assert_eq!(sites[0].id, "KTLX");
    }
}

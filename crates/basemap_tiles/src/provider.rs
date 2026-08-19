//! The provider table: URL templates, attribution obligations, and the policy
//! flags that follow from each provider's terms.
//!
//! Attribution here is a **correctness requirement, not paperwork**. Every
//! string in [`TileProvider::attribution`] is drawn on screen by the scene
//! layer with no toggle to turn it off, and every claim below was checked
//! against the provider's own machine-readable metadata or published policy
//! rather than copied from another application.
//!
//! # Why Esri / ArcGIS Online is not in this table
//!
//! The sibling application this feature was modelled on fetches
//! `server.arcgisonline.com/.../World_Imagery`. That endpoint answers without
//! a token, which is not the same thing as a licence. The ArcGIS Online item
//! record for World Imagery states that the layer is governed by the Esri
//! Master License Agreement — an agreement between Esri and a licensed ArcGIS
//! customer, which a third-party desktop application is not a party to — and
//! it carries an explicit export restriction: "This layer is not intended to
//! be used to export tiles for offline." A persistent on-disk tile cache is
//! exactly that. This design cannot both comply with those terms and meet its
//! own requirement to cache tiles on disk and rate-limit through that cache,
//! so the provider is not shipped. USGS Imagery is the same picture over the
//! United States at higher resolution and is a U.S. Government work.

use crate::{MAX_TILE_ZOOM, TileId};

/// A raster tile source, with the policy that governs its use.
///
/// # Coverage is per *tile*, not per region
///
/// Measured against the live services on 2026-08-18, not assumed: the USGS
/// services answer 404 for individual tiles in a way that is **not monotonic
/// in zoom**. `USGSShadedReliefOnly`, probed at z5 through z16 over four
/// NEXRAD sites, answers 200 at every zoom over PAKC, but over KTLX it is
/// missing z9 and z14-z16; over KRTX it is missing z9, z10, z11 and z14-z16;
/// over TJUA it is missing z14-z16. A cold z9 view of that layer over
/// Oklahoma City therefore 404s on *every single tile in the pane*, which was
/// observed directly.
///
/// Coverage is also regional at fine zooms: `USGSImageryOnly` answers 404 for
/// Paris at z12 and 200 for its z8 ancestor, so outside the United States the
/// imagery is a coarse picture rather than none.
///
/// Consequently the ancestor-texture fallback
/// ([`TileId::uv_offset_scale_within`]) is mandatory rather than an
/// optimisation: without it the basemap is a checkerboard of holes. 404 is
/// recorded as [`crate::TileState::Absent`], which is permanent for the
/// session and is deliberately a different state from a transient failure, so
/// the layer does not re-probe the same holes on every frame forever.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum TileProvider {
    /// USGS The National Map — orthoimagery only.
    UsgsImagery,
    /// USGS The National Map — orthoimagery with roads and place names burned
    /// in. The most legible single choice under a reflectivity colour table.
    UsgsImageryTopo,
    /// USGS The National Map — the US Topo product: roads, place names,
    /// hydrography, boundaries. The "streets" equivalent, with none of the
    /// usage constraints that come with a community tile server.
    UsgsTopo,
    /// USGS The National Map — shaded relief only. The quiet option: terrain
    /// shape with no colour and no labels competing with the colour table,
    /// which for orographic and beam-blockage work beats an aerial photograph.
    UsgsShadedRelief,
    /// The OpenStreetMap Standard tile layer. The only global street map here
    /// with terms legible enough to ship against, and the answer wherever the
    /// USGS layers are blank.
    OpenStreetMap,
}

/// Highest zoom the USGS National Map basemap caches publish (24 LODs, z0-z23,
/// read from each service's own `tileInfo`).
const USGS_SERVICE_MAX_ZOOM: u8 = 23;

/// Highest zoom the OpenStreetMap standard layer publishes.
const OSM_SERVICE_MAX_ZOOM: u8 = 19;

/// `Cache-Control: max-age` the USGS services actually send, measured.
const USGS_MAX_AGE_SECONDS: u64 = 86_400;

/// The OpenStreetMap Foundation tile usage policy's fallback expectation for
/// clients that do not read cache headers: keep a tile at least seven days.
const OSM_MIN_CACHE_SECONDS: u64 = 7 * 86_400;

impl TileProvider {
    /// Every shipped provider, in picker order.
    pub const ALL: [TileProvider; 5] = [
        Self::UsgsImageryTopo,
        Self::UsgsImagery,
        Self::UsgsTopo,
        Self::UsgsShadedRelief,
        Self::OpenStreetMap,
    ];

    /// Stable on-disk cache directory name.
    ///
    /// Never change one of these in place: the string is a directory that
    /// already holds a user's cached tiles, and reusing a key for different
    /// imagery serves the old pictures under the new name.
    #[must_use]
    pub fn key(self) -> &'static str {
        match self {
            Self::UsgsImagery => "usgs-imagery",
            Self::UsgsImageryTopo => "usgs-imagery-topo",
            Self::UsgsTopo => "usgs-topo",
            Self::UsgsShadedRelief => "usgs-shaded-relief",
            Self::OpenStreetMap => "osm-standard",
        }
    }

    /// Menu text.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::UsgsImagery => "USGS imagery",
            Self::UsgsImageryTopo => "USGS imagery + topo",
            Self::UsgsTopo => "USGS topo",
            Self::UsgsShadedRelief => "USGS shaded relief",
            Self::OpenStreetMap => "OpenStreetMap",
        }
    }

    /// The credit drawn on the map. Displaying it is a condition of use, so
    /// there is no switch for it anywhere in the application.
    ///
    /// These are the on-screen forms. [`Self::attribution_full`] carries each
    /// service's complete `copyrightText` verbatim, for a tooltip or an about
    /// box, because one of them runs to nearly five hundred characters and
    /// would not be a credit so much as a wall.
    #[must_use]
    pub fn attribution(self) -> &'static str {
        match self {
            Self::UsgsImagery => "USDA, USGS The National Map: Orthoimagery",
            Self::UsgsImageryTopo => "USGS The National Map: Orthoimagery and US Topo",
            Self::UsgsTopo => "USGS The National Map: US Topo",
            Self::UsgsShadedRelief => "USGS The National Map: 3DEP, EROS GMTED2010",
            Self::OpenStreetMap => "\u{a9} OpenStreetMap contributors",
        }
    }

    /// The provider's complete credit string, verbatim.
    ///
    /// The four USGS strings are each service's own `copyrightText`, read from
    /// `<service>/MapServer?f=json`. The OpenStreetMap string is the licence
    /// attribution the OSMF tile usage policy requires.
    ///
    /// "Verbatim" is checked rather than asserted: the ignored live test
    /// `every_usgs_credit_matches_the_service_that_publishes_it` compares each
    /// of these against the running service, character for character. It was
    /// added because one of them had silently drifted — the US Topo string was
    /// missing its trailing data-refresh sentence, so the application was
    /// displaying an abridged version of a credit whose whole point is to be
    /// the provider's own words. A failure there means the service has updated
    /// its credit and this table has to follow, not that the test is wrong.
    #[must_use]
    pub fn attribution_full(self) -> &'static str {
        match self {
            Self::UsgsImagery => {
                "USDA, USGS The National Map: Orthoimagery. Data refreshed June, 2024."
            }
            Self::UsgsImageryTopo => {
                "USGS The National Map: Orthoimagery and US Topo. \
                 Data refreshed February, 2025."
            }
            Self::UsgsTopo => {
                "USGS The National Map: National Boundaries Dataset, 3DEP Elevation \
                 Program, Geographic Names Information System, National Hydrography \
                 Dataset, National Land Cover Database, National Structures Dataset, \
                 and National Transportation Dataset; USGS Global Ecosystems; \
                 U.S. Census Bureau TIGER/Line data; USFS Road data; Natural Earth \
                 Data; U.S. Department of State HIU; NOAA National Centers for \
                 Environmental Information. Data refreshed October 27, 2025-v2.1"
            }
            Self::UsgsShadedRelief => {
                "USGS The National Map: 3D Elevation Program. USGS Earth Resources \
                 Observation & Science (EROS) Center: GMTED2010. \
                 Data refreshed April, 2025."
            }
            Self::OpenStreetMap => {
                "\u{a9} OpenStreetMap contributors, available under the Open Database \
                 License (ODbL)."
            }
        }
    }

    /// Where the credit points. The OpenStreetMap one is the URL the tile
    /// usage policy names; the USGS ones point at the programme that publishes
    /// the service.
    #[must_use]
    pub fn attribution_url(self) -> &'static str {
        match self {
            Self::UsgsImagery | Self::UsgsImageryTopo | Self::UsgsTopo | Self::UsgsShadedRelief => {
                "https://www.usgs.gov/programs/national-geospatial-program"
            }
            Self::OpenStreetMap => "https://www.openstreetmap.org/copyright",
        }
    }

    /// One line on why a provider might be blank where the user is looking.
    /// Worth surfacing in the picker: "the imagery layer is empty" reads as a
    /// bug otherwise.
    #[must_use]
    pub fn coverage_note(self) -> &'static str {
        match self {
            Self::UsgsImagery | Self::UsgsImageryTopo => {
                "United States and territories. Outside the U.S. there is no imagery \
                 above about zoom 8 and the layer falls back to a coarse parent tile."
            }
            Self::UsgsTopo => {
                "United States and territories. Coverage is per tile, so isolated \
                 zoom levels can be missing even inside the U.S."
            }
            Self::UsgsShadedRelief => {
                "Global, but with real per-site holes in the zoom stack that the \
                 coarser-tile fallback covers. Measured over four NEXRAD sites: \
                 KTLX is missing zoom 9 and 14-16, KRTX is missing 9-11 and 14-16, \
                 TJUA is missing 14-16, and PAKC has every zoom."
            }
            Self::OpenStreetMap => "Global.",
        }
    }

    /// The tile URL.
    ///
    /// Note the two different component orders. The ArcGIS REST tile endpoint
    /// is `/tile/{level}/{row}/{col}`, i.e. **z/y/x**; the OpenStreetMap
    /// standard layer is **z/x/y**. Transposing either one produces a map that
    /// looks like a map and is in the wrong place, which is the worst kind of
    /// wrong.
    #[must_use]
    pub fn tile_url(self, tile: TileId) -> String {
        match self {
            Self::UsgsImagery | Self::UsgsImageryTopo | Self::UsgsTopo | Self::UsgsShadedRelief => {
                format!(
                    "https://basemap.nationalmap.gov/arcgis/rest/services/{}/MapServer/tile/{}/{}/{}",
                    self.arcgis_service(),
                    tile.z,
                    tile.y,
                    tile.x
                )
            }
            // Exactly this host, over HTTPS, with no subdomain sharding: the
            // OSMF policy names the URL and asks that clients not spread load
            // across the legacy a/b/c aliases.
            Self::OpenStreetMap => {
                format!(
                    "https://tile.openstreetmap.org/{}/{}/{}.png",
                    tile.z, tile.x, tile.y
                )
            }
        }
    }

    fn arcgis_service(self) -> &'static str {
        match self {
            Self::UsgsImagery => "USGSImageryOnly",
            Self::UsgsImageryTopo => "USGSImageryTopo",
            Self::UsgsTopo => "USGSTopo",
            Self::UsgsShadedRelief => "USGSShadedReliefOnly",
            Self::OpenStreetMap => "",
        }
    }

    /// The finest zoom this layer will request from this provider: the lower
    /// of what the service publishes and [`MAX_TILE_ZOOM`].
    #[must_use]
    pub fn max_zoom(self) -> u8 {
        let service = match self {
            Self::UsgsImagery | Self::UsgsImageryTopo | Self::UsgsTopo | Self::UsgsShadedRelief => {
                USGS_SERVICE_MAX_ZOOM
            }
            Self::OpenStreetMap => OSM_SERVICE_MAX_ZOOM,
        };
        service.min(MAX_TILE_ZOOM)
    }

    /// Whether the terms allow fetching tiles the user is not actively
    /// viewing.
    ///
    /// `false` for OpenStreetMap. Section 4 of the OSMF Standard Tile Layer
    /// Usage Policy defines bulk downloading as "any pre-emptive fetching of
    /// tiles other than those a user is actively viewing" and states that
    /// offline use is not permitted. So: no look-ahead ring, no "download this
    /// area", no speculative next-zoom warming for that provider. The USGS
    /// services carry no such restriction.
    #[must_use]
    pub fn prefetch_permitted(self) -> bool {
        !matches!(self, Self::OpenStreetMap)
    }

    /// How long a fetched tile must be kept before the network is touched for
    /// it again. This is the rate limit: the disk cache is what enforces it.
    #[must_use]
    pub fn min_cache_seconds(self) -> u64 {
        match self {
            Self::UsgsImagery | Self::UsgsImageryTopo | Self::UsgsTopo | Self::UsgsShadedRelief => {
                USGS_MAX_AGE_SECONDS
            }
            Self::OpenStreetMap => OSM_MIN_CACHE_SECONDS,
        }
    }

    /// Suggested darkening drawn over the imagery and under the vector map.
    ///
    /// Radar over an aerial photograph is genuinely harder to read than radar
    /// over flat dark: light returns, near-zero velocity and translucent
    /// hazard fills all lose contrast, and it reads as "the radar got worse"
    /// rather than "the basemap arrived". The scrim is the fix, and it ships
    /// with the feature rather than after it. Busier imagery needs more.
    #[must_use]
    pub fn default_scrim_alpha(self) -> f32 {
        match self {
            Self::UsgsImagery => 0.35,
            Self::OpenStreetMap => 0.30,
            Self::UsgsImageryTopo => 0.20,
            Self::UsgsTopo => 0.12,
            Self::UsgsShadedRelief => 0.05,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_provider_carries_a_credit_and_a_link() {
        for provider in TileProvider::ALL {
            assert!(!provider.attribution().is_empty(), "{provider:?}");
            assert!(!provider.attribution_full().is_empty(), "{provider:?}");
            assert!(
                provider.attribution_url().starts_with("https://"),
                "{provider:?}"
            );
            assert!(!provider.coverage_note().is_empty(), "{provider:?}");
        }
    }

    /// A cache key collision would serve one provider's imagery under
    /// another's name, and a key that is not a legal path component would put
    /// the cache somewhere unexpected.
    #[test]
    fn cache_keys_are_unique_and_path_safe() {
        let mut seen = std::collections::HashSet::new();
        for provider in TileProvider::ALL {
            assert!(seen.insert(provider.key()), "duplicate key {provider:?}");
            assert!(
                provider
                    .key()
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'),
                "{provider:?} key is not path safe"
            );
        }
        assert_eq!(seen.len(), TileProvider::ALL.len());
    }

    /// The ArcGIS row/col transposition is the mistake this test exists to
    /// catch. KTLX at z9 is x=117, y=202, and the URL that returned HTTP 200
    /// was `/tile/9/202/117`.
    #[test]
    fn arcgis_urls_use_row_col_order_and_osm_uses_x_y() {
        let tile = TileId::new(9, 117, 202).expect("valid");
        assert_eq!(
            TileProvider::UsgsImagery.tile_url(tile),
            "https://basemap.nationalmap.gov/arcgis/rest/services/USGSImageryOnly\
             /MapServer/tile/9/202/117"
        );
        assert_eq!(
            TileProvider::OpenStreetMap.tile_url(tile),
            "https://tile.openstreetmap.org/9/117/202.png"
        );
    }

    #[test]
    fn every_url_is_https_and_names_the_tile() {
        let tile = TileId::new(11, 468, 809).expect("valid");
        for provider in TileProvider::ALL {
            let url = provider.tile_url(tile);
            assert!(url.starts_with("https://"), "{provider:?}: {url}");
            assert!(url.contains("11"), "{provider:?}: {url}");
            assert!(
                url.contains("468") && url.contains("809"),
                "{provider:?}: {url}"
            );
        }
    }

    /// The OpenStreetMap policy constraints are load-bearing, so they are
    /// pinned by a test rather than left to a comment somebody might edit.
    #[test]
    fn openstreetmap_policy_flags_are_pinned() {
        assert!(!TileProvider::OpenStreetMap.prefetch_permitted());
        assert!(TileProvider::OpenStreetMap.min_cache_seconds() >= 7 * 86_400);
        assert!(
            TileProvider::OpenStreetMap
                .tile_url(TileId::new(9, 117, 202).expect("valid"))
                .starts_with("https://tile.openstreetmap.org/"),
            "the policy names this exact host; no subdomain sharding"
        );
        for provider in TileProvider::ALL {
            if provider != TileProvider::OpenStreetMap {
                assert!(provider.prefetch_permitted(), "{provider:?}");
            }
            assert!(provider.min_cache_seconds() >= 3_600, "{provider:?}");
            assert!(provider.max_zoom() <= MAX_TILE_ZOOM, "{provider:?}");
            assert!(provider.max_zoom() >= crate::MIN_TILE_ZOOM, "{provider:?}");
        }
    }

    #[test]
    fn scrim_defaults_are_in_range_and_track_how_busy_the_imagery_is() {
        for provider in TileProvider::ALL {
            let alpha = provider.default_scrim_alpha();
            assert!((0.0..=0.6).contains(&alpha), "{provider:?}: {alpha}");
        }
        assert!(
            TileProvider::UsgsImagery.default_scrim_alpha()
                > TileProvider::UsgsShadedRelief.default_scrim_alpha()
        );
    }
}

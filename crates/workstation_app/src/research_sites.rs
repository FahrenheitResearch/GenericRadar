//! Radars that publish Level 1 records and that the operational station feed
//! does not list.
//!
//! # Why this exists
//!
//! An RVP8 time-series header states a signal-processor name and no
//! coordinates, so a Level 1 record's position has to be looked up. The
//! directory the application already keeps for its site markers is
//! `https://api.weather.gov/radar/stations`, which is the NWS OPERATIONAL
//! network - 159 WSR-88Ds, 45 terminal radars and four profilers, all of them
//! feeding warnings. The radars whose time series are actually archived and
//! published are, almost by definition, the ones that are NOT in that list:
//! research and testbed installations, which serve no operational product and
//! so appear in no operational catalog.
//!
//! The reference record this feature was written against is from KOUN, NSSL's
//! research WSR-88D at Norman, Oklahoma. Before this module the application
//! looked KOUN up, found nothing, and said POSITION UNKNOWN - which was true,
//! and was already an improvement on the version that anchored the sweep
//! wherever the map happened to be left and drew a Norman stare over Smith and
//! Osborne counties, Kansas. Refusing to guess is right. Refusing when the
//! answer is published in a NOAA table is not the best this can do.
//!
//! # The evidence standard
//!
//! Every coordinate below is one row of one named public source, quoted beside
//! the number in enough detail to re-fetch it. Nothing here is transcribed
//! from memory, and nothing here is an average of what several pages say: an
//! averaged coordinate is a position no source states, drawn under real
//! weather, and it would be worse than the POSITION UNKNOWN it replaced. A
//! site that cannot be sourced to that standard is left out, and
//! `every_entry_carries_a_re_fetchable_source` fails if an entry ever
//! arrives without its citation.
//!
//! The trap this standard is guarding against is a real one and was hit while
//! writing this file. NCEI's HOMR holds two different stations under the ICAO
//! id `KOUN`: station 20014726, "NORMAN UNIVERSITY OF OKLAHOMA WESTHEIMER AP",
//! reported to the nearest arcMINUTE at 35.25 / -97.46667, and station
//! 30126015, "NORMAN NSSL", platform `NEXRAD`, reported to hundredths of an
//! arcsecond. The first is the airport. Taking it would have moved the radar
//! about 1.6 km north. The citations below therefore name the station id and
//! the platform, not just the ICAO code.
//!
//! # What is deliberately NOT here
//!
//! **Mobile radars - NOXP, and the DOW/COW family.** NSSL's X-band NOXP
//! appears in the same archives and in this repository's own DORADE fixture,
//! and it has no fixed position to catalogue: it is a truck. Its sweepfiles
//! carry the deployment's latitude and longitude in their own headers, which
//! is the only place the answer exists. A fixed entry for a mobile radar would
//! be wrong by tens or hundreds of kilometres on every deployment but one, and
//! wrong in the specific way this module exists to prevent - silently, under
//! real weather, at full confidence. So [`research_site`] answers `None` for
//! `NOXP`, and `a_mobile_radar_is_refused_rather_than_pinned` keeps it that
//! way.
//!
//! **The phased array at Norman.** The National Weather Radar Testbed's
//! successor, the Advanced Technology Demonstrator, publishes through the same
//! NSSL archive - but, as that archive's own catalogue states, "ATD data are
//! minimally quality controlled and are available in NEXRAD MSG31 and CfRadial
//! 1 formats". Both of those state the radar's position in the file: MSG31 in
//! the volume constant block this workspace already parses
//! (`nexrad_io::parse_volume_constant_block`), CfRadial in its `latitude` and
//! `longitude` variables. A record that carries its own coordinates must never
//! be overridden by a table - the file is the better authority about where its
//! own antenna stood - so an entry here could only ever be dead weight or a
//! disagreement. It is left out for that reason and not for want of a source.
//!
//! # Where this sits in the resolution order
//!
//! Last. `WorkstationApp::time_series_site` asks the operational directory
//! first and only falls through to here, so a site the NWS publishes is placed
//! from the NWS position even if this table were ever to disagree. As it
//! happens the two cannot disagree, because no id here is in the published
//! catalog - `no_entry_here_shadows_a_published_station` replays the whole
//! retrieved feed to prove it - but the ORDER is what makes that a property of
//! the code rather than a coincidence of the data.

/// A radar this application can place without the station feed.
///
/// Static rather than fetched, because that is what the data is: two research
/// RDAs whose positions have not moved since they were commissioned, in a
/// table nobody serves as a feed.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ResearchSite {
    /// The radar's id, as a record's site name reduces to. Uppercase.
    pub id: &'static str,
    /// What to show an analyst.
    pub name: &'static str,
    pub latitude_deg: f64,
    pub longitude_deg: f64,
    /// Where [`Self::latitude_deg`] and [`Self::longitude_deg`] came from,
    /// stated so that a reader can go and re-fetch the same row.
    ///
    /// `allow(dead_code)`, and the reason matters: this field is read by the
    /// tests below and by nothing in the shipped binary, and `dead_code` is
    /// judged per compilation unit. The alternative - putting the citation in
    /// a comment - would leave nothing able to FAIL when an entry arrives
    /// without one, and this whole module is an argument about evidence.
    #[allow(dead_code)]
    pub source: &'static str,
}

/// Every radar this module can place.
///
/// Two, and it will stay small. The bar for an entry is a citable public
/// coordinate for a radar that (a) has a fixed position at all and (b) does
/// not state that position in the records it publishes. Almost everything in
/// the research archives fails one of those two.
pub const RESEARCH_SITES: &[ResearchSite] = &[
    // NSSL's research WSR-88D, Norman, Oklahoma. The radar the published
    // Level 1 / time-series archive is mostly made of, and the one the
    // reference record `KOUN_RVP.20130520...` came from.
    ResearchSite {
        id: "KOUN",
        name: "Norman NSSL",
        // 35 deg 14 min 09.81 sec N, 097 deg 27 min 44.46 sec W.
        latitude_deg: 35.236058,
        longitude_deg: -97.46235,
        source: "NOAA NCEI HOMR station 30126015, principal name \"NORMAN NSSL\", \
                 platform NEXRAD, ids ICAO=KOUN and NEXRAD=KOUN; reported position \
                 35,14,09.81,N / 097,27,44.46,W (precision DDMMSSss), ground elevation \
                 391.4 m. Row `30126015 KOUN NORMAN NSSL UNITED STATES OK CLEVELAND \
                 35.236058 -97.46235 1284 -6 NEXRAD` in \
                 https://www.ncei.noaa.gov/access/homr/file/nexrad-stations.txt, \
                 retrieved 2026-08-21; same numbers from \
                 https://www.ncei.noaa.gov/access/homr/services/station/search\
                 ?qid=NCDCSTNID:30126015. NOT station 20014726, which is the airport.",
    },
    // The Radar Operations Center's test RDA, about 250 m north-east of KOUN
    // on the same Norman campus. It is the only other radar in NOAA's own
    // NEXRAD station table that the operational feed does not carry, and a
    // record from it would otherwise be refused for the same reason KOUN was.
    // Its own time series has not been observed among the NSSL archive files
    // examined for this reader; it is here because it is sourceable and
    // because the alternative is a second POSITION UNKNOWN, not because a file
    // from it has been read.
    ResearchSite {
        id: "KCRI",
        name: "Norman ROC test radar",
        // 35 deg 14 min 18 sec N, 097 deg 27 min 36 sec W. The longitude is
        // exactly -97.46 because 27 min 36 sec is exactly 0.46 deg, not
        // because a rounder number was written down.
        latitude_deg: 35.238333,
        longitude_deg: -97.46,
        source: "NOAA NCEI HOMR station 30078819, principal name \
                 \"ROC FAA REDUNDANT RDA 1\", ids ICAO=KCRI, NEXRAD=KCRI, NWSLI=CRIO2; \
                 reported position 35,14,18,N / 097,27,36,W (precision DDMMSSss), \
                 ground elevation 400.7 m. Row `30078819 KCRI ROC FAA REDUNDANT RDA 1 \
                 UNITED STATES OK CLEVELAND 35.238333 -97.46 1315 -6 NEXRAD` in \
                 https://www.ncei.noaa.gov/access/homr/file/nexrad-stations.txt, \
                 retrieved 2026-08-21; same numbers from \
                 https://www.ncei.noaa.gov/access/homr/services/station/search\
                 ?qid=ICAO:KCRI.",
    },
];

/// The radar a record's site name names, if this table knows it.
///
/// Tolerant of the signal processor's suffix, because the name in the header
/// is the PROCESSOR's: the reference records call themselves `KOUN_RVP`, which
/// is the RVP8 in the KOUN equipment room. `crate::iq_session` strips that
/// suffix before it gets here, and this function strips it again anyway rather
/// than requiring a caller to have done so - a catalog that only answers
/// correctly when its input has already been normalised somewhere else is a
/// catalog with a second, invisible, uncatalogued rule.
///
/// Tolerant, and no more. The match after the suffix is stripped is exact, so
/// `KOUNTY` is not KOUN and `KO` is not KOUN. A prefix or fuzzy match here
/// would be a guess, which is the thing this module is not for.
pub fn research_site(site_name: &str) -> Option<&'static ResearchSite> {
    let key = catalog_key(site_name);
    if key.is_empty() {
        return None;
    }
    RESEARCH_SITES
        .iter()
        .find(|site| site.id.eq_ignore_ascii_case(key))
}

/// The radar name inside a processor name: everything up to the first
/// separator.
///
/// Returns a borrow of the input, so the lookup allocates nothing. A name that
/// is nothing but separators is returned whole rather than as an empty string,
/// so it can fail the exact match below like any other unknown name instead of
/// being silently treated as "no name given".
fn catalog_key(site_name: &str) -> &str {
    let trimmed = site_name.trim();
    match trimmed.split(['_', '-', ' ', '.']).next() {
        Some(head) if !head.is_empty() => head,
        _ => trimmed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The record the whole feature was written against resolves, to the
    /// position the source states and not to a rounded version of it.
    #[test]
    fn the_reference_records_site_resolves_to_the_sourced_position() {
        let site = research_site("KOUN_RVP").expect("KOUN_RVP is the reference record's site");
        assert_eq!(site.id, "KOUN");
        assert_eq!(site.latitude_deg, 35.236058);
        assert_eq!(site.longitude_deg, -97.46235);
        // Whatever shape the name arrives in. The header field is the
        // processor's, and this table is not the place that decides how a
        // processor spells itself.
        for spelling in ["KOUN", "koun_rvp", "KOUN-RVP8", " KOUN_RVP ", "KOUN.RVP"] {
            assert_eq!(
                research_site(spelling).map(|site| site.id),
                Some("KOUN"),
                "{spelling:?} did not reach the catalog"
            );
        }
    }

    /// The honest-refusal path survives. This is the behaviour being extended,
    /// not replaced: a name the table does not know still yields no position,
    /// and the pane still says POSITION UNKNOWN rather than drawing a sweep
    /// over a borrowed geography.
    #[test]
    fn a_name_the_catalog_does_not_know_yields_no_position() {
        let refused = |name: &str| {
            assert_eq!(
                research_site(name),
                None,
                "{name:?} was given a position it has no source for"
            );
        };

        // `ZQZQ_RVP` is the id the application's own unlocated-frame test
        // uses; the rest are not radar ids at all.
        for nonsense in ["ZQZQ_RVP", "", "   ", "_RVP", "___"] {
            refused(nonsense);
        }

        // Real radars that belong to the operational directory and must not be
        // answered from here.
        for published in ["KTLX", "KDVN", "TDFW"] {
            refused(published);
        }

        // Close to an entry, and not it. A prefix or fuzzy match would place a
        // record from a radar that does not exist at KOUN's antenna.
        for near_miss in ["KOUNTY", "KO", "KOU", "KOUN2", "KCRIB"] {
            refused(near_miss);
        }
    }

    /// A truck is not a site. See the module note: NOXP's position lives in
    /// the sweepfile it wrote, because that is the only place it exists.
    #[test]
    fn a_mobile_radar_is_refused_rather_than_pinned() {
        for spelling in ["NOXP", "NOXP-RVP8", "NOXPRVP", "noxp_rvp"] {
            assert_eq!(
                research_site(spelling),
                None,
                "{spelling:?} was given a fixed position, and it does not have one"
            );
        }
    }

    /// The operational catalog cannot lose an argument it is never in.
    ///
    /// `WorkstationApp::time_series_site` asks the directory first, so
    /// precedence is settled there; this is the stronger statement that there
    /// is no id for the two tables to disagree about in the first place.
    /// Replays the whole retrieved feed rather than a handful of rows.
    #[test]
    fn no_entry_here_shadows_a_published_station() {
        for line in crate::nearest_site::REAL_STATION_CATALOG.lines() {
            let Some(id) = line.split('\t').next() else {
                continue;
            };
            if id.is_empty() {
                continue;
            }
            assert_eq!(
                research_site(id),
                None,
                "{id} is published by api.weather.gov and is also in this table"
            );
        }
    }

    /// The point of the module, enforced. An entry without a re-fetchable
    /// source is a coordinate somebody remembered.
    #[test]
    fn every_entry_carries_a_re_fetchable_source() {
        for site in RESEARCH_SITES {
            assert!(
                site.source.contains("https://") || site.source.contains("doi:"),
                "{}'s source names no fetchable document: {:?}",
                site.id,
                site.source
            );
            assert!(
                site.source.len() > 80,
                "{}'s source is too short to identify a row: {:?}",
                site.id,
                site.source
            );
            assert!(!site.name.is_empty(), "{} has no display name", site.id);
        }
    }

    /// House rules for the table itself: ids are the shape a lookup key has,
    /// they are unique, and the coordinates are on the earth.
    #[test]
    fn the_table_is_well_formed() {
        for site in RESEARCH_SITES {
            assert_eq!(
                site.id,
                site.id.to_ascii_uppercase(),
                "{} is not uppercase, so a lookup would depend on how it was typed",
                site.id
            );
            assert_eq!(
                catalog_key(site.id),
                site.id,
                "{} contains a separator, so it can never be matched",
                site.id
            );
            assert!(
                (-90.0..=90.0).contains(&site.latitude_deg)
                    && (-180.0..=180.0).contains(&site.longitude_deg),
                "{} is not on the earth",
                site.id
            );
            assert_eq!(
                RESEARCH_SITES
                    .iter()
                    .filter(|other| other.id == site.id)
                    .count(),
                1,
                "{} is in the table twice",
                site.id
            );
        }
    }

    /// The two Norman entries are on the same campus, which is the one
    /// cross-check the sources allow: KOUN and KCRI are a few hundred metres
    /// apart, and a digit dropped or a sign flipped in either row would show
    /// up here as a continental separation rather than as a plausible number
    /// nobody looks at again.
    #[test]
    fn the_two_norman_radars_landed_on_the_same_campus() {
        let koun = research_site("KOUN").expect("KOUN");
        let kcri = research_site("KCRI").expect("KCRI");
        let north_km = (kcri.latitude_deg - koun.latitude_deg) * 111.32;
        let east_km = (kcri.longitude_deg - koun.longitude_deg)
            * 111.32
            * koun.latitude_deg.to_radians().cos();
        let separation_km = north_km.hypot(east_km);
        assert!(
            separation_km < 1.0,
            "the two Norman radars came out {separation_km:.1} km apart"
        );
        // And Norman is where both of them are, to a tolerance no transcription
        // error could survive.
        for site in [koun, kcri] {
            assert!(
                (35.0..35.5).contains(&site.latitude_deg)
                    && (-97.7..-97.2).contains(&site.longitude_deg),
                "{} is not in central Oklahoma",
                site.id
            );
        }
    }
}

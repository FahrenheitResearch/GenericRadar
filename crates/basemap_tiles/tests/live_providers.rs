//! Live provider tests. **Network required, so every test here is
//! `#[ignore]`d** and the ordinary gate does not depend on the internet.
//!
//! Run them deliberately:
//!
//! ```text
//! cargo test --release -p basemap_tiles --test live_providers -- --ignored --nocapture
//! ```
//!
//! What they prove, against the real endpoints rather than a mock: that a cold
//! cache fills, that a warm cache serves without a socket, that an expired
//! entry revalidates to 304 instead of re-downloading, that offline serves the
//! cache and nothing else, and that an out-of-coverage tile becomes
//! `Absent` rather than a retry loop. The first test also writes the decoded
//! pixels back out as PNG so a human can look at real imagery instead of
//! trusting a byte count.
//!
//! Set `BASEMAP_TILES_LIVE_OUT` to choose where those PNGs land.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use basemap_tiles::{
    DecodedTile, TileCacheConfig, TileId, TileProvider, TileState, TileStore, default_user_agent,
};

/// KTLX, Oklahoma City — a real NEXRAD site.
const KTLX: (f64, f64) = (-97.2778, 35.3333);

fn scratch(label: &str) -> PathBuf {
    let base = std::env::var_os("BASEMAP_TILES_LIVE_OUT")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let path = base.join(format!("basemap-tiles-live-{label}"));
    let _ = std::fs::remove_dir_all(&path);
    std::fs::create_dir_all(&path).expect("create scratch directory");
    path
}

fn config(root: &Path, offline: bool) -> TileCacheConfig {
    TileCacheConfig {
        disk_root: Some(root.to_path_buf()),
        max_disk_bytes: 64 * 1024 * 1024,
        max_workers: 4,
        user_agent: default_user_agent(),
        offline,
    }
}

/// Drive the store until every key reaches a terminal state, or time out.
fn settle(
    store: &mut TileStore,
    keys: &[(TileProvider, TileId)],
    timeout: Duration,
) -> Vec<Arc<DecodedTile>> {
    let wanted: HashSet<(TileProvider, TileId)> = keys.iter().copied().collect();
    let deadline = Instant::now() + timeout;
    let mut decoded = Vec::new();
    loop {
        for (provider, tile) in keys {
            store.request(*provider, *tile);
        }
        store.retain(&wanted);
        decoded.extend(store.drain_ready(64));
        let settled = keys.iter().all(|(provider, tile)| {
            matches!(
                store.state(*provider, *tile),
                TileState::Ready | TileState::Absent | TileState::Failed
            )
        });
        if settled || Instant::now() > deadline {
            // One last drain, so a tile that landed on the final pass is not
            // left in the channel.
            decoded.extend(store.drain_ready(64));
            return decoded;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn tiles_over(lon: f64, lat: f64, z: u8, radius: i64) -> Vec<TileId> {
    let center = TileId::containing(lon, lat, z).expect("a real site is on the map");
    let span = i64::from(center.span());
    let mut tiles = Vec::new();
    for dy in -radius..=radius {
        for dx in -radius..=radius {
            let y = i64::from(center.y) + dy;
            if y < 0 || y >= span {
                continue;
            }
            let x = (i64::from(center.x) + dx).rem_euclid(span) as u32;
            if let Some(tile) = TileId::new(z, x, y as u32) {
                tiles.push(tile);
            }
        }
    }
    tiles
}

/// A cold cache over a real radar site, on every shipped provider, and the
/// decoded pixels written out so they can be looked at.
#[test]
#[ignore = "requires network access to the tile providers"]
fn a_cold_cache_fills_from_every_provider_over_a_real_radar_site() {
    let out = scratch("cold");
    let wakes = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&wakes);
    let mut store = TileStore::new(
        config(&out.join("cache"), false),
        Arc::new(move || {
            counter.fetch_add(1, Ordering::Relaxed);
        }),
    );

    for provider in TileProvider::ALL {
        let tiles = tiles_over(KTLX.0, KTLX.1, 9, 1);
        let keys: Vec<_> = tiles.iter().map(|tile| (provider, *tile)).collect();
        let started = Instant::now();
        let decoded = settle(&mut store, &keys, Duration::from_secs(60));
        let elapsed = started.elapsed();

        let ready = keys
            .iter()
            .filter(|(p, t)| store.state(*p, *t) == TileState::Ready)
            .count();
        let absent = keys
            .iter()
            .filter(|(p, t)| store.state(*p, *t) == TileState::Absent)
            .count();
        let failed = keys
            .iter()
            .filter(|(p, t)| store.state(*p, *t) == TileState::Failed)
            .count();
        println!(
            "{:22} {:2} tiles in {:6.0} ms -> ready {ready}, absent {absent}, failed {failed}",
            provider.label(),
            keys.len(),
            elapsed.as_secs_f64() * 1000.0
        );
        println!("    attribution: {}", provider.attribution());

        assert_eq!(failed, 0, "{:?}: {failed} tiles failed", provider);
        assert_eq!(
            ready + absent,
            keys.len(),
            "{provider:?}: some tiles never settled"
        );

        // MEASURED, and the reason `Absent` exists: the USGS shaded-relief
        // service really is missing zoom 9 over KTLX, so at this zoom every
        // tile 404s and the layer has to stand a coarser one in. What the
        // product requires is not that the exact tile arrives, but that
        // SOMETHING drawable does.
        let mut substituted = 0;
        for (provider, tile) in &keys {
            if store.state(*provider, *tile) != TileState::Absent {
                continue;
            }
            let mut stood_in = None;
            for levels in 1..=basemap_tiles::MAX_ANCESTOR_LEVELS {
                let Some(ancestor) = tile.ancestor(levels) else {
                    break;
                };
                settle(
                    &mut store,
                    &[(*provider, ancestor)],
                    Duration::from_secs(30),
                );
                if store.state(*provider, ancestor) == TileState::Ready {
                    assert!(
                        tile.uv_offset_scale_within(ancestor).is_some(),
                        "the stand-in must have a UV sub-rect"
                    );
                    stood_in = Some((levels, ancestor));
                    break;
                }
            }
            let (levels, ancestor) = stood_in.unwrap_or_else(|| {
                panic!(
                    "{provider:?}: {tile:?} is absent and no ancestor within \
                     {} levels could stand in for it, so this pane would have a hole",
                    basemap_tiles::MAX_ANCESTOR_LEVELS
                )
            });
            if substituted == 0 {
                println!("    absent tiles fall back {levels} level(s), e.g. to {ancestor:?}");
            }
            substituted += 1;
        }
        if substituted > 0 {
            println!("    {substituted} of {} tiles were substituted", keys.len());
        }

        // Write the centre tile out so a human can look at real imagery.
        if let Some(sample) = decoded.first() {
            for level in 0..sample.mip_level_count() {
                let (bytes, edge) = sample.level(level).expect("level present");
                let image = image::RgbaImage::from_raw(edge, edge, bytes.to_vec())
                    .expect("RGBA buffer matches its declared size");
                let path = out.join(format!("{}-mip{level}.png", provider.key()));
                image.save(&path).expect("write PNG");
                if level == 0 {
                    println!("    wrote {}", path.display());
                }
            }
        }
    }

    let metrics = store.metrics();
    println!("metrics: {metrics:?}");
    println!("wake callbacks: {}", wakes.load(Ordering::Relaxed));
    assert!(metrics.downloaded > 0);
    assert!(metrics.bytes_downloaded > 0);
    assert!(metrics.disk_bytes > 0, "nothing reached the disk cache");
    assert!(
        wakes.load(Ordering::Relaxed) > 0,
        "the host was never woken, so a real UI would never repaint"
    );
    println!("output directory: {}", out.display());
}

/// The second visit must not touch the network. This is the rate limit: the
/// provider's minimum cache lifetime is enforced by the disk cache, not by a
/// timer.
#[test]
#[ignore = "requires network access to the tile providers"]
fn a_warm_cache_serves_without_opening_a_socket() {
    let out = scratch("warm");
    let cache_root = out.join("cache");
    let tiles = tiles_over(KTLX.0, KTLX.1, 9, 1);
    let keys: Vec<_> = tiles
        .iter()
        .map(|tile| (TileProvider::UsgsImageryTopo, *tile))
        .collect();

    let cold_downloaded;
    let cold_bytes;
    {
        let mut store = TileStore::new(config(&cache_root, false), Arc::new(|| {}));
        let started = Instant::now();
        settle(&mut store, &keys, Duration::from_secs(60));
        let metrics = store.metrics();
        cold_downloaded = metrics.downloaded;
        cold_bytes = metrics.bytes_downloaded;
        println!(
            "cold:  {} downloaded, {} bytes, {:.0} ms",
            metrics.downloaded,
            metrics.bytes_downloaded,
            started.elapsed().as_secs_f64() * 1000.0
        );
        assert_eq!(cold_downloaded as usize, keys.len());
    }

    // A fresh store over the same directory: the entries are inside the
    // provider's minimum cache lifetime, so nothing may go out.
    let mut store = TileStore::new(config(&cache_root, false), Arc::new(|| {}));
    let started = Instant::now();
    settle(&mut store, &keys, Duration::from_secs(60));
    let metrics = store.metrics();
    println!(
        "warm:  {} from disk, {} downloaded, {} revalidated, {:.0} ms",
        metrics.served_from_disk,
        metrics.downloaded,
        metrics.revalidated_304,
        started.elapsed().as_secs_f64() * 1000.0
    );

    assert_eq!(
        metrics.served_from_disk as usize,
        keys.len(),
        "the warm run did not come off disk"
    );
    assert_eq!(
        metrics.downloaded, 0,
        "the warm run went to the network anyway"
    );
    assert_eq!(
        metrics.revalidated_304, 0,
        "no request should have been made"
    );
    assert_eq!(metrics.bytes_downloaded, 0);
    let _ = cold_bytes;
}

/// An entry past its lifetime revalidates rather than re-downloading. The USGS
/// services send an ETag and no Last-Modified, so `If-None-Match` is the only
/// mechanism available — and it works: 304, no body, no bytes.
#[test]
#[ignore = "requires network access to the tile providers"]
fn an_expired_entry_revalidates_to_304_instead_of_downloading_again() {
    let out = scratch("revalidate");
    let cache_root = out.join("cache");
    let tiles = tiles_over(KTLX.0, KTLX.1, 9, 0);
    let keys: Vec<_> = tiles
        .iter()
        .map(|tile| (TileProvider::UsgsTopo, *tile))
        .collect();

    let mut store = TileStore::new(config(&cache_root, false), Arc::new(|| {}));
    settle(&mut store, &keys, Duration::from_secs(60));
    assert_eq!(store.metrics().downloaded as usize, keys.len());
    drop(store);

    // Age every entry past the provider's minimum cache lifetime by rewriting
    // the fetch timestamp in place. The entry layout is documented in
    // `basemap_tiles::cache`: magic(4) version(1) etag_len(2) fetched_at(8).
    let aged = age_every_entry(&cache_root);
    assert!(aged > 0, "no cache entries were found to age");
    println!("aged {aged} cache entries");

    let mut store = TileStore::new(config(&cache_root, false), Arc::new(|| {}));
    settle(&mut store, &keys, Duration::from_secs(60));
    let metrics = store.metrics();
    println!("revalidation metrics: {metrics:?}");
    assert_eq!(
        metrics.revalidated_304 as usize,
        keys.len(),
        "expected a 304 for every aged entry"
    );
    assert_eq!(metrics.downloaded, 0, "a 304 must not re-download the body");
    assert_eq!(metrics.bytes_downloaded, 0);
    for (provider, tile) in &keys {
        assert_eq!(store.state(*provider, *tile), TileState::Ready);
    }
}

/// A cached body that no longer decodes, revalidated against a provider that
/// answers **304 Not Modified**, must still recover.
///
/// This is the nastiest shape the cache can take and it cannot be proved
/// against a fixture: it needs a real server that really does answer 304 for
/// the ETag we still hold. "Your copy is current" is exactly the wrong answer
/// when our copy is a corrupt file, so the entry has to be thrown away at that
/// point rather than revalidated forever. Before this was fixed the entry was
/// *touched* — marked fresh for another day — before its body was decoded, so
/// a single flipped byte in the cache became a tile that never came back.
#[test]
#[ignore = "requires network access to the tile providers"]
fn a_corrupt_cached_body_recovers_even_when_the_provider_answers_304() {
    let out = scratch("corrupt-304");
    let cache_root = out.join("cache");
    let key = (
        TileProvider::UsgsTopo,
        TileId::containing(KTLX.0, KTLX.1, 9).expect("on the map"),
    );

    let mut store = TileStore::new(config(&cache_root, false), Arc::new(|| {}));
    settle(&mut store, &[key], Duration::from_secs(60));
    assert_eq!(store.state(key.0, key.1), TileState::Ready);
    assert_eq!(store.metrics().downloaded, 1);
    drop(store);

    // Keep the header and the ETag, replace the image with an error page, and
    // age the entry so it is revalidated rather than served.
    let corrupted = corrupt_every_body(&cache_root);
    assert_eq!(corrupted, 1, "expected exactly one cache entry to corrupt");
    let aged = age_every_entry(&cache_root);
    assert_eq!(aged, 1);

    let mut store = TileStore::new(config(&cache_root, false), Arc::new(|| {}));
    settle(&mut store, &[key], Duration::from_secs(60));
    let metrics = store.metrics();
    println!(
        "after corruption: {metrics:?} state {:?}",
        store.state(key.0, key.1)
    );
    assert_eq!(
        metrics.revalidated_304, 1,
        "the provider did not answer 304, so this test proved nothing"
    );
    assert_ne!(
        store.state(key.0, key.1),
        TileState::Ready,
        "a body that does not decode must not be reported as ready"
    );
    let remaining = count_entries(&cache_root);
    assert_eq!(
        remaining, 0,
        "the undecodable entry survived its own revalidation"
    );
    drop(store);

    // A fresh session refetches it unconditionally and the tile comes back.
    let mut store = TileStore::new(config(&cache_root, false), Arc::new(|| {}));
    settle(&mut store, &[key], Duration::from_secs(60));
    println!("recovery: {:?}", store.metrics());
    assert_eq!(store.state(key.0, key.1), TileState::Ready);
    assert_eq!(store.metrics().downloaded, 1, "the tile was not refetched");
    drop(store);

    // The other half of the same problem: an entry corrupted while still
    // INSIDE its cache lifetime. Its body is decoded up front, fails, and the
    // file is deleted — after which its ETag must not be sent either, or the
    // provider answers 304 about a file that no longer exists and the tile
    // fails on a pass that should have simply refetched it.
    assert_eq!(corrupt_every_body(&cache_root), 1);
    let mut store = TileStore::new(config(&cache_root, false), Arc::new(|| {}));
    settle(&mut store, &[key], Duration::from_secs(60));
    let metrics = store.metrics();
    println!(
        "fresh-but-corrupt: {metrics:?} state {:?}",
        store.state(key.0, key.1)
    );
    assert_eq!(
        store.state(key.0, key.1),
        TileState::Ready,
        "a corrupt but unexpired entry must be refetched on the same pass"
    );
    assert_eq!(
        metrics.revalidated_304, 0,
        "a body we have just thrown away must not be revalidated"
    );
    assert_eq!(metrics.downloaded, 1);
}

/// Offline serves what is cached and opens nothing. Proved by pulling the
/// network out from under a warm cache.
#[test]
#[ignore = "requires network access to the tile providers"]
fn offline_serves_the_warm_cache_and_nothing_else() {
    let out = scratch("offline");
    let cache_root = out.join("cache");
    let cached = tiles_over(KTLX.0, KTLX.1, 9, 0);
    let keys: Vec<_> = cached
        .iter()
        .map(|tile| (TileProvider::UsgsImagery, *tile))
        .collect();

    let mut store = TileStore::new(config(&cache_root, false), Arc::new(|| {}));
    settle(&mut store, &keys, Duration::from_secs(60));
    assert_eq!(store.metrics().downloaded as usize, keys.len());
    drop(store);

    let mut store = TileStore::new(config(&cache_root, true), Arc::new(|| {}));
    assert!(store.is_offline());
    settle(&mut store, &keys, Duration::from_secs(10));
    let metrics = store.metrics();
    println!("offline metrics: {metrics:?}");
    assert_eq!(metrics.served_from_disk as usize, keys.len());
    assert_eq!(metrics.downloaded, 0);
    assert_eq!(metrics.failed, 0, "offline must never record a failure");
    for (provider, tile) in &keys {
        assert_eq!(store.state(*provider, *tile), TileState::Ready);
    }

    // A tile that was never cached must park, not fail, and not retry.
    let uncached = (
        TileProvider::UsgsImagery,
        TileId::containing(-122.965, 45.715, 9).expect("KRTX is on the map"),
    );
    settle(&mut store, &[uncached], Duration::from_secs(5));
    assert_eq!(store.state(uncached.0, uncached.1), TileState::Pending);
    assert_eq!(store.metrics().failed, 0);
    let requested = store.metrics().requested;
    for _ in 0..200 {
        store.request(uncached.0, uncached.1);
    }
    assert_eq!(
        store.metrics().requested,
        requested,
        "an offline miss must not be re-queued every frame"
    );
}

/// An out-of-coverage tile is `Absent`, permanently, and is never asked for
/// again. Measured: the USGS imagery service has nothing outside the United
/// States above roughly zoom 8.
#[test]
#[ignore = "requires network access to the tile providers"]
fn an_out_of_coverage_tile_becomes_absent_and_is_never_retried() {
    let out = scratch("absent");
    let mut store = TileStore::new(config(&out.join("cache"), false), Arc::new(|| {}));

    // Paris, well outside USGS orthoimagery coverage at this zoom.
    let paris = TileId::containing(2.3522, 48.8566, 12).expect("on the map");
    let key = (TileProvider::UsgsImagery, paris);
    settle(&mut store, &[key], Duration::from_secs(30));

    let state = store.state(key.0, key.1);
    println!("Paris z12 on {}: {state:?}", key.0.label());
    assert_eq!(
        state,
        TileState::Absent,
        "expected a 404 to be recorded as permanently absent"
    );
    assert!(store.metrics().absent > 0);

    let requested = store.metrics().requested;
    for _ in 0..500 {
        store.request(key.0, key.1);
    }
    assert_eq!(
        store.metrics().requested,
        requested,
        "an absent tile must never be probed again"
    );
    assert_eq!(store.metrics().queued, 0);

    // And an ancestor is available to stand in for it, which is the whole
    // reason `Absent` is distinct from `Failed`.
    let ancestor = paris.ancestor(4).expect("has an ancestor");
    let ancestor_key = (TileProvider::UsgsImagery, ancestor);
    settle(&mut store, &[ancestor_key], Duration::from_secs(30));
    println!(
        "ancestor z{} {}/{}: {:?}, uv {:?}",
        ancestor.z,
        ancestor.x,
        ancestor.y,
        store.state(ancestor_key.0, ancestor_key.1),
        paris.uv_offset_scale_within(ancestor)
    );
    assert!(paris.uv_offset_scale_within(ancestor).is_some());
}

/// Attribution is a condition of use, so it is checked against the provider
/// rather than transcribed and trusted.
///
/// Each USGS service publishes the credit it requires in its own
/// `copyrightText`. This compares [`TileProvider::attribution_full`] against
/// that field, character for character, for all four. It found a real drift
/// the first time it ran: the US Topo credit shipped without its trailing
/// data-refresh sentence.
///
/// A failure here is not a broken test. It means the service has changed the
/// credit it asks for and the provider table has to follow.
#[test]
#[ignore = "requires network access to the tile providers"]
fn every_usgs_credit_matches_the_service_that_publishes_it() {
    let client = reqwest::blocking::Client::builder()
        .user_agent(default_user_agent())
        .timeout(Duration::from_secs(30))
        .build()
        .expect("build client");

    for provider in TileProvider::ALL {
        let Some(service) = usgs_service_url(provider) else {
            // OpenStreetMap publishes no such document; its required credit is
            // fixed by the licence and pinned by a unit test instead.
            assert_eq!(
                provider.attribution(),
                "\u{a9} OpenStreetMap contributors",
                "the ODbL attribution is not ours to paraphrase"
            );
            continue;
        };
        let body = client
            .get(&service)
            .send()
            .expect("service metadata")
            .text()
            .expect("body");
        let published = copyright_text(&body)
            .unwrap_or_else(|| panic!("{provider:?}: no copyrightText in {service}"));
        println!("{:22} {published}", provider.key());
        assert_eq!(
            provider.attribution_full(),
            published,
            "{provider:?}: the shipped credit is no longer what the service asks for"
        );
        // The short on-screen form is an abbreviation, not a prefix — "USGS
        // The National Map: US Topo" stands for a paragraph naming a dozen
        // datasets. What it must not do is credit somebody else, so it has to
        // name the same publisher the service names, and it has to be short
        // enough to actually fit in the corner of a map pane.
        const PUBLISHER: &str = "USGS The National Map";
        assert!(
            published.contains(PUBLISHER),
            "{provider:?}: the service no longer names {PUBLISHER}"
        );
        assert!(
            provider.attribution().contains(PUBLISHER),
            "{provider:?}: the on-screen credit does not name the publisher"
        );
        assert!(
            provider.attribution().chars().count() <= 60,
            "{provider:?}: the on-screen credit is too long to draw"
        );
    }
}

fn usgs_service_url(provider: TileProvider) -> Option<String> {
    // Derived from the tile URL rather than repeated, so the two cannot drift.
    let tile_url = provider.tile_url(TileId::new(9, 117, 202).expect("valid"));
    let base = tile_url.split("/MapServer/tile/").next()?;
    (base != tile_url).then(|| format!("{base}/MapServer?f=json"))
}

/// Pull `copyrightText` out of the service document without a JSON dependency.
/// The USGS strings contain no escapes, which is asserted rather than assumed.
fn copyright_text(body: &str) -> Option<String> {
    let key = "\"copyrightText\":";
    let start = body.find(key)? + key.len();
    let rest = body[start..].trim_start();
    let rest = rest.strip_prefix('"')?;
    let end = rest.find('"')?;
    let value = &rest[..end];
    assert!(
        !value.contains('\\'),
        "the credit contains a JSON escape this reader does not handle: {value}"
    );
    Some(value.to_owned())
}

/// A network failure with no cache must not become an infinite retry loop.
/// Pointed at a host that does not resolve, so no provider is bothered.
#[test]
#[ignore = "requires a DNS lookup to fail, which needs a real resolver"]
fn a_hostile_network_backs_off_rather_than_hammering() {
    // The store only ever talks to the provider table, so an unreachable host
    // is simulated by taking the network away: an offline store with a cache
    // directory that exists but is empty. See `offline_serves_the_warm_cache`
    // for the cached half. This test documents that there is no code path in
    // `TileStore` that retries faster than `RETRY_BACKOFF`, which the unit
    // test `the_retry_schedule_widens_and_then_stops` pins directly.
    let out = scratch("backoff");
    let mut store = TileStore::new(config(&out.join("cache"), true), Arc::new(|| {}));
    let key = (
        TileProvider::UsgsImagery,
        TileId::containing(KTLX.0, KTLX.1, 9).expect("on the map"),
    );
    settle(&mut store, &[key], Duration::from_secs(5));
    assert_eq!(store.metrics().failed, 0);
}

/// Replace every cached image body with an error page, keeping the entry
/// header and its ETag intact. Returns how many entries were rewritten.
fn corrupt_every_body(root: &Path) -> usize {
    let mut corrupted = 0;
    for path in cache_entry_paths(root) {
        let Ok(bytes) = std::fs::read(&path) else {
            continue;
        };
        if bytes.len() < 15 || &bytes[..4] != b"RWT1" {
            continue;
        }
        let etag_len = u16::from_le_bytes([bytes[5], bytes[6]]) as usize;
        let mut rewritten = bytes[..15 + etag_len].to_vec();
        rewritten.extend_from_slice(b"<html><head><title>not an image</title></head></html>");
        if std::fs::write(&path, &rewritten).is_ok() {
            corrupted += 1;
        }
    }
    corrupted
}

fn count_entries(root: &Path) -> usize {
    cache_entry_paths(root).len()
}

fn cache_entry_paths(root: &Path) -> Vec<PathBuf> {
    fn walk(directory: &Path, out: &mut Vec<PathBuf>) {
        let Ok(listing) = std::fs::read_dir(directory) else {
            return;
        };
        for entry in listing.flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, out);
            } else if path
                .extension()
                .is_some_and(|extension| extension == "tile")
            {
                out.push(path);
            }
        }
    }
    let mut out = Vec::new();
    walk(root, &mut out);
    out
}

/// Rewrite the `fetched_at` field of every cache entry to the epoch, so the
/// next visit treats it as stale and revalidates.
fn age_every_entry(root: &Path) -> usize {
    fn walk(directory: &Path, aged: &mut usize) {
        let Ok(listing) = std::fs::read_dir(directory) else {
            return;
        };
        for entry in listing.flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, aged);
            } else if path
                .extension()
                .is_some_and(|extension| extension == "tile")
            {
                let Ok(mut bytes) = std::fs::read(&path) else {
                    continue;
                };
                if bytes.len() < 15 || &bytes[..4] != b"RWT1" {
                    continue;
                }
                bytes[7..15].copy_from_slice(&0u64.to_le_bytes());
                if std::fs::write(&path, &bytes).is_ok() {
                    *aged += 1;
                }
            }
        }
    }
    let mut aged = 0;
    walk(root, &mut aged);
    aged
}

/// The fetch pool, sized by measurement rather than taste: sixteen cold tiles
/// — one 512-point pane's worth — against the live USGS service under one,
/// four, six and eight workers.
///
/// Each pool size fetches a DIFFERENT sixteen tiles (adjacent 4x4 blocks at
/// z12 around KTLX) so no run is warmed by the one before it, on a fresh
/// scratch cache each time. The numbers are printed for the record; the only
/// assertion is the one that justifies a pool at all — that one worker is
/// materially slower than several — because absolute times belong to the
/// network being measured, not to this crate.
#[test]
#[ignore = "requires network access to the tile providers"]
fn the_worker_pool_is_sized_against_a_measured_cold_pane() {
    let provider = TileProvider::UsgsImageryTopo;
    let center = TileId::containing(KTLX.0, KTLX.1, 12).expect("KTLX is on the map");
    let mut wall_clock = Vec::new();
    for (slot, workers) in [1_usize, 4, 6, 8].into_iter().enumerate() {
        // A distinct 4x4 block per pool size, offset east so nothing overlaps.
        let mut keys = Vec::new();
        for dy in 0..4_u32 {
            for dx in 0..4_u32 {
                let x = center.x + 8 * slot as u32 + dx;
                let tile = TileId::new(12, x, center.y + dy).expect("in range");
                keys.push((provider, tile));
            }
        }
        let root = scratch(&format!("pool-{workers}"));
        let mut store = TileStore::new(
            TileCacheConfig {
                max_workers: workers,
                ..config(&root, false)
            },
            Arc::new(|| {}),
        );
        let started = Instant::now();
        let decoded = settle(&mut store, &keys, Duration::from_secs(60));
        let elapsed = started.elapsed();
        let metrics = store.metrics();
        println!(
            "{workers} worker(s): 16 tiles in {} ms ({} downloaded, {} failed)",
            elapsed.as_millis(),
            metrics.downloaded,
            metrics.failed
        );
        assert_eq!(
            metrics.failed, 0,
            "a fetch failed; the timing is not comparable"
        );
        assert!(
            decoded.len() >= 16,
            "only {} of 16 tiles decoded",
            decoded.len()
        );
        wall_clock.push((workers, elapsed));
        let _ = std::fs::remove_dir_all(&root);
    }
    let one = wall_clock[0].1;
    let best = wall_clock[1..]
        .iter()
        .map(|(_, elapsed)| *elapsed)
        .min()
        .expect("measured");
    assert!(
        best * 2 < one,
        "parallel fetching gained less than 2x over sequential ({best:?} vs {one:?}), so the \
         pool size deserves re-measuring"
    );
}

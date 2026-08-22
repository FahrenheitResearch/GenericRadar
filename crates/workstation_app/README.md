# workstation_app

The application crate: the egui/wgpu front end and the `GenericRadar` binary.
The radar work itself lives in the sibling crates - `nexrad_io` (decoding),
`render2d` (rasterising and derived products), `map_scene` and `basemap_tiles`
(the map), `color_tables`, `settings`, `product_engine`, `analyst_runtime`.

## Run

From the repository root:

```text
cargo run --release -p workstation_app --bin GenericRadar
```

Open a radar file by dropping it onto the window, entering its path in the
command bar, or passing it on the command line. The format is read from the
file's contents, not its extension:

```text
cargo run --release -p workstation_app --bin GenericRadar -- /path/to/volume
```

Start at a stated camera, so a particular view is reproducible without driving
the window by hand:

```text
cargo run --release -p workstation_app --bin GenericRadar -- \
    <volume-file> --zoom 0.12 --center -60,45
```

`--zoom` is kilometres per point; `--center` is `east_km,north_km` from the
radar.

Start a live session directly:

```text
cargo run --release -p workstation_app --bin GenericRadar -- --live KTLX
```

For public real-time Level II, enter a four-character radar identifier such as
`KRTX` and select **Start live**. The source worker installs decode previews,
growing partial volumes, and the immutable completed replacement under the
same history identity.

## What this crate owns

- one, two-horizontal, two-vertical and four-pane layouts, with independent
  per-pane product and tilt selection and optional linked camera groups;
- the product surface: REF, VEL, DVEL, SRV, DSRV, SW, ZDR, CC, PHI, KDP and
  the derived volume products;
- gate filters, cross-sections, the 3D volume pane, the palette editor, the
  settings window and profiles, VROT sampling and warning polygons;
- preview-aware decode, real-time chunk aggregation, atomic caching and
  partial-to-complete replacement;
- immediate pan and zoom by transforming the retained texture while a
  newest-wins exact raster is generated off-thread;
- typed generation guards for source, frame, pane, view and palette state, and
  policy-controlled immutable volume history: unlimited by default for
  operator-selected local playlists, with bounded fallback history for
  unattended live feeds.

## Data sources

The application reads NOAA/NWS NEXRAD Level II from the Unidata-hosted
`unidata-nexrad-level2` and `unidata-nexrad-level2-chunks` buckets, NOAA/NWS
`api.weather.gov`, and basemap imagery from USGS The National Map and
OpenStreetMap. The decoder fixtures under `crates/nexrad_io/tests/data` are
real files from other operators, some under CC BY 4.0. Everything this
software reads or redistributes is credited in
[`DATA-SOURCES.md`](../../DATA-SOURCES.md) at the repository root.

## Architectural rules

- `main.rs` is startup only.
- egui composes state and drains bounded results; it does not download,
  decode, derive, project, tessellate, or rasterize.
- source/site changes invalidate the old session before any late result can
  install.
- map geometry identity never includes exact camera-center or scale bits.
- every queue, cache and texture owner is explicitly bounded; volume history
  follows an explicit retention policy, with unlimited local playlists and
  bounded unattended-live fallback by default.
- the direct-dependency allowlist in `tests/architecture.rs` is the one hard
  boundary; adding to it is deliberate. See `docs/extending.md`.

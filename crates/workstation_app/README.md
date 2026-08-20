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
  byte- and count-bounded immutable volume history.

## Architectural rules

- `main.rs` is startup only.
- egui composes state and drains bounded results; it does not download,
  decode, derive, project, tessellate, or rasterize.
- source/site changes invalidate the old session before any late result can
  install.
- map geometry identity never includes exact camera-center or scale bits.
- every queue, history, cache, and texture owner is explicitly bounded.
- the direct-dependency allowlist in `tests/architecture.rs` is the one hard
  boundary; adding to it is deliberate. See `docs/extending.md`.

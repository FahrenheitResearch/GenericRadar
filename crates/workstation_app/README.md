# GenericRadar

`GenericRadar` is the clean native Rust/egui successor for professional Level II radar analysis. It is intentionally separate from BowEcho.

The complete product and architecture contract is in [`../../docs/ANALYST_WORKSTATION.md`](../../docs/ANALYST_WORKSTATION.md).

## Run

From the repository root:

```text
cargo run --release -p workstation_app --bin GenericRadar
```

Open a local Level II file by dropping it onto the window, entering its path in the command bar, or passing it on the command line:

```text
cargo run --release -p workstation_app --bin GenericRadar -- /path/to/KRTX-volume
```

Start at a stated camera, so a particular view is reproducible without
driving the window by hand:

```text
cargo run --release -p workstation_app --bin GenericRadar -- \
    <level2-file> --zoom 0.12 --center -60,45
```

`--zoom` is kilometres per point; `--center` is `east_km,north_km` from the
radar.

Start a live session directly from the command line:

```text
cargo run --release -p workstation_app --bin GenericRadar -- --live KTLX
```

For public real-time Level II, enter a four-character radar identifier such as `KRTX` and select **Start live**. The source worker installs decode previews, growing partial volumes, and the immutable completed replacement under the same history identity.

## Implemented in the current stacked branch

- one, two-horizontal, two-vertical, and four-pane layouts;
- independent active-pane product and tilt selection;
- optional linked camera groups;
- REF, VEL, DVEL, SRV, DSRV, SW, ZDR, CC, PHI, and KDP;
- preview-aware Level II decode using the existing bounded Rust decoder;
- public real-time Level II discovery, chunk aggregation, atomic caching, and partial-to-complete replacement;
- immediate pan and zoom by transforming the retained texture while a newest-wins exact raster is generated off-thread;
- typed generation guards for source, frame, pane, view, and palette state;
- byte- and count-bounded immutable volume history;
- ready-gated looping that holds instead of flashing an unavailable frame;
- radar-only direct dependency and module-size firewalls.

## Not yet represented as complete

This branch is the working shell and source foundation, not a claim that the complete Analyst Workstation contract is implemented. The accepted contract still requires the retained GPU map/overlay scene, archive browser and backfill, warning/LSR/placefile support, analyst measurements, volume-derived and temporal products, cross-sections, user-defined products, and lit/isosurface 3D analysis.

## Architectural rules

- `main.rs` is startup only.
- egui composes state and drains bounded results; it does not download, decode, derive, project, tessellate, or rasterize.
- source/site changes invalidate the old session before any late result can install.
- map geometry identity never includes exact camera-center or scale bits.
- every queue, history, cache, and texture owner is explicitly bounded.
- BowEcho-only model, satellite, simulation, social, and experimental crates are forbidden as direct dependencies.

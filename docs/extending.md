# Extending the Radar Workstation

This is the white-label plumbing document: how a new capability — a data
provider, a derived product, an overlay, a whole module ported from BowEcho —
plugs into this workspace so that it appears in the application, appears in
the master settings window, persists its state, and passes the gates. It is
written for an agent doing the work: exact seams, exact files, no narrative.

Baseline for this document: the settings system landed with the
`settings` crate rewrite and `workstation_app/src/settings_ui.rs`.

---

## 1. The dependency rules

The workspace is `crates/*`. The one hard architectural boundary is the
**dependency firewall** in `crates/workstation_app/tests/architecture.rs`:
`workstation_app/Cargo.toml`'s `[dependencies]` section must equal the
allowlist in that test. Everything else about module size is judgement — there
is deliberately **no line cap** and one must not be reintroduced (the test's
own header records why).

Consequences, in order of use:

* **A new third-party dependency goes in the crate that owns the capability,
  never in `workstation_app`.** Example: `serde_json` is a dependency of
  `settings`, because the settings file is `settings`' capability; the
  workstation sees only typed values.
* **A new first-party crate** becomes reachable from the application only by
  (a) adding it to `workstation_app/Cargo.toml` **and** (b) adding its name,
  with a comment saying what capability it admits, to
  `ALLOWED_DIRECT_DEPENDENCIES` in `architecture.rs`. Both edits are a
  deliberate owner decision — the firewall exists so this is never a drive-by.
  Precedent inside the allowlist: `product_engine` ("product meaning is
  declared once"), `rayon` (admitted "deliberately, and narrowly").
* A crate that only *feeds* an existing capability goes behind the crate that
  owns the seam instead: `basemap_tiles` is not in the allowlist — it is
  re-exported through `map_scene` (`map_scene/src/lib.rs`: "the application
  never names `basemap_tiles`").
* The `settings` crate depends on `serde`/`serde_json` **only**. It sits at
  the bottom of the workspace precisely so any crate can depend on it to
  declare settings without a cycle. Do not add workspace crates to its
  `[dependencies]` (dev-dependencies for its test harness are fine — the
  firewall reads only `[dependencies]`).

---

## 2. Declaring settings (the part that makes white-labelling real)

Settings are **contributed, not centrally enumerated**. A category is plain
data — `(id, label, list of typed items with ids, labels, ranges, defaults)` —
and the master settings window renders whatever was contributed. There are no
trait objects and no macros anywhere in this path.

### 2.1 The types (`crates/settings/src/registry.rs`)

```rust
settings::SettingsCategory::new("mycrate", "My Feature", vec![
    settings::SettingSpec::new(
        "update_seconds",
        "Update interval",
        settings::SettingKind::Slider {
            min: 5.0, max: 600.0, default: 60.0, decimals: 0,
            unit: "s".to_owned(),
        },
    )
    .help("One or two sentences. Shown inline under the control - never \
           hover-only, because hover does not exist on glass."),
])
```

`SettingKind` variants: `Toggle { default }`, `Slider { min, max, default,
decimals, unit }`, `Integer { min, max, default, unit }`, `Choice { options,
default_id }`, `Text { default, placeholder, max_len }`. Every kind carries
its default; `SettingKind::sanitize` resolves any stored value (missing,
malformed, out of range, unknown choice id) to something usable — resolution
is total and can never blank a pane.

`SettingSpec::pending_wiring()` marks an item that is *declared* — id, range
and default are the contract, the stored value survives — but whose owning
code does not read it yet. The window draws it disabled with an honest note.
Use it when a setting should exist ahead of its feature (`map/range_rings`
and `data/live_cache_limit_mb` are current examples).

### 2.2 How a crate contributes

1. In your crate: `pub fn settings_category() -> settings::SettingsCategory`
   (plain function, plain data; your crate adds `settings = { path =
   "../settings" }` to its own `[dependencies]`).
2. At the collection point — `workstation_app/src/settings_ui/catalog.rs`,
   `registry()` — add one line: `registry.register(mycrate::settings_category());`.
   That is the only application edit. The window has **zero** per-setting
   code; your items render, search, reset and persist immediately.
   Registering an existing category id merges items into that page, so a
   crate can extend "Map" instead of creating "Map 2".
3. Values persist automatically under `(category id, setting id)` in the
   settings file. **Ids are the persistence contract**: never reuse an id for
   a different meaning; renaming one orphans the old value (safe) but loses
   the user's choice.

### 2.3 How values reach your code

Your crate does **not** read the store. The composition root (`app.rs`) does,
and pushes plain values into your API — the same way `render2d` receives a
`DisplayQuality` and `analyst_runtime::VolumeHistory` receives a
`HistoryPolicy` today:

```rust
// app.rs, on outcome.changed containing ("mycrate", "update_seconds"):
let seconds = self.settings_store.effective_float(&self.settings_registry,
    "mycrate", "update_seconds");
self.my_service.set_update_interval(Duration::from_secs_f64(seconds));
```

`draw_settings_window` returns `SettingsOutcome { changed, palette_changed }`
each frame; `changed` lists `(category, id)` pairs to apply. Design your
crate so every setting has a setter or is a constructor parameter, and the
wiring stays one line per setting.

### 2.4 Scalar knobs vs structured state

* Scalars (numbers, toggles, choices, short text) → registry values, above.
* Structured session state (per-pane products, cameras, palettes, window
  geometry) → `settings::WorkspaceSnapshot`, a serde struct with
  string-vocabulary fields, every field optional, unknown fields preserved.
  Conversions between live types and the snapshot live in
  `workstation_app/src/settings_ui/sync.rs` (workspace) and
  `settings_ui/palettes.rs` (colour tables). Follow those two files' pattern:
  capture never fails, apply resolves every string defensively and falls back
  to current behaviour.

### 2.5 What the store guarantees (so you do not re-implement it)

`settings::SettingsStore` (`crates/settings/src/store.rs`):

* **Forward/backward compatible.** A file written by a future build round-trips
  through this one intact: maps carry unknown categories/ids verbatim, every
  struct has a `#[serde(flatten)] unknown` catch, a higher `version` is
  preserved on save. Proven in `crates/settings/tests/store_proof.rs`.
* **Crash-safe.** Writes go to a temp sibling, `sync_all`, then rename.
* **Debounced.** `autosave_tick()` once per frame; writes after 2 s of quiet
  or 20 s of continuous change. Never save per frame; `set()` with an
  unchanged value does not even mark dirty, so mirroring live state into the
  store every frame is free.
* **Corruption-tolerant.** An unparseable file is moved to
  `settings.json.corrupt`, defaults apply, the next save writes fresh. An
  *unreadable* path disables autosave rather than overwrite what it could
  not read.
* **Injectable location.** `settings::default_settings_file()` resolves per
  platform; an iOS/Android shell calls `settings::set_app_config_root` /
  `set_app_cache_root` once before the UI starts. Never hardcode a desktop
  path inside a crate — take the directory as a parameter and let the
  composition root pass `settings::app_cache_root()`.

---

## 3. Getting a product in front of the user

Product *meaning* is declared once, in `product_engine` (`registry.rs`:
`ProductDescriptor` — id, aliases, short name, units/domain, source moment,
cut policy, computation). The panes hold a `Copy` handle:
`workstation_app/src/product.rs::DisplayProduct`, an adapter its own header
calls temporary.

To add a product today:

1. Declare the descriptor in `product_engine`'s registry (units, domain,
   moment, computation). Cite primary literature in the descriptor or the
   computation for anything with a research basis — that is a standing rule,
   see `vrot.rs` (Thompson et al. 2017) and `color_tables` (Ottosson 2020)
   for the expected form.
2. Add the variant to `DisplayProduct` and its `ALL`/`try_from_product_id`
   mapping (`product.rs`).
3. Availability comes from `VolumeCapabilities` measurement — wire nothing;
   the picker greys out what the volume cannot show.
4. Colours: pick the `color_tables::ColorTableFamily` the product reads
   (`palettes::table_for`). A genuinely new measurement family is a
   `color_tables` change: new family + builtin tables + domain, and the
   settings palette section picks it up from `ColorTableFamily::ALL`
   automatically.
5. Persistence: nothing to do. Pane products persist as registry-id strings;
   on load `app.rs` resolves them through `try_from_product_id` and resets
   unknown ids to the default **with a visible status line**.

## 4. Getting a pane overlay or map layer in front of the user

* Per-pane radar overlays (legend, badges, probe) enter through
  `pane_canvas::PaneOverlay`; map-anchored geometry (sites, warning polygons)
  through `pane_canvas::PaneMap`, projected once per anchor change in
  `app.rs` (`refresh_placed_sites` / `refresh_placed_hazards` are the
  pattern: project off the paint path, hand `Arc<[T]>` to the pane).
* Basemap-level layers belong in `map_scene` (styles/presets in
  `style_presets.rs`, raster underlay via the `TileProvider` seam).
* A visibility toggle for your overlay is a settings `Toggle` (§2) plus one
  line in `app.rs` choosing whether to hand the pane your `Arc` or an empty
  one — `show_warnings` is the existing example of exactly this shape.

## 5. The gates

Every change, before it is claimed done (all release; never pipe cargo
through `tail`/`head` — redirect to a file, check `$?`, read the file):

```
cargo test  --release -p <your crates>
cargo clippy --release -p <your crates> --all-targets -- -D warnings
rustfmt --edition 2024 --check <your files>
```

Plus the standing rules that are not commands:

* **Real data.** Prove behaviour on real Level II volumes (the live cache
  under `FahrenheitResearch/RadarWorkstation/cache/level2-live`) or, for UI,
  by rendering the real thing and looking at it (the settings window has
  `crates/settings/examples/settings_preview.rs` for exactly this). Synthetic
  fixtures are an extra unit check, never the proof.
* **Mobile.** No hover-only affordances (help is inline text), hit targets
  ≥ 24 pt (`settings_ui::MIN_INTERACT_HEIGHT` is the existing floor), no
  hardcoded desktop paths (§2.5), no continuous repaint (repaint on input or
  on `request_repaint_after` with a reason).
* **Line endings are per-file.** This tree mixes CRLF and LF; scripts that
  patch files must detect and preserve. Never bulk-rewrite files with
  PowerShell `Get-Content`/`Set-Content`.
* The firewall test (§1) runs with `-p workstation_app`; if your change adds
  a workstation dependency without the allowlist comment, the gate is red by
  design.

## 6. Current temporary scaffolding (delete-on-wiring)

Until the human-owned `mod settings_ui;` wiring lands in
`workstation_app/src/main.rs`, the settings window is compiled and tested via
`crates/settings/tests/workstation_settings_ui.rs` (a `#[path]` include of
the real source) and photographed via
`crates/settings/examples/settings_preview.rs`. When the wiring lands:

* keep the example (it remains the fastest way to photograph the window),
* delete the harness test **or** leave it as a second compile — but if it is
  deleted, move its three proof tests into `settings_ui`'s own `#[cfg(test)]`
  modules first,
* do not "simplify" the explicit `#[path = "settings_ui/…"]` child-module
  attributes in `settings_ui.rs`; the comment above them says why they
  resolve identically in both homes.

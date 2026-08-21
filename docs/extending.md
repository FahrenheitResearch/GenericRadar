# Extending GenericRadar

How a new capability — a data
provider, a derived product, an overlay, a whole module ported from BowEcho —
plugs into this workspace so that it appears in the application, appears in
the master settings window, persists its state, and passes the gates. It
gives exact seams and exact files, and no narrative.

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
  deliberate maintainer decision — the firewall exists so this is never a drive-by.
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

## 2. Declaring settings

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

`SettingSpec::group("…")` puts an item under a subsection heading, so a long
page reads as structure rather than as a wall:

```rust
SettingSpec::new("opacity", "Opacity", …).group("How the volume is drawn"),
SettingSpec::new("density", "Density", …).group("How the volume is drawn"),
SettingSpec::new("show_grid", "Height grid", …).group("Annotations"),
```

Sections are **runs of consecutive items** carrying the same heading
(`SettingsCategory::sections`), so declaration order is the order on screen
and nothing is silently reordered. Two rules the catalog's own tests enforce,
and yours should keep: group *all* of a page's items or none of them (an
ungrouped item above the first heading has nothing naming it), and do not use
one heading in two separate runs (it draws twice and reads as a mistake).
Headings are presentation only — they are never stored, and renaming one
loses nothing. A page that declares no headings renders exactly as it did
before headings existed; a test over every ungrouped page in the real catalog
compares the two shape-for-shape.

Short pages should stay ungrouped. A heading over three toggles is noise.

### 2.2 How a crate contributes

1. In your crate: `pub fn settings_category() -> settings::SettingsCategory`
   (plain function, plain data; your crate adds `settings = { path =
   "../settings" }` to its own `[dependencies]`).
2. At the collection point — `workstation_app/src/settings_ui/catalog.rs`,
   `registry()` — add one line: `registry.register(mycrate::settings_category());`.
   That is the only application edit. The window has **zero** per-setting
   code; your items render, search, group under their headings, carry a
   modified mark with their own Reset, join their page's reset and the
   whole-application one, travel in an export and come back through an
   import, and persist — immediately, with nothing written here.
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

## 3b. The user colour table folder

Analysts bring their own palettes. GenericRadar reads them out of one folder,
resolved from the same root the settings file uses:

```
<settings folder>/colortables/
```

* Windows: `%LOCALAPPDATA%\FahrenheitResearch\RadarWorkstation\colortables`
* Linux/BSD: `$XDG_CONFIG_HOME/radar-workstation/colortables`
  (`~/.config/...` when unset)
* macOS/iOS: `~/Library/Application Support/RadarWorkstation/colortables`
  — on iOS the shell has already injected the sandbox root, so this follows
  it automatically.

Never spell any of that out in code, and never derive it a second time
either: it is `settings::user_colortables_dir()`, one function, and both
front doors on to it — `workstation_app/src/user_tables.rs::user_tables_dir`
for the scanner and `palette_editor::store::user_colortables_dir` for the
editor — are one line over it. It hangs off `settings::app_config_root()`, so
an injected root moves the palettes with the rest of the application's state.
**Anything that writes a palette for this application to read (the colour
table editor, an importer, a sync job) writes it here, and the next scan
picks it up.** The folder is created on demand: it does not exist until the
first table lands in it, and a missing folder is not an error anywhere.

What the reader does (`color_tables::user`, `workstation_app/src/user_tables.rs`):

* every `*.pal` and `*.txt` in the folder is parsed with
  `ColorTable::parse` — the GR2Analyst/RadarScope dialect, no conversion
  step;
* the file's `Product:` header routes it to a `ColorTableFamily`
  (`BR`/`REF` → Reflectivity, `BV`/`VEL` → Velocity, `SW`, `ZDR`,
  `CC`/`RHO`, `PHI`, `KDP`; case- and punctuation-insensitive). A missing or
  unrecognised header lands in `Generic` rather than being refused;
* the table's name is the `Name:` row **inside** the file, falling back to
  the file stem for a hand-dropped GR palette, which never carries the row.
  Never the filename a name would produce: that mapping is lossy and
  many-to-one. This is one rule — `color_tables::files::palette_identity` —
  shared with `color_tables::palette_named_in`, the search the colour table
  editor and the launch-time restore both call, so a row the picker offers is
  a row that resolves;
* that name is suffixed `" (user)"` when this build cannot carry it as an
  analyst's own — it is a shipped palette's base name in the same family, or
  it ends in a rendering suffix — and numbered when two files **in the same
  family** would otherwise share a name. The numbering is per family, so a
  reflectivity `Mine.pal` and a velocity `Mine.txt` are both offered as
  "Mine" — they never share a picker list. **Which names those are is one
  function, `color_tables::user_palette_name_fault`**, and the colour table
  editor asks the same one before it writes a file: a refusal where there is
  still somebody on screen to tell, this rename where there is not. Change
  the rule in one place or the editor writes files this scan renames out from
  under the name the settings file stored;
* a symlink is followed, so `ln -s ~/Dropbox/palettes/Mine.pal
  <config>/colortables/` is a perfectly ordinary way to keep palettes in
  sync. The listing describes the file at the far end of the link, so an
  edit made over there is an edit this folder sees;
* a file that does not parse is **skipped, never silent**: it is listed with
  its name, the parser's reason and the line number under
  *Settings → Radar → Your colour tables*. So is a file over 2 MB, which is
  not read at all — `.txt` is admitted because shared palettes wear it, and
  a stray 50 MB pile of notes in that folder must not be parsed on the UI
  thread. Its fault row names its size. So is anything past the **8 MB one
  scan reads in total**: the per-file cap alone bounds N × 2 MB and nothing
  more, and twenty files that are each just legal are a third of a second of
  frozen window. The budget is spent in name order, so the same files load
  every time, and the ones it turns away say which budget they ran into;
* the folder is re-read at startup, whenever the window regains focus, after
  a drop, and one frame after the colour table editor saves — a save made
  inside the window is a change the focus rescan can never see, because focus
  was never lost. There is no watcher thread and no polling. A rescan whose
  directory listing (name, length, modification time per entry) matches the
  last one stops there: an untouched folder costs one listing and zero
  parses, so an alt-tab is never a pause;
* **what a listing can and cannot see**, which is the same problem `git` has
  with its index and gets the same answer. A filesystem stamps a file from a
  clock that moves in steps (~15 ms on NTFS, whole seconds on HFS+), so a
  save that lands in the step the scan ran in carries a stamp the scan
  cannot order against itself: any entry stamped within a second of its own
  scan is therefore distrusted and re-read next time. What survives that is
  one case — a change that keeps the byte count **and** carries a
  deliberately back-dated timestamp, which is what a timestamp-preserving
  copy does (`robocopy` by default, `rsync -t`, an archive extraction). That
  is documented rather than papered over, and the *Rescan colour table
  folder* button is the way out: it never looks at the listing and always
  re-reads every file.

Dropping a `.pal` (or `.txt`) on the window copies it into the folder and
loads it immediately, answering in a floating notice — the table's name and
family, or the parser's reason and line. A dropped file that does not parse
is reported and **left where it was**; it is never filed and never
overwrites anything, and a name already taken in the folder gets a numbered
sibling. A drop past the 2 MB cap is turned away on its size before its
bytes are read at all — a mis-dragged half-gigabyte file costs one
`metadata` call rather than half a gigabyte on the UI thread — and it is
told it is *too large for this build*, naming the cap, rather than that it is
not a colour table: a 3 MB palette in a dialect this build reads fluently is
a colour table, and a message that says otherwise sends the analyst hunting
for a fault that is not there. A drop whose *bytes* are already in the folder files nothing and
answers "already imported as …", so re-dropping a palette you have not
edited cannot fill the picker with copies of one table.

Persistence is by name (`settings_ui/palettes.rs`). A stored name resolves
against the shipped catalogue first and the folder second — through the
running scan when there is one, and otherwise through
`color_tables::palette_named_in`, the same search the editor uses to find the
file behind a table it has been asked to edit. Both read the folder in one
order and identify a file by one rule, so where both answer they answer with
the same file. A name nothing can
resolve falls back to the family default on screen **and is kept in the
settings file**, because the file may come back — so a drive that had not
mounted yet does not cost the analyst their palette.
`docs/palettes/Sample Ramp-Pair Velocity.pal` is a worked example of the
format — original colours in a borrowed dialect, which is the rule for
anything shipped in that folder: name a file after the format it is written
in, never after somebody else's product;
`cargo run --release -p workstation_app --example user_table_proof --
<level2-file> <out-dir>` drives one through the whole chain on a real volume
and writes the two frames to compare.

## 3c. The colour table editor

The other end of the same folder. `workstation_app/src/palette_editor` is a
full editor for the dialect §3b reads, opened from a per-row affordance in the
product picker and from *Settings → Radar → Colour tables*, and it writes
`.pal` files into the folder §3b scans. Four modules: `model` (what a table
is, and the two crossings to `color_tables::ColorTable`), `pal` (reading a
file back, including the `Scale:` row a `ColorTable` applies and forgets),
`store` (where files go and the round-trip check a save must pass), `ui` (the
window).

Three things hold the two features together, and each is one function rather
than two agreeing implementations:

* **one folder** — `settings::user_colortables_dir()`; see §3b;
* **one identity** — the `Name:` row inside a file, falling back to its stem
  (`color_tables::files::palette_identity`). The scanner names its rows with
  it and `color_tables::palette_named_in` resolves with it, so Edit opens the
  file the next launch installs;
* **one name policy** — `color_tables::user_palette_name_fault`. The editor
  refuses to save under a name that ends in a rendering suffix or that a
  shipped palette already holds, because such a file is written perfectly and
  the palette is gone at the next launch; the scanner applies the same rule to
  a file already on disk by renaming the row it offers.

Which rows offer *Edit* and which offer *Copy* is the caller's answer, not the
editor's: `color_tables::is_builtin_table` — cached with the picker's rows in
`settings_ui::PaletteOfferCache` — says whether this build ships the palette.
A shipped preset is duplicated under a free name and claims no file; anything
else is edited in the file whose `Name:` row matches it. There is no code path
in the editor that writes over a preset.

Save reports its path back to the application, which re-reads the folder one
frame later (§3b), so a table an analyst has just written is in the picker
immediately rather than after an alt-tab.

`cargo run --release -p workstation_app --example palette_editor_proof --
<level2-file> <out-dir>` drives the editor end to end on a real volume:
edit-and-repaint, unit round trips, a shared GR palette opened and re-saved
pixel-identically, and the real window photographed in both themes.

## 3d. Adding a theme

A theme is **data**: one value, in one file, registered on one line.

1. **Write the file.** `crates/workstation_app/src/theme/<id>.rs`, where
   `<id>` is the theme's stable id with hyphens written as underscores
   (`amber_crt.rs` for id `amber-crt`). Copy `light.rs` or `dark.rs` — they
   are the two worked examples — and change the values. The module exposes
   exactly one item:

   ```rust
   pub const THEME: ThemeSpec = ThemeSpec {
       id: "amber-crt",           // stable, persisted, never reused
       label: "Amber CRT",        // what the settings list shows
       description: "…",          // one clause about what it is FOR
       ground: Ground::Dark,      // seeds egui's own mode defaults
       palette: Palette { … },    // all 20 roles, no inheritance
   };
   ```

   The 20 roles, each documented in `theme/palette.rs`: `face`,
   `face_raised`, `face_pressed`, `hover`, `well`, `text`, `text_weak`,
   `text_disabled`, `border`, `border_strong`, `link`, `selection_bg`,
   `selection_text`, `selection_tint`, `warn`, `error`, `hi_outer`,
   `hi_inner`, `sh_inner`, `sh_outer`.

2. **Register it.** One line in the `catalog!` list in
   `crates/workstation_app/src/theme/catalog.rs`, in alphabetical order:

   ```rust
   catalog! {
       amber_crt,
       dark,
       light,
   }
   ```

   That is the whole registration. The settings page derives its options
   from the catalog, the contact sheet photographs the catalog, and the
   contrast audit measures the catalog — none of them are edited.

3. **Pass the audit.** `cargo test --release -p workstation_app --test
   theme_catalog` measures every registered theme crossed with every accent
   against every pairing the chrome paints, and names the theme, the accent,
   the pairing and the ratio it failed on. The floors and their WCAG
   citations are in that file's module docs.

4. **Look at it.**

   ```text
   cargo run --release -p workstation_app --example theme_gallery --        --volume <level2-file>
   ```

   writes `sheet_<id>.png` — the whole chrome plus a real radar pane — for
   every registered theme, plus `gallery_<id>_1x.png` / `_2x.png` at both
   device scales. A theme nobody has looked at over real echo is not done.

The four customization axes (`theme::Accent`, `Density`, `ChromeEdges`,
`UiScale`) are orthogonal to a theme and a theme author does not touch them.
An accent supplies four roles per ground and is added to `Accent` in
`theme/appearance.rs`; the same audit measures it against every theme.

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
* **Line endings are LF.** `.gitattributes` sets `* text=auto eol=lf` for the
  whole tree; do not commit CRLF, and prefer editors and scripts that leave
  the ending alone rather than rewriting a whole file to change one line.
* The firewall test (§1) runs with `-p workstation_app`; if your change adds
  a workstation dependency without the allowlist comment, the gate is red by
  design.

## 6. The settings window is compiled in two homes

`workstation_app/src/settings_ui.rs` is linked into the binary by
`mod settings_ui;` in `workstation_app/src/main.rs`. It is ALSO compiled a
second time by `crates/settings/tests/workstation_settings_ui.rs` (a `#[path]`
include of the same source) and photographed by
`crates/settings/examples/settings_preview.rs`. That is deliberate, and if you
touch the settings window:

* keep the example — it remains the fastest way to photograph the window,
* keep the harness test, or if you delete it, move its three proof tests into
  `settings_ui`'s own `#[cfg(test)]` modules first. The harness is what pins
  that the settings window reaches back into nothing in `workstation_app`; a
  change that breaks that rule fails there before it fails anywhere else,
* do not "simplify" the explicit `#[path = "settings_ui/…"]` child-module
  attributes in `settings_ui.rs`; the comment above them says why they
  resolve identically in both homes.

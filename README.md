# GenericRadar

A native NEXRAD Level II radar workstation, which also reads ODIM_H5, DORADE,
CfRadial and mobile deployment archives. Rust, egui, wgpu. No accounts, no
API keys, no telemetry.

## Features

- Live Level II via the NEXRAD real-time chunk feed, with per-radial sweep
  updates and automatic backfill of the previous complete volume
- Optional automatic following of arriving low-elevation sweeps, including
  in-progress supplemental scans revealed radial by radial. Maximum elevation,
  minimum sweep-update interval, and live-feed polling cadence are independently
  adjustable
- Live surface-station observations plotted directly over the radar with
  temperatures, dewpoints, wind barbs, cloud cover, present weather and
  station history
- Local and remote GR-compatible placefiles, including text, icons, lines,
  polygons, refresh intervals and independently controlled map overlays
- Files routed by content rather than by extension: NEXRAD Archive II,
  including the legacy Message 1 that every volume before 2008 is written in;
  ODIM_H5 polar volumes; DORADE sweepfiles and mobile deployment zips
  (DOW, COW, RaXPol, NOXP); CfRadial 1.x in either container, classic
  netCDF or netCDF-4/HDF5; GR2Analyst-style `.msg31` exports; and Vaisala
  RVP8/RVP900 Level 1 time series; plus MATLAB Level 5/v7 OU-PRIME I/Q cubes,
  including a gzip wrapper
- An in-app browser for NOAA/NSSL's public KOUN RVP Level 1 archive, with
  background download progress, cancellation, non-overwriting writes to
  Downloads, and optional open-after-download
- Multiple local files can be selected or dropped together. Operator-selected
  local playlists are unlimited by default; a background preflight estimates
  decoded RAM and asks **Continue** or **Cancel** above 16 GiB rather than
  refusing the load. Optional frame and RAM limits remain available;
  unattended live feeds retain a 30-frame/1-GiB fallback when those settings
  are zero
- Files decode in filename order into a timeline ordered by radar volume time.
  A bad file does not stop the rest, different selected files with the same
  radar and time remain distinct, and data from different radar positions is
  never silently combined
- One-cut Archive II/Message 31 research exports assemble into one logical
  multi-elevation volume only when their internal volume sequence, radar
  identity, exact recorded position, acquisition date/VCP and radial timing
  agree. Ambiguous files stay independent; the same conservative assembly
  applies inside mobile deployment ZIPs
- **Export current view** writes the fully composited application window as a
  non-overwriting PNG directly to Downloads
- **Export loop** writes the complete radar timeline as a smoothly timed,
  infinitely looping GIF directly to Downloads, preserving the visible map,
  overlays, legends and station observations. Adaptive color quantization,
  duplicate-frame coalescing, changed-region cropping and LZW compression keep
  radar colors accurate and files compact without external encoders
- Up to four linked panes: reflectivity, velocity (dealiased and
  storm-relative), spectrum width, ZDR, CC, PHI, KDP, and derived products
  (CREF, ET, VIL, VILD, MESH, POH/POSH)
- A dedicated DOW dual-frequency group models DBMH1/2/M, DBMV1/2/M,
  DBZH1/2/M and DBZV1/2/M when available, keeping received power in dBm and
  equivalent reflectivity in dBZ
- DORADE, CfRadial and ODIM volumes also expose exact producer-native fields
  under **SOURCE FIELDS FROM THIS FILE**. Names, descriptions, unit tokens and
  observed ranges come from the file rather than inferred semantics; exact
  source fields are 2D-only, and unsupported 3D/cross-section views say so
  instead of substituting a modeled product
- Gate filters on the 2D panes: a reflectivity floor, velocity gated on its
  companion reflectivity sweep, RhoHV censoring, range-folded gates, and a
  near-range cutoff, with presets. A pane that is hiding gates says so on its
  header and says what is hidden; the toolbar's filter chip carries a clear
  key that shows everything again
- Cross sections along a user-drawn line, with physically honest beam
  coverage (Doviak & Zrnić 1993; Zhang et al. 2005, 2011)
- GPU volumetric 3D with reflectivity-dependent opacity, orbit and fly cameras
- Vector basemap in a geodesic radar-centred projection, transitioning to an
  orthographic globe at far zoom; beyond the radar's own surveillance range
  the view is turned to put north up at the middle of the pane
- Optional raster imagery: USGS The National Map and OpenStreetMap
- Colour tables render continuous (Oklab) or stepped and are switchable and
  editable per palette. Exact source fields start in automatic observed-range
  display and support separate field-specific colours and fixed endpoints;
  **Apply** is session-only unless the matching edit is saved, and **Reset to
  observed range** restores automatic display
- Eight themes, with interface scale, density, accent colour, and bevelled or
  flat chrome chosen separately; every theme holds 7:1 body-text contrast
  (WCAG AAA), and one is drawn so that all of its text does
- Settings that persist, with search across every page, a mark on whatever
  differs from the shipped default, per-setting and per-page reset,
  import/export, and named profiles to switch between
- Distance, altitude, time zone and clock format chosen once and applied to
  every readout
- Level 1 (I/Q) time series processed in the application: reflectivity,
  velocity, spectrum width and RhoHV estimated from the pulses themselves by
  pulse-pair processing, with the dwell length, the window (rectangular, von
  Hann, Hamming, Blackman) and the SNR threshold under the analyst's control,
  and a Doppler spectrum for a chosen gate
- Calibration-free OU-PRIME cubes are kept honest: the application shows
  relative stored-I/Q power rather than fabricated dBm or dBZ, and does not
  invent an SNR threshold. Their measured 32-pulse ray boundaries fix the
  dwell length; the Window control still re-estimates the field, while the
  preferred-dwell and SNR sliders intentionally cannot change that source
- A file browser drawn in the application's own chrome, on every platform,
  which identifies each file by reading it rather than by its name - so a
  volume stored with no extension still says what it is
- VROT sampling, NWS warning polygons

## Surface observations and placefiles

Open **Layers** to enable surface observations or manage placefiles. Station
models use measured METAR values: red temperature, green dewpoint, a standard
wind barb, cloud cover and the reporting station's identifier. Click a station
to inspect its historical reports. **Settings → Surface observations** controls
individual fields, refresh cadence, station spacing, units and optional
supplemental mesonet networks.

The placefile manager accepts either a local file or an HTTP(S) URL. Sources
can be enabled, refreshed and removed independently, and their configured
visibility persists across restarts. GR-compatible text, icons, lines,
polygons and time ranges are drawn on the radar map; remote files and icon
sheets are fetched in the background.

## Build

```
cargo build --release -p workstation_app --bin GenericRadar
```

Rust 1.94+, edition 2024. No configuration required.

Adding a data provider, a derived product or an overlay: see
[`docs/extending.md`](docs/extending.md).

## Run

```
GenericRadar --live KTLX
```

or open a radar file from the File menu - browse for it, type its path, or
drop it on the window. The format is read from the file's contents, never
from its name.

## Data sources

- NEXRAD Level II: NOAA/NWS, read from the Unidata-hosted
  `unidata-nexrad-level2` and `unidata-nexrad-level2-chunks` buckets
- Warnings and site metadata: NOAA/NWS api.weather.gov
- Surface observations: NOAA Aviation Weather Center; historical reports and
  supplemental station networks: Iowa Environmental Mesonet
- Imagery: USGS The National Map (public domain);
  © OpenStreetMap contributors, under the Open Database License (ODbL)
- Research Level 1 downloads: NOAA/NSSL KOUN THREDDS archive. Research-radar
  I/Q holdings are distributed among operators; this browser is not presented
  as a universal archive.

This repository also redistributes real radar files as decoder test fixtures,
including material used under CC BY 4.0 from EUMETNET OPERA / AEMET, EUMETNET
OPERA / SMHI and NOAA. Every one of them, and the terms it is carried under,
is listed in [`DATA-SOURCES.md`](DATA-SOURCES.md).

## License

MIT or Apache-2.0, at your option.

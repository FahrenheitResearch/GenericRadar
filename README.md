# GenericRadar

A native NEXRAD Level II radar workstation, which also reads ODIM_H5, DORADE,
CfRadial and mobile deployment archives. Rust, egui, wgpu. No accounts, no
API keys, no telemetry.

## Features

- Live Level II via the NEXRAD real-time chunk feed, with per-radial sweep
  updates and automatic backfill of the previous complete volume
- Files routed by content rather than by extension: NEXRAD Archive II,
  including the legacy Message 1 that every volume before 2008 is written in;
  ODIM_H5 polar volumes; DORADE sweepfiles and mobile deployment zips
  (DOW, COW, RaXPol, NOXP); CfRadial 1.x; GR2Analyst-style `.msg31` exports
- Up to four linked panes: reflectivity, velocity (dealiased and
  storm-relative), spectrum width, ZDR, CC, PHI, KDP, and derived products
  (CREF, ET, VIL, VILD, MESH, POH/POSH)
- Gate filters on the 2D panes: a reflectivity floor, velocity gated on its
  companion reflectivity sweep, RhoHV censoring, range-folded gates, and a
  near-range cutoff, with presets. A pane that is hiding gates says so and
  says what is hidden; clicking that band shows everything again
- Cross sections along a user-drawn line, with physically honest beam
  coverage (Doviak & Zrnić 1993; Zhang et al. 2005, 2011)
- GPU volumetric 3D with reflectivity-dependent opacity, orbit and fly cameras
- Vector basemap in a geodesic radar-centred projection, transitioning to an
  orthographic globe at far zoom
- Optional raster imagery: USGS The National Map and OpenStreetMap
- Colour tables rendered continuous (Oklab) or stepped, switchable per palette
- Eight themes, with interface scale, density, accent colour, and bevelled or
  flat chrome chosen separately; one theme holds 7:1 text contrast (WCAG AAA)
  with colour-universal accents
- Settings that persist, with search across every page, a mark on whatever
  differs from the shipped default, per-setting and per-page reset,
  import/export, and named profiles to switch between
- Distance, altitude, time zone and clock format chosen once and applied to
  every readout
- VROT sampling, NWS warning polygons

## Build

```
cargo build --release -p workstation_app --bin GenericRadar
```

Rust 1.94+, edition 2024. No configuration required.

## Run

```
GenericRadar --live KTLX
```

or open a radar file from the File menu; the format is read from its contents.

## Data sources

- NEXRAD Level II: NOAA National Weather Service
- Warnings and site metadata: api.weather.gov
- Imagery: USGS The National Map (public domain);
  © OpenStreetMap contributors (openstreetmap.org/copyright)

## License

MIT or Apache-2.0, at your option.

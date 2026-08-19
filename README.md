# GenericRadar

A native NEXRAD Level II radar workstation. Rust, egui, wgpu. No accounts, no
API keys, no telemetry.

## Features

- Live Level II via the NEXRAD real-time chunk feed, with per-radial sweep
  updates and automatic backfill of the previous complete volume
- Up to four linked panes: reflectivity, velocity (dealiased and
  storm-relative), spectrum width, ZDR, CC, PHI, KDP, and derived products
  (CREF, ET, VIL, VILD, MESH, POH/POSH)
- Cross sections along a user-drawn line, with physically honest beam
  coverage (Doviak & Zrnić 1993; Zhang et al. 2005, 2011)
- GPU volumetric 3D with reflectivity-dependent opacity, orbit and fly cameras
- Vector basemap in a geodesic radar-centred projection, transitioning to an
  orthographic globe at far zoom
- Optional raster imagery: USGS The National Map and OpenStreetMap
- Colour tables rendered continuous (Oklab) or stepped, switchable per palette
- Persistent settings, VROT sampling, NWS warning polygons

## Build

```
cargo build --release -p workstation_app --bin GenericRadar
```

Rust 1.94+, edition 2024. No configuration required.

## Run

```
GenericRadar --live KTLX
```

or open a Level II archive file from the File menu.

## Data sources

- NEXRAD Level II: NOAA National Weather Service
- Warnings and site metadata: api.weather.gov
- Imagery: USGS The National Map (public domain);
  © OpenStreetMap contributors (openstreetmap.org/copyright)

## License

MIT or Apache-2.0, at your option. Copyright (c) 2026 Fahrenheit Research.

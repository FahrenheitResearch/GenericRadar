# Data sources

Every body whose data this application reads, or whose data this repository
redistributes. It is one file, at the root, because attribution has to reach
the people who receive the software: a credit in a `#[cfg(test)]` doc comment,
or in a README that the release process rewrites, is a credit nobody is ever
shown.

This file ships verbatim in the public release snapshot. It is not patched, not
replaced and not stripped, and `docs/RELEASING.md` says so; the tests named at
the bottom fail if a fixture arrives without a decision, or if the credits here
stop matching the code.

## Read live

- **NEXRAD Level II** - collected, quality-controlled and published by
  **NOAA/NWS**. The copies this application actually fetches are the
  **Unidata**-hosted public buckets `unidata-nexrad-level2` (archive) and
  `unidata-nexrad-level2-chunks` (in-progress volumes), operated by
  UCAR/Unidata. Both credits are load-bearing and neither substitutes for the
  other: NWS produces the data, Unidata pays for and serves the objects every
  request here lands on.
- **KOUN Level 1 / I/Q** - raw RVP8/RVP900 time-series records published by
  **NOAA/NSSL** through the public KOUN THREDDS catalog at
  `data.nssl.noaa.gov/thredds/catalog/RRDD/KOUN`. The application browses that
  machine-readable hierarchy and downloads only validated `KOUN_RVP` objects
  from the same fixed HTTPS host. This is one official archive, not a claim
  that research-radar Level 1 holdings are centralized.
- **Watches, warnings and advisories** - NOAA/NWS `api.weather.gov`.
- **Surface aviation weather observations** - METAR reports published by the
  **NOAA/NWS Aviation Weather Center** through its public aviation-weather
  data service.
- **Surface-observation history and supplemental station networks** -
  historical ASOS/AWOS reports and environmental station observations served
  by the **Iowa Environmental Mesonet**, operated by Iowa State University.
  The application identifies station observations as reports, not forecasts.
- **User-supplied placefiles** - local files or remote endpoints chosen by the
  operator. Their contents, attribution requirements and terms belong to each
  selected provider; no third-party placefile data is redistributed here.
- **Basemap tiles** - **USGS The National Map** (each service's own credit
  string is displayed in the application, verbatim as that service publishes
  it) and **© OpenStreetMap contributors**, available under the Open Database
  License (ODbL).

## Redistributed in this repository

The repository carries real radar files and two real map tiles, kept verbatim
or as byte-exact head excerpts, so the decoders are pinned against operational
writers rather than against fixtures this project wrote itself. They ship with
the application: none of these directories is stripped from the release
snapshot.

### `crates/nexrad_io/tests/data`

2.30 MB in total, of which 2.19 MB is somebody else's data; the remaining
116 KB is this repository's own synthetic ODIM volumes and the Python scripts
that write them, which are nobody else's and are credited to nobody.

- **RMI Belgium** (Royal Meteorological Institute of Belgium) - Jabbeke
  (`bejab.pvol.hdf`, WMO 06410) and Wideumont
  (`20130429043000.rad.bewid.pvol.dbzh.scan1.hdf`, WMO 06477) ODIM_H5 polar
  volumes, redistributed from the `wradlib-data` collection (MIT).
- **met.no** (Norwegian Meteorological Institute) - Røst ODIM_H5 polar volume
  (`T_PAGZ35_C_ENMI_20170421090837.hdf`, WMO 01104), redistributed from
  `open-radar-data` (MIT).
- **EUMETNET OPERA / AEMET** (Spain) - Perdiguera Doppler polar volume
  (`espdg.pvol.20260707.dbzh_vradh.h5`, WMO 08162), fetched 2026-07-07 from
  the EUMETNET OPERA ORD 24-hour bucket and used under **CC BY 4.0**.
- **EUMETNET OPERA / SMHI** (Sweden) - Ängelholm scan
  (`seang.scan.20260820.dbzh_th_vradh.h5`, WMO 02606), fetched 2026-08-20 from
  the same bucket and used under **CC BY 4.0**.
- **NOAA** - a head excerpt of a VORTEX-2 NOXP DORADE sweepfile
  (`swp.1090509143923.NOXPRVP.0.0.5_PPI_v1.head3`), used under **CC BY 4.0**
  from "VORTEX-2 2009-2010 radar data from NOAA X-band dual Polarimetric radar
  (NOXP)", Zenodo, doi:10.5281/zenodo.14194361.
- **US DOE ARM** - an X-SAPR CfRadial PPI at the Southern Great Plains site,
  in both container formats (`cfrad.xsapr_sgp_ppi_20110520.classic.nc` and
  `cfrad.xsapr_sgp_ppi_20110520.netcdf4.nc`; the file's own `institution`
  attribute reads "United States Department of Energy - Atmospheric Radiation
  Measurement (ARM) program"), redistributed from ARM-DOE/Py-ART
  (BSD-3-Clause).
- **NSSL** - three excerpts of KOUN time-series (Level 1 / I/Q) records from
  20 May 2013 (`KOUN_RVP.20130520.194601.730.Ascope_DEFAULT.0.H+V.250.head24`,
  `KOUN_RVP.20130520.224139.456.Ascope_DEFAULT.0.H+V.150.head8` and
  `koun_20130520_194601.iq.rain_shaft.iqd`, the last being 32 pulses
  re-encoded into this project's own interchange format). KOUN is the research
  WSR-88D at Norman, Oklahoma; the records were fetched from the NSSL THREDDS
  archive at `data.nssl.noaa.gov`, whose catalogue states their rights as
  "Freely available".
- **NOAA/NWS** - four LDM records excerpted from a KDVN Archive II volume
  (`KDVN20260819_192802_V06.rec0_1_7_79`), fetched from the public NEXRAD
  Level II archive.
- **Operator and terms not established** -
  `swp.1260521225514.COW2.229.1.0_SUR_v215.head24`, a 37,380-byte head excerpt
  of a DORADE surveillance sweepfile. Everything known about it comes from the
  file itself: written by an LROSE `DoradeRadxFile` writer, instrument name
  `COW2`, volume project name `BOULDER`, radar position 39.740 N 103.293 W at
  1,519 m, sweep start 2026-05-21 22:55:14 UTC. Who collected it, and whether
  they permit redistribution, is recorded nowhere in this repository. An
  earlier note in the test that reads it called it a CSWR sweepfile; that was
  never sourced, so it is not repeated here as a credit. The excerpt is kept
  only because it is the one big-endian, run-length-encoded DORADE file the
  decoder is tested against, and it will be removed if the terms cannot be
  established or if whoever collected it asks.

The five European volumes are 1.69 MB of the total.

CC BY 4.0 asks for attribution wherever the work is redistributed. That is
here, in the shipped tree, and not only in a source comment a reader would
have to go looking for.

### `crates/basemap_tiles/tests/data`

Two real tile bodies, 50 KB together, checked in so that dropping an image
codec from the dependency list fails a test instead of silently blanking the
basemap. Both are the z9 tile over KTLX, captured 2026-08-18:

- **USGS The National Map** - `usgs-imagery-9-117-202.jpg`, from
  `/USGSImageryOnly/MapServer/tile/9/202/117`. USGS National Map imagery is
  released as **public domain**.
- **© OpenStreetMap contributors** - `osm-9-117-202.png`, from the standard
  layer, available under the **Open Database License (ODbL)**.

## What keeps this file honest

- `crates/nexrad_io/tests/fixture_attribution.rs` maps every fixture on disk to
  the credit it owes and fails if a new one arrives undecided, or if a credit
  here names a file that is no longer present.
- `crates/data_source/tests/data_credits.rs` compares the live-source credits
  above against the bucket constants the code actually requests, so renaming a
  bucket breaks the credit in the same commit.
- `crates/workstation_app/tests/release_process.rs` fails if the release
  process stops carrying this file into the published snapshot.

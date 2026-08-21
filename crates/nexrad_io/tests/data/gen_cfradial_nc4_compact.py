#!/usr/bin/env python
"""Generate the compact-link-storage netCDF-4 CfRadial 1.x golden fixture.

Provenance: this file is SYNTHETIC, written by the netCDF-C library through
netCDF4-python (1.7.4 / libnetcdf 4.9.x at generation time) with
`format="NETCDF4"`. It is a real netCDF-4 file from the reference writer,
not a hand-assembled byte blob.

Why it exists: netCDF-4 groups store their links two ways, and the real
CfRadial fixture beside it only exercises one of them. HDF5 keeps a group's
links as COMPACT link messages inside the object header while the group
stays under its max-compact threshold (eight links by default), and moves
them to DENSE storage — a fractal heap indexed by a version 2 B-tree — past
it. Every published CfRadial 1 file has dozens of variables and is therefore
always dense; this five-variable volume is the smallest thing that is still
CfRadial and is compact. Its link info message carries UNDEFINED heap and
B-tree addresses, which is the marker of compact storage, and its global
attributes are compact too.

Volume: one 0.5-degree PPI sweep, 3 rays x 4 gates, one packed-short
`reflectivity` field. Values are deterministic so the Rust test can assert
exact physical numbers through CF packing:

  raw   = [[0, 2, 4, 6], [8, 10, -9999, 14], [16, 18, 20, 22]]  (i2)
  scale_factor = 0.5, add_offset = 1.0, _FillValue = -9999
  physical = raw * 0.5 + 1.0, with the _FillValue gate masked to NaN
          = [[1, 2, 3, 4], [5, 6, NaN, 8], [9, 10, 11, 12]]

Gate geometry is 100 m to the first gate centre and 100 m spacing, so the
`range` coordinate reading is checkable by eye as well.

Regenerate with:
    python gen_cfradial_nc4_compact.py cfrad.tiny_compact_links.netcdf4.nc
"""

import sys

import numpy as np
import netCDF4

RAW = np.array(
    [[0, 2, 4, 6], [8, 10, -9999, 14], [16, 18, 20, 22]],
    dtype="i2",
)


def main(path):
    data = netCDF4.Dataset(path, "w", format="NETCDF4")
    data.Conventions = "CF/Radial instrument_parameters"
    data.instrument_name = "TINY"
    data.site_name = "compact link storage"
    data.time_coverage_start = "2020-01-01T00:00:00Z"

    data.createDimension("time", RAW.shape[0])
    data.createDimension("range", RAW.shape[1])

    time = data.createVariable("time", "f8", ("time",))
    time.units = "seconds since 2020-01-01 00:00:00Z"
    time[:] = [0.0, 1.0, 2.0]

    gates = data.createVariable("range", "f4", ("range",))
    gates.units = "meters"
    gates[:] = [100.0, 200.0, 300.0, 400.0]

    azimuth = data.createVariable("azimuth", "f4", ("time",))
    azimuth.units = "degrees"
    azimuth[:] = [10.0, 20.0, 30.0]

    elevation = data.createVariable("elevation", "f4", ("time",))
    elevation.units = "degrees"
    elevation[:] = [0.5, 0.5, 0.5]

    field = data.createVariable(
        "reflectivity", "i2", ("time", "range"), fill_value=np.int16(-9999)
    )
    # Without this netCDF4-python re-applies scale_factor on write and
    # rewrites every value it is handed.
    field.set_auto_maskandscale(False)
    field.units = "dBZ"
    field.standard_name = "equivalent_reflectivity_factor"
    field.scale_factor = 0.5
    field.add_offset = 1.0
    field[:] = RAW

    data.close()
    print("wrote", path)


if __name__ == "__main__":
    main(sys.argv[1] if len(sys.argv) > 1 else "cfrad.tiny_compact_links.netcdf4.nc")

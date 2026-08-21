#!/usr/bin/env python
"""Generate the "declared but never written" CfRadial pair, one file per container.

Provenance: both files are SYNTHETIC, written by the netCDF-C library through
netCDF4-python (1.7.4 / libnetcdf 4.9.3 / HDF5 1.14.6 at generation time).
The netCDF-4 one is a real HDF5 file from the reference writer, not a
hand-assembled byte blob, and the classic one is written from the values
netCDF4-python reads back out of it - so the two carry the same volume and
differ only in container.

Why they exist: HDF5 allocates dataset space lazily, so a variable that is
created and never written has NO bytes on disk (HDF5 File Format Specification
v3.0, section IV.A.2.viii "Data Layout": an undefined data address for a
contiguous dataset, and simply no chunk records for a chunked one). A reader
that materialises those regions as zero returns 0.0 dBZ for ground the file
says was never measured - a wrong answer painted as weak echo, which is worse
than a refusal. The right answer is the dataset's own fill value, from the
Fill Value message (0x0005), which is what netCDF writes `_FillValue` into.

No published CfRadial file surveyed in 2026 has unallocated storage: ARM,
Py-ART and Radx all write every gate explicitly, and their all-fill fields
therefore decode correctly even from a reader that ignores the fill message.
This pair is the file that does not, and it is written here because there is
no published one to point at.

The three fields cover the three shapes the storage can take:

  reflectivity    f4, CONTIGUOUS, never written  -> undefined data address
  velocity        f4, chunked (4 x 6) + deflate, never written  -> no chunks
  spectrum_width  f4, chunked (4 x 6) + deflate, rays 0-3 written -> 1 of 2
                  chunks allocated, the other absent

All three declare `_FillValue = -9999.0`. netCDF4-python reads:

  reflectivity    -9999.0 in all 48 gates
  velocity        -9999.0 in all 48 gates
  spectrum_width  0.5, 1.0, ... 12.0 across rays 0-3, then -9999.0 for rays 4-7

so a CfRadial decode must mask every -9999.0 gate to NaN and keep exactly the
24 written spectrum-width gates finite, in BOTH containers.

The netCDF-4 file also declares one USER-DEFINED type, `gate_class_t`, which
netCDF stores as an HDF5 committed (named) datatype sitting in the root group
beside the variables. It is not a variable, has no dataspace, and nothing
below uses it - it is here because a reader that mistakes it for a variable
fails the whole file over an object it never needed to read. Classic netCDF
has no user-defined types, so only the netCDF-4 file carries it.

Regenerate with:
    python gen_cfradial_unwritten_storage.py
"""

import os

import netCDF4
import numpy as np

RAYS, GATES = 8, 6
FILL = np.float32(-9999.0)
WRITTEN_RAYS = 4
FIELDS = ("reflectivity", "velocity", "spectrum_width")
STANDARD_NAMES = {
    "reflectivity": ("dBZ", "equivalent_reflectivity_factor"),
    "velocity": ("m/s", "radial_velocity_of_scatterers_away_from_instrument"),
    "spectrum_width": ("m/s", "doppler_spectrum_width"),
}
# 0.5, 1.0, ... 12.0 m/s over the rays that were written.
WRITTEN = (np.arange(WRITTEN_RAYS * GATES, dtype="f4").reshape(WRITTEN_RAYS, GATES) + 1.0) * 0.5


def write_geometry(data):
    """The ray and gate geometry both containers share."""
    data.Conventions = "CF/Radial instrument_parameters"
    data.instrument_name = "FILLTEST"
    data.site_name = "unwritten storage"
    data.time_coverage_start = "2020-01-01T00:00:00Z"

    data.createDimension("time", RAYS)
    data.createDimension("range", GATES)

    time = data.createVariable("time", "f8", ("time",))
    time.units = "seconds since 2020-01-01 00:00:00Z"
    time[:] = np.arange(RAYS, dtype="f8")

    gates = data.createVariable("range", "f4", ("range",))
    gates.units = "meters"
    gates[:] = 100.0 + 100.0 * np.arange(GATES, dtype="f4")

    azimuth = data.createVariable("azimuth", "f4", ("time",))
    azimuth.units = "degrees"
    azimuth[:] = np.arange(RAYS, dtype="f4") * 45.0

    elevation = data.createVariable("elevation", "f4", ("time",))
    elevation.units = "degrees"
    elevation[:] = np.full(RAYS, 0.5, dtype="f4")


def describe(field, name):
    units, standard_name = STANDARD_NAMES[name]
    # Without this netCDF4-python re-applies its own mask on write.
    field.set_auto_maskandscale(False)
    field.units = units
    field.standard_name = standard_name


def write_netcdf4(path):
    data = netCDF4.Dataset(path, "w", format="NETCDF4")
    write_geometry(data)

    # A committed (named) datatype in the root group: an object with a
    # datatype and no dataspace, which is neither a variable nor a group.
    data.createEnumType(np.uint8, "gate_class_t", {"clear": 0, "precip": 1})

    # Contiguous and never written: the data layout message carries the
    # undefined address, so there is nothing on disk to read.
    describe(
        data.createVariable(
            "reflectivity", "f4", ("time", "range"), fill_value=FILL, contiguous=True
        ),
        "reflectivity",
    )
    # Chunked and never written: the chunk B-tree address is undefined.
    describe(
        data.createVariable(
            "velocity",
            "f4",
            ("time", "range"),
            fill_value=FILL,
            zlib=True,
            complevel=1,
            chunksizes=(WRITTEN_RAYS, GATES),
        ),
        "velocity",
    )
    # Chunked and HALF written: one chunk record exists, the other does not.
    width = data.createVariable(
        "spectrum_width",
        "f4",
        ("time", "range"),
        fill_value=FILL,
        zlib=True,
        complevel=1,
        chunksizes=(WRITTEN_RAYS, GATES),
    )
    describe(width, "spectrum_width")
    width[0:WRITTEN_RAYS, :] = WRITTEN

    data.close()


def write_classic(source_path, path):
    """The same volume in CDF-1, from the values netCDF4-python reads back."""
    source = netCDF4.Dataset(source_path, "r")
    data = netCDF4.Dataset(path, "w", format="NETCDF3_CLASSIC")
    write_geometry(data)
    for name in FIELDS:
        original = source.variables[name]
        original.set_auto_maskandscale(False)
        field = data.createVariable(name, "f4", ("time", "range"), fill_value=FILL)
        describe(field, name)
        field[:] = np.asarray(original[:])
    data.close()
    source.close()


def report(path):
    print("---", os.path.basename(path))
    data = netCDF4.Dataset(path, "r")
    for name in FIELDS:
        field = data.variables[name]
        field.set_auto_maskandscale(False)
        values = np.asarray(field[:])
        print(
            f"  {name}: {int(np.sum(values != FILL))} of {values.size} gates written,"
            f" first={values.flat[0]} last={values.flat[-1]}"
        )
    data.close()
    print("  ", os.path.getsize(path), "bytes")


def main():
    here = os.path.dirname(os.path.abspath(__file__))
    netcdf4 = os.path.join(here, "cfrad.unwritten_storage.netcdf4.nc")
    classic = os.path.join(here, "cfrad.unwritten_storage.classic.nc")
    write_netcdf4(netcdf4)
    write_classic(netcdf4, classic)
    report(netcdf4)
    report(classic)


if __name__ == "__main__":
    main()

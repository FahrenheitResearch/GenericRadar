#!/usr/bin/env python
"""Generate the "declared but never written" ODIM_H5 fixture.

Provenance: SYNTHETIC and deliberately HOSTILE, written by h5py (3.15.1 at
generation time) following the ODIM_H5 v2.2 information model (D. B. Michelson
et al., EUMETNET OPERA WD_2008_03).

HDF5 allocates dataset space lazily, so a dataset that is created and never
written carries an UNDEFINED data address (HDF5 File Format Specification v3.0,
section IV.A.2.viii "Data Layout") and reads back as the fill value. That makes
the on-disk size of a plane independent of its DECLARED size: this 15 KB file
declares 16 planes of 360 x 2000 gates - 11.5 MB of decoded data, 780x its own
size - and a file that declares thousands of them is a few hundred KB.

The decoder must therefore bound the SUM of what a file declares, not just each
plane, and this fixture is the regression pin for that bound. It is also a
legitimate (if empty) volume: every plane decodes as all-no-data, which is what
a never-radiated sweep looks like.

Run: python gen_odim_declared_only.py  (writes next to itself)
"""

import os

import h5py
import numpy as np

SWEEPS = 4
PLANES = ("DBZH", "VRADH", "WRADH", "ZDR")
NRAYS, NBINS = 360, 2000


def s(value: str) -> np.bytes_:
    return np.bytes_(value.encode("ascii"))


def main() -> None:
    out = os.path.join(
        os.path.dirname(os.path.abspath(__file__)), "odim_pvol_declared_only.h5"
    )
    with h5py.File(out, "w", libver="earliest") as f:
        f.attrs.create("Conventions", s("ODIM_H5/V2_2"))

        what = f.create_group("what")
        what.attrs.create("object", s("PVOL"))
        what.attrs.create("version", s("H5rad 2.2"))
        what.attrs.create("date", s("20260820"))
        what.attrs.create("time", s("000000"))
        what.attrs.create("source", s("NOD:declared,PLC:Declared Only"))

        where = f.create_group("where")
        where.attrs.create("lat", np.float64(50.0))
        where.attrs.create("lon", np.float64(5.0))
        where.attrs.create("height", np.float64(100.0))

        for index in range(1, SWEEPS + 1):
            ds = f.create_group(f"dataset{index}")
            dwhat = ds.create_group("what")
            dwhat.attrs.create("product", s("SCAN"))
            dwhere = ds.create_group("where")
            dwhere.attrs.create("elangle", np.float64(0.5 * index))
            dwhere.attrs.create("nbins", np.int64(NBINS))
            dwhere.attrs.create("nrays", np.int64(NRAYS))
            dwhere.attrs.create("rstart", np.float64(0.0))
            dwhere.attrs.create("rscale", np.float64(500.0))

            for number, quantity in enumerate(PLANES, start=1):
                data = ds.create_group(f"data{number}")
                # created, never written -> contiguous with an UNDEFINED address
                data.create_dataset("data", shape=(NRAYS, NBINS), dtype="u1")
                qwhat = data.create_group("what")
                qwhat.attrs.create("quantity", s(quantity))
                qwhat.attrs.create("gain", np.float64(0.5))
                qwhat.attrs.create("offset", np.float64(-32.0))
                qwhat.attrs.create("nodata", np.float64(255.0))
                qwhat.attrs.create("undetect", np.float64(0.0))
    print(
        out,
        os.path.getsize(out),
        "bytes on disk,",
        SWEEPS * len(PLANES) * NRAYS * NBINS,
        "bytes declared",
    )


if __name__ == "__main__":
    main()

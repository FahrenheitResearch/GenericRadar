//! netCDF-4 reader: the netCDF data model as HDF5 stores it.
//!
//! Format reference: Unidata, "The NetCDF-4 Format" and "NetCDF-4 File
//! Format Specification"
//! (<https://docs.unidata.ucar.edu/netcdf-c/current/file_format_specifications.html>),
//! layered on The HDF Group, "HDF5 File Format Specification Version 3.0".
//! netCDF-4 is not a container of its own: it is an HDF5 file with three
//! conventions layered on top, and this module is exactly those three
//! conventions read back off [`crate::hdf5lite`].
//!
//! 1. A netCDF VARIABLE is an HDF5 dataset in the group.
//! 2. A netCDF DIMENSION is an HDF5 "dimension scale" — a rank-1 dataset
//!    carrying `CLASS = "DIMENSION_SCALE"`. A dimension that also has
//!    values (a coordinate variable) is both; a dimension that does not is
//!    marked by a `NAME` attribute reading "This is a netCDF dimension but
//!    not a netCDF variable...", and is a dimension ONLY. `_Netcdf4Dimid`
//!    carries netCDF's own dimension id where the writer emits it.
//! 3. A variable's SHAPE is named by `DIMENSION_LIST`, a variable-length
//!    sequence of HDF5 object references — one per axis, each pointing at
//!    that axis's dimension-scale dataset.
//!
//! Reading `DIMENSION_LIST` rather than matching axis lengths is not
//! fussiness. On the ARM X-SAPR vertical-pointing file surveyed here the
//! `time` and `sweep` dimensions are both 360 long, so length matching
//! would put `fixed_angle(sweep)` on the ray axis and quietly turn a
//! one-angle file into a 360-sweep one.
//!
//! Scope: the ROOT group. CfRadial 1.x is a flat file — every variable and
//! every attribute sits at the root — so that is the whole of what this
//! reader needs. CfRadial 2 puts each sweep in its own HDF5 group and is
//! detected and refused by name below rather than being read as an empty
//! root.
//!
//! Why this exists: CfRadial 1.x is DOMINANTLY netCDF-4 in the wild. Every
//! published CfRadial 1 sample surveyed in 2026 — ARM's X-SAPR, Ka-SACR and
//! CSAPR2 products, CSWR's DOW8 deployments, Py-ART's own example files —
//! is a netCDF-4 file, so a CfRadial decoder that reads only the classic
//! container opens almost nothing an analyst actually has.

use std::collections::BTreeMap;

use crate::hdf5lite::{H5Attr, H5Data, H5File};
use crate::netcdf3::{NcArray, NcSource, NcValue, NcVar};
use crate::{NexradError, Result};

/// The `CLASS` attribute value that marks a dimension scale.
const DIMENSION_SCALE_CLASS: &str = "DIMENSION_SCALE";
/// Prefix of the `NAME` attribute HDF5 gives a dimension scale that netCDF
/// created for a dimension with no coordinate variable behind it.
const DIMENSION_ONLY_NAME_PREFIX: &str = "This is a netCDF dimension but not a netCDF variable";
/// Attributes that are netCDF-4/HDF5 PLUMBING rather than data.
///
/// These are how the conventions above are written down; netCDF itself
/// hides them, and a CfRadial decode reading `standard_name` should not
/// have to step over `REFERENCE_LIST` to get there.
const RESERVED_ATTRIBUTES: [&str; 6] = [
    "CLASS",
    "NAME",
    "REFERENCE_LIST",
    "DIMENSION_LIST",
    "_Netcdf4Dimid",
    "_Netcdf4Coordinates",
];
/// Ceiling on how many root-group objects one file may declare, matching
/// the classic reader's variable ceiling.
const MAX_NC4_VARIABLES: usize = 4096;

/// `true` when the buffer is an HDF5 file that carries netCDF-4's own
/// conventions.
///
/// The discriminator is the dimension scale, because that is what netCDF-4
/// cannot be written without: every netCDF-4 file with any dimension at all
/// has at least one rank-1 dataset marked `CLASS = "DIMENSION_SCALE"`, and
/// no ODIM_H5 volume has any. `_NCProperties` — the netCDF library's own
/// provenance stamp — would be a narrower test but is absent from files
/// written before netCDF 4.4.1 and from anything that copied a file without
/// it, so it is used only as a second opinion.
pub fn looks_like_netcdf4(file: &H5File<'_>) -> bool {
    if file.attr("/", "_NCProperties").is_some() {
        return true;
    }
    file.child_names("/")
        .iter()
        .any(|name| is_dimension_scale(file, &format!("/{name}")))
}

fn is_dimension_scale(file: &H5File<'_>, path: &str) -> bool {
    file.attr(path, "CLASS")
        .as_ref()
        .and_then(H5Attr::as_str)
        .is_some_and(|class| class == DIMENSION_SCALE_CLASS)
}

/// A netCDF-4 file's root group, read into the shared netCDF data model.
pub struct Nc4File<'a> {
    file: H5File<'a>,
    dims: Vec<(String, usize)>,
    gattrs: BTreeMap<String, NcValue>,
    vars: BTreeMap<String, NcVar>,
}

impl<'a> Nc4File<'a> {
    pub fn open(bytes: &'a [u8]) -> Result<Self> {
        Self::from_hdf5(H5File::open(bytes)?)
    }

    /// Build the netCDF view over an HDF5 file that is already open.
    ///
    /// The routing layer opens the file once to decide whether it is ODIM_H5
    /// or netCDF-4 and hands the same view straight here, rather than
    /// walking the object tree of a large volume twice.
    pub fn from_hdf5(file: H5File<'a>) -> Result<Self> {
        let children = file.child_names("/");
        if children.len() > MAX_NC4_VARIABLES {
            return Err(invalid(format!(
                "netCDF-4 root group holds {} objects (limit {MAX_NC4_VARIABLES})",
                children.len()
            )));
        }
        // A user-defined type — `nc_def_enum` and friends — is a committed
        // HDF5 datatype sitting in the group beside the variables. It is not
        // a variable, has no shape, and no CfRadial moment is stored in one,
        // so it is skipped rather than read or refused: a file that declares
        // a gate-classification enum next to ordinary float fields is still
        // a file every one of whose fields this decoder can read.
        let children: Vec<String> = children
            .into_iter()
            .filter(|name| !file.is_committed_datatype(&format!("/{name}")))
            .collect();
        if let Some(group) = children
            .iter()
            .find(|name| !file.is_dataset(&format!("/{name}")))
        {
            // CfRadial 2 is the reason a netCDF-4 radar file has groups at
            // all: it puts each sweep in `/sweep_0000`, `/sweep_0001`, ...
            // Reading its root would find no `(time, range)` field and
            // report a missing dimension, which sends the reader looking
            // for a corrupt file instead of a different convention.
            return Err(invalid(format!(
                "netCDF-4 file has a '{group}' group; this decoder reads CfRadial 1.x, which is \
                 flat. A grouped radar file is CfRadial 2 — convert it with `RadxConvert \
                 -cfradial1`"
            )));
        }

        // Pass one: the dimension scales, which every variable's shape is
        // written in terms of.
        let mut scales: Vec<DimensionScale> = Vec::new();
        for name in &children {
            let path = format!("/{name}");
            if !is_dimension_scale(&file, &path) {
                continue;
            }
            let length = file.dataset_shape(&path)?.first().copied().unwrap_or(0);
            scales.push(DimensionScale {
                name: name.clone(),
                length,
                address: file.object_address(&path).unwrap_or_default(),
                // A dimension with no coordinate variable behind it says so
                // in its own NAME attribute.
                coordinate_variable: !file
                    .attr(&path, "NAME")
                    .as_ref()
                    .and_then(H5Attr::as_str)
                    .is_some_and(|label| label.starts_with(DIMENSION_ONLY_NAME_PREFIX)),
            });
        }
        // netCDF's own dimension ids where the writer recorded them, its
        // name order where it did not. Either way the order is stable, and
        // only self-consistency matters: nothing above looks a dimension up
        // by index, only by name.
        scales.sort_by(|left, right| {
            let key = |scale: &DimensionScale| {
                file.attr(&format!("/{}", scale.name), "_Netcdf4Dimid")
                    .as_ref()
                    .and_then(H5Attr::as_i64)
                    .unwrap_or(i64::MAX)
            };
            key(left)
                .cmp(&key(right))
                .then_with(|| left.name.cmp(&right.name))
        });
        let dims: Vec<(String, usize)> = scales
            .iter()
            .map(|scale| (scale.name.clone(), scale.length))
            .collect();

        // Pass two: the variables.
        let mut vars = BTreeMap::new();
        for name in &children {
            let path = format!("/{name}");
            let scale = scales.iter().position(|scale| scale.name == *name);
            if scale.is_some_and(|index| !scales[index].coordinate_variable) {
                continue; // a dimension, and nothing else
            }
            let rank = file.dataset_shape(&path)?.len();
            let dim_ids = resolve_dim_ids(&file, &path, rank, scale, &scales)?;
            vars.insert(
                name.clone(),
                NcVar {
                    name: name.clone(),
                    dim_ids,
                    attrs: read_attributes(&file, &path),
                },
            );
        }

        let gattrs = read_attributes(&file, "/");
        Ok(Self {
            file,
            dims,
            gattrs,
            vars,
        })
    }
}

/// One HDF5 dimension scale: a netCDF dimension, and possibly also the
/// coordinate variable of the same name.
struct DimensionScale {
    name: String,
    length: usize,
    /// Object header address, which is what a `DIMENSION_LIST` reference
    /// points at.
    address: u64,
    coordinate_variable: bool,
}

/// A variable's dimension indices, from `DIMENSION_LIST` where the file has
/// one.
///
/// A dimension scale is its own coordinate: `time(time)` carries no
/// `DIMENSION_LIST` because the axis it lies along is itself. A scalar
/// variable has no axes at all. Anything else without a usable
/// `DIMENSION_LIST` gets no dimension ids rather than guessed ones — an
/// invented axis would put a field on the wrong grid, and a field with no
/// axes is simply one a CfRadial decode will not select.
fn resolve_dim_ids(
    file: &H5File<'_>,
    path: &str,
    rank: usize,
    own_scale: Option<usize>,
    scales: &[DimensionScale],
) -> Result<Vec<usize>> {
    if rank == 0 {
        return Ok(Vec::new());
    }
    if let Some(references) = file
        .attr_object_references(path, "DIMENSION_LIST")
        .filter(|references| references.len() == rank)
    {
        let mut dim_ids = Vec::with_capacity(rank);
        for axis in &references {
            // An axis can carry more than one attached scale; the first
            // is the one netCDF treats as the dimension.
            let Some(address) = axis.first() else {
                return Err(invalid(format!(
                    "netCDF-4 variable '{path}' has an axis with no dimension attached"
                )));
            };
            let index = scales
                .iter()
                .position(|scale| scale.address == *address)
                .ok_or_else(|| {
                    invalid(format!(
                        "netCDF-4 variable '{path}' names a dimension that is not in this group"
                    ))
                })?;
            dim_ids.push(index);
        }
        return Ok(dim_ids);
    }
    // The coordinate variable of a one-dimensional dimension lies along
    // itself, and carries no `DIMENSION_LIST` saying so.
    match own_scale {
        Some(index) if rank == 1 => Ok(vec![index]),
        _ => Ok(Vec::new()),
    }
}

/// An object's attributes, converted to netCDF attribute values with the
/// HDF5 plumbing left out.
fn read_attributes(file: &H5File<'_>, path: &str) -> BTreeMap<String, NcValue> {
    file.attrs(path)
        .into_iter()
        .filter(|(name, _)| !RESERVED_ATTRIBUTES.contains(&name.as_str()))
        .map(|(name, value)| (name, attr_value(value)))
        .collect()
}

fn attr_value(value: H5Attr) -> NcValue {
    match value {
        H5Attr::Str(text) => NcValue::Str(text.trim_end_matches('\0').to_owned()),
        H5Attr::F64(number) => NcValue::Doubles(vec![number]),
        H5Attr::I64(number) => NcValue::Ints(vec![number]),
        H5Attr::F64Array(numbers) => NcValue::Doubles(numbers),
        H5Attr::I64Array(numbers) => NcValue::Ints(numbers),
    }
}

impl NcSource for Nc4File<'_> {
    fn dims(&self) -> &[(String, usize)] {
        &self.dims
    }

    fn vars(&self) -> &BTreeMap<String, NcVar> {
        &self.vars
    }

    fn gattrs(&self) -> &BTreeMap<String, NcValue> {
        &self.gattrs
    }

    fn read_var(&self, name: &str) -> Result<NcArray> {
        if !self.vars.contains_key(name) {
            return Err(invalid(format!("netCDF variable '{name}' not found")));
        }
        // Integer widths widen rather than narrow, so no stored value is
        // lost on the way into the shared model: netCDF's unsigned byte and
        // unsigned short have no signed counterpart of their own width.
        Ok(match self.file.dataset(&format!("/{name}"))?.data {
            H5Data::Chars(bytes) => NcArray::Char(bytes),
            H5Data::U8(bytes) => NcArray::I16(bytes.into_iter().map(i16::from).collect()),
            H5Data::U16(values) => NcArray::I32(values.into_iter().map(i32::from).collect()),
            H5Data::F32(values) => NcArray::F32(values),
            H5Data::F64(values) => NcArray::F64(values),
        })
    }
}

fn invalid(reason: impl Into<String>) -> NexradError {
    NexradError::InvalidMessage {
        offset: 0,
        reason: reason.into(),
    }
}

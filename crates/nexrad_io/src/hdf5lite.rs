//! Minimal read-only HDF5 parser — just enough for the two radar formats
//! that arrive inside HDF5 containers: ODIM_H5 polar volumes and netCDF-4.
//!
//! The workspace has no HDF5 dependency (the C library is a heavy, awkward
//! build input on Windows CI), and between them those two formats exercise
//! a narrow, stable corner of the format — but they sit at opposite ends of
//! it. ODIM writers (BALTRAD/rave, HL-HDF, IRIS export, and h5py at its
//! default libver "earliest") stay on the 1.6 layout: version-0
//! superblocks, version-1 object headers, old-style groups. netCDF-4 — the
//! container CfRadial 1.x is dominantly written in, so
//! [`crate::netcdf4`] and [`crate::cfradial`] depend on it — writes the 1.8
//! layout instead: version-2/3 superblocks, version-2 object headers, and
//! links and attributes in fractal heaps indexed by version-2 B-trees once
//! a group outgrows compact storage. This module implements the union of
//! the two, byte-for-byte against the HDF5 File Format Specification (The
//! HDF Group, "HDF5 File Format Specification Version 3.0";
//! <https://support.hdfgroup.org/documentation/hdf5/latest/_f_m_t3.html>):
//!
//! - Superblock v0/v1 (offset and length sizes behind the free-space and
//!   root-table version bytes, root group reached through a symbol table
//!   entry) and v2/v3 (sizes straight after the version byte, root object
//!   header addressed directly, trailing checksum).
//! - Version 1 object headers, including continuation blocks, and version 2
//!   headers ("OHDR", with "OCHK" continuation blocks and Jenkins lookup3
//!   checksum verification). Real files mix the two: AEMET/Spain writes
//!   ODIM H5rad 2.4 (IRIS 8.13/10.3 export, live in ORD since 2026-06-23)
//!   with a version-0 superblock and old-style groups but version-2 headers
//!   on the leaf metadata groups (`datasetN/{how,what,where}`,
//!   `datasetN/dataM/{how,what}`).
//! - All three group link storages: the symbol table message (0x0011) — a
//!   v1 B-tree of SNOD leaves plus a local heap of names, which is what
//!   ODIM writes; compact link messages (0x0006), which a group under its
//!   max-compact threshold (eight links) uses; and dense storage behind a
//!   link info message (0x0002) — a fractal heap indexed by a version 2
//!   B-tree, where every netCDF-4 root group with more than eight variables
//!   keeps its links, which is every published CfRadial 1 file.
//! - Attributes both ways: compact attribute messages (0x000C, versions
//!   1-3) and dense storage behind an attribute info message (0x0015),
//!   again a fractal heap plus a version 2 B-tree.
//! - Messages: dataspace (0x0001), link info (0x0002), datatype (0x0003),
//!   fill value (0x0005 versions 1-3, and the deprecated 0x0004), link
//!   (0x0006), data layout (0x0008, v3 compact/contiguous/chunked), filter
//!   pipeline (0x000B, deflate id 1 and shuffle id 2), attribute (0x000C),
//!   header continuation (0x0010), symbol table (0x0011), attribute info
//!   (0x0015).
//! - Datatypes: fixed-point (1-8 bytes, signed or not) and IEEE float
//!   (f32/f64) anywhere, plus fixed-length strings — one byte wide in a
//!   dataset, which is how netCDF-4 stores `NC_CHAR` (the string length is
//!   the array's last dimension), any width in an attribute. Variable-length
//!   strings are read in attributes only, through global heap collections.
//! - Chunk index: v1 B-trees; raw chunks pass through the inverse filter
//!   pipeline (deflate, then unshuffle) and edge chunks are clipped.
//! - Storage the writer never allocated: a contiguous dataset whose data
//!   address is undefined, and a chunk with no record in the index, both
//!   read back as the dataset's FILL VALUE. Zero is the answer only where
//!   the file defines no fill value, which is HDF5's own default — not a
//!   stand-in for one it does define. A radar moment declared with
//!   `_FillValue = -9999` and never written is no-data everywhere, and
//!   returning 0.0 for it would paint weak echo over ground the file says
//!   was never measured.
//!
//! Out of scope, and refused BY NAME rather than guessed at: data layout
//! messages other than version 3 — version 4 is the 1.10 "latest" set of
//! chunk indexes (extensible array, fixed array, version 2 B-tree), and
//! versions 1-2 predate 1.6 — virtual layout, external data storage
//! (message 0x0007, which pairs with a contiguous layout whose address is
//! undefined and must not be mistaken for a plane that was never written),
//! filters other than deflate and shuffle (fletcher32 id 3, szip id 4,
//! scale-offset id 6, LZF id 32000), datatype classes other than the four
//! above (compound, enum, array, opaque, bitfield, reference, and
//! variable-length datasets — as opposed to variable-length string
//! attributes, which are read), object header messages carrying the SHARED
//! flag, attributes whose datatype or dataspace is a shared message, and
//! fractal heaps with I/O filters on their direct blocks. Soft and external
//! links are the one thing skipped rather than refused, so a group that
//! carries one alongside real datasets still opens.
//!
//! A committed (named) datatype — what netCDF-4 writes for a user-defined
//! type — is recognised as neither a group nor a dataset
//! ([`H5File::is_committed_datatype`]) so a caller can step over it; its
//! contents are not read.
//!
//! Hostile input is assumed: these bytes arrive by file drop, so every walk
//! is bounded by a `MAX_*` constant below and every failure must be a
//! `Result` a caller can show in a dialog. Two of those bounds exist because
//! neither of the failures they prevent is catchable — a stack overflow from
//! deep B-tree recursion ([`MAX_BTREE_DEPTH`]) and an allocation abort from a
//! file that declares far more data than it carries
//! ([`MAX_HDF5_TOTAL_DATASET_BYTES`]) both kill the process outright.

use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;

use flate2::read::ZlibDecoder;

use crate::{NexradError, Result};

const SIGNATURE: [u8; 8] = [0x89, b'H', b'D', b'F', b'\r', b'\n', 0x1a, b'\n'];
/// Version 2 object header signature (HDF5 spec section IV.A.2).
const OHDR_SIGNATURE: &[u8; 4] = b"OHDR";
/// Version 2 object header continuation block signature.
const OCHK_SIGNATURE: &[u8; 4] = b"OCHK";
/// Fractal heap header signature (HDF5 spec section III.G).
const FRHP_SIGNATURE: &[u8; 4] = b"FRHP";
/// Fractal heap indirect block signature.
const FHIB_SIGNATURE: &[u8; 4] = b"FHIB";
/// Fractal heap direct block signature.
const FHDB_SIGNATURE: &[u8; 4] = b"FHDB";
/// Version 2 B-tree header / internal node / leaf node signatures
/// (HDF5 spec section III.A.2).
const BTHD_SIGNATURE: &[u8; 4] = b"BTHD";
const BTIN_SIGNATURE: &[u8; 4] = b"BTIN";
const BTLF_SIGNATURE: &[u8; 4] = b"BTLF";
const UNDEFINED_ADDR: u64 = u64::MAX;
/// Object header message flag bit 1: the message body is a POINTER into the
/// file's shared-message table, not the message itself.
///
/// Nothing that writes radar data turns shared messages on — HDF5 leaves
/// `H5Pset_shared_mesg_nindexes` at zero and neither ODIM writers nor
/// netCDF-C change it — and reading one as if it were inline would take a
/// heap address for a datatype. So it is refused by name wherever it
/// appears.
const MESSAGE_FLAG_SHARED: u8 = 0x02;
/// Defense against corrupt files: deepest group nesting we will walk.
const MAX_GROUP_DEPTH: usize = 16;
/// Defense against corrupt B-trees: most nodes visited per tree walk.
const MAX_BTREE_NODES: usize = 1 << 16;
/// Defense against CRAFTED B-trees: deepest chain of internal nodes the
/// walkers will descend.
///
/// [`MAX_BTREE_NODES`] bounds how many nodes a walk visits, not how deep it
/// recurses, and depth is the dangerous one: a chain of single-entry internal
/// nodes (three patched bytes plus 64 bytes per link in a real file) recurses
/// once per link, and a stack overflow is a process kill Rust cannot catch —
/// no `catch_unwind`, no error dialog, just `STATUS_STACK_OVERFLOW`. Measured
/// on this parser: ~8 000 links exhaust the 2 MiB a Rust worker thread gets by
/// default, which is exactly where a GUI decodes a dropped file.
///
/// Well-formed trees never come close. HDF5 v1 B-trees split at 2K entries
/// per node (K = 32 for chunk indexes, 16 for symbol tables by default), so
/// even the [`MAX_DATA_CHUNKS`] / [`MAX_GROUP_ENTRIES`] ceilings below fit in a
/// handful of levels; a fanout-2 tree large enough to need 32 levels would
/// blow the node cap first.
const MAX_BTREE_DEPTH: usize = 32;
/// Defense against corrupt/self-referencing v2 header continuations: most
/// header blocks (chunk 0 + OCHK continuations) per object header.
const MAX_HEADER_BLOCKS: usize = 1 << 10;
const MAX_OBJECT_MESSAGES: usize = 4096;
const MAX_OBJECT_MESSAGE_BYTES: usize = 64 * 1024 * 1024;
const MAX_GROUP_ENTRIES: usize = 1 << 20;
const MAX_DATA_CHUNKS: usize = 1 << 18;
const MAX_DATASPACE_RANK: usize = 32;
const MAX_DATASPACE_DIM: usize = 100 * 1024 * 1024;
const MAX_HDF5_DATASET_BYTES: usize = 256 * 1024 * 1024;
/// Defense against declaration bombs: total decoded dataset bytes one
/// [`H5File`] view will materialise, summed over every [`H5File::dataset`]
/// call.
///
/// The per-dataset ceiling above bounds one plane; nothing bounded the SUM,
/// and an ODIM volume is a loop over `/datasetN/dataM`. Because HDF5 allocates
/// dataset space lazily, a plane that was never written carries an UNDEFINED
/// address and decodes to a fill-value buffer conjured from zero input bytes —
/// so a 170 KB file declaring 64 such planes made this parser allocate ~6 GB
/// and return `Ok` (measured with a counting allocator, 34 000x amplification,
/// linear in the declared plane count). A machine with less headroom aborts
/// instead, which like the stack overflow above is uncatchable.
///
/// 512 MB is the same order as the Archive II decode ceiling and roughly 5x
/// the largest real ODIM volume surveyed (the widest OPERA/DWD volumes decode
/// to ~100 MB: 10-14 sweeps x 8 moments x 720 rays x 1 200 gates x 2 bytes).
pub(crate) const MAX_HDF5_TOTAL_DATASET_BYTES: usize = 512 * 1024 * 1024;
const MAX_HDF5_ATTRIBUTE_BYTES: usize = 16 * 1024 * 1024;
/// Longest link or attribute name accepted, so a corrupt length field
/// cannot reserve a string the file does not contain.
const MAX_HDF5_NAME_BYTES: usize = 64 * 1024;
const MAX_HDF5_FILTERS: usize = 32;
const MAX_HDF5_FILTER_VALUES: usize = 1024;
/// Defense against corrupt fractal heaps: most managed direct blocks one
/// heap may resolve to, and the deepest chain of indirect blocks walked.
///
/// A heap's doubling table doubles the row size every row after the second,
/// so a legitimate heap reaches gigabytes in a couple of dozen rows and
/// these ceilings cannot be met by real data: the widest netCDF-4 root
/// group surveyed here (118 links, 37 attributes) resolves 12 direct blocks
/// at depth 1. They exist because the row/column walk below is driven
/// entirely by counts read out of the file.
const MAX_FRACTAL_HEAP_BLOCKS: usize = 1 << 14;
const MAX_FRACTAL_HEAP_DEPTH: usize = 16;
/// Defense against corrupt version 2 B-trees: deepest node chain descended
/// and most records collected from one tree. The depth ceiling matters for
/// the same reason [`MAX_BTREE_DEPTH`] does — recursion is what a stack
/// overflow is made of, and that is a process kill Rust cannot catch.
const MAX_BTREE_V2_DEPTH: usize = 32;
const MAX_BTREE_V2_RECORDS: usize = 1 << 20;

/// `true` when the buffer starts with the HDF5 superblock signature.
pub fn looks_like_hdf5_bytes(bytes: &[u8]) -> bool {
    bytes.len() >= SIGNATURE.len() && bytes[..SIGNATURE.len()] == SIGNATURE
}

/// A decoded scalar or 1-D attribute value.
#[derive(Clone, Debug, PartialEq)]
pub enum H5Attr {
    Str(String),
    F64(f64),
    I64(i64),
    F64Array(Vec<f64>),
    I64Array(Vec<i64>),
}

impl H5Attr {
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::Str(value) => Some(value),
            _ => None,
        }
    }

    /// Numeric view: integers widen to f64 (ODIM writers disagree about
    /// whether e.g. `nodata` is a long or a double).
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Self::F64(value) => Some(*value),
            Self::I64(value) => Some(*value as f64),
            _ => None,
        }
    }

    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Self::I64(value) => Some(*value),
            Self::F64(value) => (value.fract() == 0.0).then_some(*value as i64),
            _ => None,
        }
    }
}

/// Raw dataset elements, converted from the on-disk datatype.
#[derive(Clone, Debug, PartialEq)]
pub enum H5Data {
    U8(Vec<u8>),
    U16(Vec<u16>),
    F32(Vec<f32>),
    F64(Vec<f64>),
    /// Characters: a fixed-length string datatype one byte wide, which is
    /// how netCDF-4 stores `NC_CHAR`. Kept apart from [`Self::U8`] because
    /// the bytes mean text, not numbers — a caller that read a `sweep_mode`
    /// as small integers would get digits where it wanted "rhi".
    Chars(Vec<u8>),
}

impl H5Data {
    pub fn len(&self) -> usize {
        match self {
            Self::U8(values) => values.len(),
            Self::U16(values) => values.len(),
            Self::F32(values) => values.len(),
            Self::F64(values) => values.len(),
            Self::Chars(values) => values.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// A dataset: dimension sizes (row-major) plus the element array.
#[derive(Clone, Debug)]
pub struct H5Dataset {
    pub dims: Vec<usize>,
    pub data: H5Data,
}

/// Read-only HDF5 file view over a byte slice.
pub struct H5File<'a> {
    bytes: &'a [u8],
    offset_size: usize,
    length_size: usize,
    /// Absolute path ("/a/b") → object header address for every object
    /// reachable from the root group.
    objects: BTreeMap<String, u64>,
    /// Whole-file decode budget: the ceiling this view was opened with, and
    /// how much of it is left. See [`MAX_HDF5_TOTAL_DATASET_BYTES`].
    budget_total: usize,
    budget_left: Cell<usize>,
}

impl<'a> H5File<'a> {
    pub fn open(bytes: &'a [u8]) -> Result<Self> {
        Self::open_within_budget(bytes, MAX_HDF5_TOTAL_DATASET_BYTES)
    }

    /// [`H5File::open`] with an explicit whole-file decode budget in bytes.
    /// Tests use a small budget to exercise the ceiling without allocating
    /// half a gigabyte.
    pub(crate) fn open_within_budget(bytes: &'a [u8], budget: usize) -> Result<Self> {
        if !looks_like_hdf5_bytes(bytes) {
            return Err(invalid(0, "missing HDF5 superblock signature"));
        }
        let version = *bytes.get(8).ok_or_else(|| truncated(8, 1, bytes.len()))?;
        if version > 3 {
            return Err(invalid(
                8,
                format!("HDF5 superblock version {version} is unsupported (need 0-3)"),
            ));
        }
        // Superblock v0/v1 put the offset/length sizes after a
        // free-space/root-table version triple; v2/v3 (the 1.8+ "latest"
        // layout, and what every netCDF-4 writer emits) put them straight
        // after the version byte and drop the symbol-table entry entirely
        // in favour of a direct root object header address.
        let modern = version >= 2;
        let (offset_size, length_size) = if modern {
            (read_u8(bytes, 9)? as usize, read_u8(bytes, 10)? as usize)
        } else {
            (read_u8(bytes, 13)? as usize, read_u8(bytes, 14)? as usize)
        };
        if !(4..=8).contains(&offset_size) || !(4..=8).contains(&length_size) {
            return Err(invalid(13, "unsupported HDF5 offset/length sizes"));
        }
        let root_header = if modern {
            // base, superblock-extension and EOF addresses, then the root
            // group's object header address.
            read_offset(bytes, 12 + 3 * offset_size, offset_size)?
        } else {
            // v0: fixed fields end at 24; v1 inserts 4 bytes (indexed-
            // storage k). base, free-space, EOF, driver-info addresses;
            // then the root group symbol table entry, whose object header
            // address is field 2.
            let addr_block = if version == 0 { 24 } else { 28 };
            let root_entry = addr_block + 4 * offset_size;
            read_offset(bytes, root_entry + offset_size, offset_size)?
        };
        let mut file = Self {
            bytes,
            offset_size,
            length_size,
            objects: BTreeMap::new(),
            budget_total: budget,
            budget_left: Cell::new(budget),
        };
        let header = file.parse_object_header(root_header)?;
        file.objects.insert("/".to_owned(), root_header);
        let mut visited_groups = BTreeSet::from([root_header]);
        file.walk_group("", &header, &mut visited_groups, 0)?;
        Ok(file)
    }

    /// Draw `bytes` from the whole-file decode budget, or fail.
    fn charge_decode_budget(&self, bytes: usize, what: &str) -> Result<()> {
        let left = self.budget_left.get();
        if bytes > left {
            return Err(invalid(
                0,
                format!(
                    "HDF5 decode budget exhausted: {what} needs {bytes} bytes, {left} of the \
                     {} byte whole-file budget left",
                    self.budget_total
                ),
            ));
        }
        self.budget_left.set(left - bytes);
        Ok(())
    }

    /// Names of the direct children of `path` (groups and datasets).
    pub fn child_names(&self, path: &str) -> Vec<String> {
        let prefix = if path == "/" {
            "/".to_owned()
        } else {
            format!("{}/", path.trim_end_matches('/'))
        };
        self.objects
            .keys()
            .filter_map(|key| {
                let rest = key.strip_prefix(&prefix)?;
                (!rest.is_empty() && !rest.contains('/')).then(|| rest.to_owned())
            })
            .collect()
    }

    pub fn has_object(&self, path: &str) -> bool {
        self.objects.contains_key(path)
    }

    /// Read one attribute of the object at `path`.
    pub fn attr(&self, path: &str, name: &str) -> Option<H5Attr> {
        for body in self.attribute_messages(path).ok()? {
            if let Ok(Some(attr)) = self.parse_attribute(&body, name) {
                return Some(attr);
            }
        }
        None
    }

    /// Every attribute of the object at `path`, in the order the file
    /// indexes them.
    ///
    /// Attributes this reader cannot decode are SKIPPED rather than fatal:
    /// netCDF-4 hangs its own bookkeeping off attributes whose datatypes are
    /// outside this parser's scope — `DIMENSION_LIST` is a variable-length
    /// sequence of object references and `REFERENCE_LIST` is a compound —
    /// and a caller enumerating a variable's CF attributes should not lose
    /// `units` because `REFERENCE_LIST` sits next to it. The ones netCDF-4
    /// bookkeeping actually needs have their own accessors below.
    pub fn attrs(&self, path: &str) -> Vec<(String, H5Attr)> {
        let Ok(messages) = self.attribute_messages(path) else {
            return Vec::new();
        };
        messages
            .iter()
            .filter_map(|body| {
                let raw = self.split_attribute(body).ok()?;
                let value = self.decode_attribute(&raw).ok()?;
                Some((raw.name.clone(), value))
            })
            .collect()
    }

    /// Names of every attribute of the object at `path`, including the ones
    /// [`Self::attrs`] cannot decode a value for.
    pub fn attr_names(&self, path: &str) -> Vec<String> {
        let Ok(messages) = self.attribute_messages(path) else {
            return Vec::new();
        };
        messages
            .iter()
            .filter_map(|body| Some(self.split_attribute(body).ok()?.name))
            .collect()
    }

    /// Decode an attribute holding a variable-length sequence of OBJECT
    /// REFERENCES into the addresses it points at.
    ///
    /// This is exactly one attribute in practice: netCDF-4's
    /// `DIMENSION_LIST`, which names a variable's dimensions by pointing at
    /// the dimension-scale dataset for each axis. Nothing else in this
    /// crate's world uses references, so the shape is read narrowly — a
    /// rank-1 dataspace of vlen elements whose base type is an 8-byte
    /// object reference — and anything else returns `None`.
    /// An attribute this reader cannot even split is SKIPPED, exactly as in
    /// [`Self::attrs`]: giving up on the whole search at the first one would
    /// mean an object whose `DIMENSION_LIST` happens to sit after some other
    /// undecodable attribute reports no dimensions at all — and a variable
    /// with no dimension ids drops out of the field filter and vanishes from
    /// the volume without an error anyone can see.
    pub fn attr_object_references(&self, path: &str, name: &str) -> Option<Vec<Vec<u64>>> {
        for body in self.attribute_messages(path).ok()? {
            let Ok(raw) = self.split_attribute(body.as_slice()) else {
                continue;
            };
            if raw.name != name {
                continue;
            }
            return self.decode_reference_sequence(&raw).ok();
        }
        None
    }

    /// Dimension sizes of a dataset, without reading (or budgeting for) its
    /// elements.
    pub fn dataset_shape(&self, path: &str) -> Result<Vec<usize>> {
        let address = *self
            .objects
            .get(path)
            .ok_or_else(|| invalid(0, format!("HDF5 object '{path}' not found")))?;
        for message in &self.parse_object_header(address)?.messages {
            if message.kind == 0x0001 {
                return self.parse_dataspace(&message.body);
            }
        }
        Err(invalid(0, format!("dataset '{path}' has no dataspace")))
    }

    /// `true` when the object at `path` is a dataset: an array, so it
    /// carries both a datatype and a dataspace.
    ///
    /// A group carries neither. A COMMITTED (named) datatype carries the
    /// datatype ALONE, which is why the dataspace has to be part of the
    /// test — see [`Self::is_committed_datatype`].
    pub fn is_dataset(&self, path: &str) -> bool {
        let (datatype, dataspace) = self.datatype_and_dataspace(path);
        datatype && dataspace
    }

    /// `true` when the object at `path` is a committed (named) datatype: a
    /// datatype message with no dataspace beside it.
    ///
    /// netCDF-4 writes one of these into the group for every user-defined
    /// type a file declares — `nc_def_enum`, `nc_def_compound`,
    /// `nc_def_vlen` — so an otherwise ordinary CfRadial file can carry one
    /// next to its variables. It is not a variable and has no shape, and a
    /// reader that treats it as one fails a whole file over an object it
    /// never needed to read.
    pub fn is_committed_datatype(&self, path: &str) -> bool {
        let (datatype, dataspace) = self.datatype_and_dataspace(path);
        datatype && !dataspace
    }

    /// Whether the object at `path` carries a datatype message (0x0003) and
    /// a dataspace message (0x0001): the pair that tells a dataset, a group
    /// and a committed datatype apart.
    fn datatype_and_dataspace(&self, path: &str) -> (bool, bool) {
        let Some(header) = self
            .objects
            .get(path)
            .and_then(|address| self.parse_object_header(*address).ok())
        else {
            return (false, false);
        };
        let has = |kind: u16| header.messages.iter().any(|message| message.kind == kind);
        (has(0x0003), has(0x0001))
    }

    /// Object header address of `path`, as [`Self::attr_object_references`]
    /// reports the targets of a reference.
    pub fn object_address(&self, path: &str) -> Option<u64> {
        self.objects.get(path).copied()
    }

    /// Every attribute message body on the object at `path`, from compact
    /// and dense storage alike.
    ///
    /// Compact attributes are 0x000C messages in the object header. Dense
    /// ones — what an object gets past eight attributes, which is most
    /// netCDF-4 root groups — live in a fractal heap announced by an
    /// attribute info (0x0015) message, and are the same message bytes once
    /// fetched.
    fn attribute_messages(&self, path: &str) -> Result<Vec<Vec<u8>>> {
        let address = *self
            .objects
            .get(path)
            .ok_or_else(|| invalid(0, format!("HDF5 object '{path}' not found")))?;
        let header = self.parse_object_header(address)?;
        let mut bodies = Vec::new();
        for message in &header.messages {
            match message.kind {
                0x000C => bodies.push(message.body.clone()),
                0x0015 => {
                    // The attribute info message's optional creation index
                    // is 2 bytes wide, where a link info message's is 8.
                    let Some((heap_address, name_btree)) =
                        self.dense_storage_addresses(&message.body, 2)?
                    else {
                        continue;
                    };
                    let heap = self.fractal_heap(heap_address)?;
                    for record in self.btree_v2_records(name_btree)? {
                        // Attribute name record (type 8): the heap ID
                        // FIRST, then message flags, creation order and the
                        // name hash.
                        let id = record.get(..heap.heap_id_len).ok_or_else(|| {
                            invalid(0, "dense attribute record is shorter than its heap ID")
                        })?;
                        bodies.push(self.heap_object(&heap, id)?);
                    }
                }
                _ => {}
            }
        }
        Ok(bodies)
    }

    /// Read the full dataset at `path`.
    pub fn dataset(&self, path: &str) -> Result<H5Dataset> {
        let address = *self
            .objects
            .get(path)
            .ok_or_else(|| invalid(0, format!("HDF5 object '{path}' not found")))?;
        let header = self.parse_object_header(address)?;
        let mut dims: Option<Vec<usize>> = None;
        let mut dtype: Option<Datatype> = None;
        let mut layout: Option<Layout> = None;
        let mut filters: Vec<Filter> = Vec::new();
        let mut fill: Option<Vec<u8>> = None;
        for message in &header.messages {
            match message.kind {
                0x0001 => dims = Some(self.parse_dataspace(&message.body)?),
                0x0003 => dtype = Some(self.parse_datatype(&message.body)?),
                0x0004 | 0x0005 => {
                    if let Some(value) = parse_fill_value(message.kind, &message.body)? {
                        fill = Some(value);
                    }
                }
                0x0007 => {
                    // External storage: the elements live in files beside
                    // this one, and the layout message that comes with it
                    // is contiguous with an UNDEFINED address. Following
                    // the reference is out of scope, and treating it as
                    // never-written would hand back a plane of fill value
                    // for a dataset whose data exists.
                    return Err(invalid(
                        0,
                        format!(
                            "dataset '{path}' keeps its elements in external data files, which \
                             this reader does not follow"
                        ),
                    ));
                }
                0x0008 => layout = Some(self.parse_layout(&message.body)?),
                0x000B => filters = self.parse_filter_pipeline(&message.body)?,
                _ => {}
            }
        }
        let dims = dims.ok_or_else(|| invalid(0, format!("dataset '{path}' has no dataspace")))?;
        let dtype = dtype.ok_or_else(|| invalid(0, format!("dataset '{path}' has no datatype")))?;
        let layout = layout.ok_or_else(|| invalid(0, format!("dataset '{path}' has no layout")))?;
        let element_count = checked_product(&dims, "HDF5 dataset element count")?;
        let byte_len = checked_allocation_bytes(
            element_count,
            dtype.size,
            MAX_HDF5_DATASET_BYTES,
            "HDF5 dataset",
        )?;
        // A fill value is stored in the dataset's own datatype, so it can
        // only be repeated across a buffer if it is exactly one element
        // wide. Anything else is a malformed file, and repeating it anyway
        // would smear a pattern across element boundaries — the silent
        // misread this module refuses to make.
        if let Some(value) = &fill
            && value.len() != dtype.size
        {
            return Err(invalid(
                0,
                format!(
                    "dataset '{path}' declares a {}-byte fill value for a {}-byte datatype",
                    value.len(),
                    dtype.size
                ),
            ));
        }
        // Charge the whole-file budget from the DECLARED size, before any of
        // the paths below allocate — including the never-written contiguous
        // one, which materialises a fill-value plane out of no input bytes.
        self.charge_decode_budget(byte_len, &format!("dataset '{path}'"))?;
        let raw = match layout {
            Layout::Compact(data) => data,
            Layout::Contiguous { address, size } => {
                if address == UNDEFINED_ADDR {
                    // Never written, so the file holds no bytes for it at
                    // all: every element reads as the fill value.
                    filled_buffer(byte_len, fill.as_deref())
                } else {
                    self.slice(address, (size as usize).min(byte_len))?.to_vec()
                }
            }
            Layout::Chunked {
                btree_address,
                chunk_dims,
            } => self.read_chunked(
                btree_address,
                &chunk_dims,
                &dims,
                dtype.size,
                &filters,
                fill.as_deref(),
            )?,
        };
        if raw.len() < byte_len {
            return Err(invalid(
                0,
                format!(
                    "dataset '{path}' raw stream too short: {} < {byte_len}",
                    raw.len()
                ),
            ));
        }
        let data = dtype.convert(&raw[..byte_len])?;
        Ok(H5Dataset { dims, data })
    }

    // ----- object graph -------------------------------------------------

    fn walk_group(
        &mut self,
        prefix: &str,
        header: &ObjectHeader,
        visited_groups: &mut BTreeSet<u64>,
        depth: usize,
    ) -> Result<()> {
        if depth > MAX_GROUP_DEPTH {
            return Err(invalid(0, "HDF5 group nesting too deep"));
        }
        for (name, child_address) in self.group_children(header)? {
            let path = format!("{prefix}/{name}");
            if self.objects.contains_key(&path) {
                continue; // hard-link cycle guard
            }
            let child = self.parse_object_header(child_address)?;
            self.objects.insert(path.clone(), child_address);
            if visited_groups.insert(child_address) {
                self.walk_group(&path, &child, visited_groups, depth + 1)?;
            }
        }
        Ok(())
    }

    /// The (name, object header address) pairs one group links to, from
    /// whichever of the three link storages it uses.
    ///
    /// * Symbol table message (0x0011) — the 1.6 "old style" group: a v1
    ///   B-tree of SNOD leaves plus a local heap of names. ODIM writers
    ///   emit this.
    /// * Link messages (0x0006) — "new style" compact storage, used while a
    ///   group stays under its max-compact threshold (eight links by
    ///   default).
    /// * Link info message (0x0002) — "new style" dense storage: the links
    ///   live in a fractal heap indexed by a version 2 B-tree. Every
    ///   netCDF-4 root group with more than eight variables is here, which
    ///   is every CfRadial file.
    fn group_children(&self, header: &ObjectHeader) -> Result<Vec<(String, u64)>> {
        let mut children = Vec::new();
        for message in &header.messages {
            match message.kind {
                0x0011 => {
                    let btree = read_offset(&message.body, 0, self.offset_size)?;
                    let heap = read_offset(&message.body, self.offset_size, self.offset_size)?;
                    let heap_data = self.local_heap_data(heap)?;
                    let mut entries = Vec::new();
                    let mut visited_nodes = BTreeSet::new();
                    self.collect_group_entries(btree, &mut entries, &mut visited_nodes, 0)?;
                    for (name_offset, child_address) in entries {
                        children.push((
                            heap_string(self.bytes, heap_data, name_offset)?,
                            child_address,
                        ));
                    }
                }
                0x0006 => {
                    if let Some(link) = self.parse_link_message(&message.body)? {
                        children.push(link);
                    }
                }
                0x0002 => self.collect_dense_links(&message.body, &mut children)?,
                _ => {}
            }
            if children.len() > MAX_GROUP_ENTRIES {
                return Err(invalid(0, "HDF5 group has too many entries"));
            }
        }
        Ok(children)
    }

    /// One "new style" link message: a name and, for a hard link, the
    /// address of the object it names.
    ///
    /// Soft and external links return `None` — their link information is a
    /// path or a filename, not an address in this file, and following one
    /// is out of scope. They are skipped rather than refused so a group that
    /// carries one alongside real datasets still opens.
    fn parse_link_message(&self, body: &[u8]) -> Result<Option<(String, u64)>> {
        let version = *body.first().ok_or_else(|| truncated(0, 1, 0))?;
        if version != 1 {
            return Err(invalid(
                0,
                format!("link message version {version} unsupported"),
            ));
        }
        let flags = *body.get(1).ok_or_else(|| truncated(1, 1, body.len()))?;
        let mut at = 2usize;
        let link_type = if flags & 0x08 != 0 {
            let kind = *body.get(at).ok_or_else(|| truncated(at, 1, body.len()))?;
            at += 1;
            kind
        } else {
            0 // hard link, the default when the field is absent
        };
        if flags & 0x04 != 0 {
            at += 8; // creation order
        }
        if flags & 0x10 != 0 {
            at += 1; // name character set
        }
        let name_length_size = 1usize << (flags & 0x03);
        let name_len = usize::try_from(read_uint(body, at, name_length_size)?)
            .map_err(|_| invalid(at, "HDF5 link name length overflows usize"))?;
        if name_len > MAX_HDF5_NAME_BYTES {
            return Err(invalid(
                at,
                format!("HDF5 link name is {name_len} bytes (limit {MAX_HDF5_NAME_BYTES})"),
            ));
        }
        at = at
            .checked_add(name_length_size)
            .ok_or_else(|| invalid(at, "HDF5 link cursor overflow"))?;
        let name = String::from_utf8_lossy(checked_range(body, at, name_len)?).into_owned();
        if link_type != 0 {
            return Ok(None);
        }
        at = at
            .checked_add(name_len)
            .ok_or_else(|| invalid(at, "HDF5 link cursor overflow"))?;
        Ok(Some((name, read_offset(body, at, self.offset_size)?)))
    }

    /// Dense link storage: read the fractal heap and its name index.
    fn collect_dense_links(&self, body: &[u8], out: &mut Vec<(String, u64)>) -> Result<()> {
        let Some((heap_address, name_btree)) = self.dense_storage_addresses(body, 8)? else {
            return Ok(());
        };
        let heap = self.fractal_heap(heap_address)?;
        for record in self.btree_v2_records(name_btree)? {
            // Link name record (type 5): a 4-byte hash of the name, then
            // the link message's heap ID.
            let id = record
                .get(4..4 + heap.heap_id_len)
                .ok_or_else(|| invalid(0, "dense link record is shorter than its heap ID"))?;
            if let Some(link) = self.parse_link_message(&self.heap_object(&heap, id)?)? {
                out.push(link);
            }
        }
        Ok(())
    }

    /// The fractal heap and name-index addresses a link info (0x0002) or
    /// attribute info (0x0015) message points at, or `None` when the object
    /// keeps that kind of metadata in compact messages instead.
    ///
    /// The two messages share a shape: version, flags, an optional maximum
    /// creation index whose WIDTH differs between them, then the heap and
    /// the two B-tree addresses.
    fn dense_storage_addresses(
        &self,
        body: &[u8],
        creation_index_bytes: usize,
    ) -> Result<Option<(u64, u64)>> {
        let version = *body.first().ok_or_else(|| truncated(0, 1, 0))?;
        if version != 0 {
            return Err(invalid(
                0,
                format!("dense storage info message version {version} unsupported"),
            ));
        }
        let flags = *body.get(1).ok_or_else(|| truncated(1, 1, body.len()))?;
        let mut at = 2usize;
        if flags & 0x01 != 0 {
            at += creation_index_bytes;
        }
        let heap_address = read_offset(body, at, self.offset_size)?;
        at = at
            .checked_add(self.offset_size)
            .ok_or_else(|| invalid(at, "dense storage cursor overflow"))?;
        let name_btree = read_offset(body, at, self.offset_size)?;
        if heap_address == UNDEFINED_ADDR || name_btree == UNDEFINED_ADDR {
            // Undefined addresses mean this object has not gone dense: its
            // links or attributes are compact messages in the header.
            return Ok(None);
        }
        Ok(Some((heap_address, name_btree)))
    }

    fn collect_group_entries(
        &self,
        node_address: u64,
        out: &mut Vec<(u64, u64)>,
        visited: &mut BTreeSet<u64>,
        depth: usize,
    ) -> Result<()> {
        if depth > MAX_BTREE_DEPTH {
            return Err(invalid(0, "HDF5 group B-tree nested too deep"));
        }
        if !visited.insert(node_address) {
            return Err(invalid(
                address_to_usize(node_address)?,
                "cycle in HDF5 group B-tree",
            ));
        }
        if visited.len() > MAX_BTREE_NODES {
            return Err(invalid(0, "HDF5 group B-tree too large"));
        }
        let node = self.slice(node_address, 8 + 2 * self.offset_size)?;
        if &node[..4] != b"TREE" {
            return Err(invalid(node_address as usize, "expected TREE signature"));
        }
        let level = node[5];
        let entries = u16::from_le_bytes([node[6], node[7]]) as usize;
        // keys/children alternate after the two sibling addresses.
        let mut cursor = address_to_usize(node_address)?
            .checked_add(8 + 2 * self.offset_size)
            .ok_or_else(|| invalid(0, "HDF5 group B-tree cursor overflow"))?;
        for _ in 0..entries {
            cursor += self.length_size; // key (heap offset) — unused here
            let child = read_offset(self.bytes, cursor, self.offset_size)?;
            cursor += self.offset_size;
            if level == 0 {
                self.read_snod(child, out)?;
            } else {
                self.collect_group_entries(child, out, visited, depth + 1)?;
            }
        }
        Ok(())
    }

    fn read_snod(&self, address: u64, out: &mut Vec<(u64, u64)>) -> Result<()> {
        let head = self.slice(address, 8)?;
        if &head[..4] != b"SNOD" {
            return Err(invalid(address as usize, "expected SNOD signature"));
        }
        let count = u16::from_le_bytes([head[6], head[7]]) as usize;
        let entry_size = 2 * self.offset_size + 8 + 16;
        let mut cursor = address_to_usize(address)?
            .checked_add(8)
            .ok_or_else(|| invalid(0, "HDF5 symbol-table address overflow"))?;
        for _ in 0..count {
            let name_offset = read_offset(self.bytes, cursor, self.length_size)?;
            let header = read_offset(self.bytes, cursor + self.offset_size, self.offset_size)?;
            if out.len() >= MAX_GROUP_ENTRIES {
                return Err(invalid(
                    address_to_usize(address)?,
                    "HDF5 group has too many entries",
                ));
            }
            out.push((name_offset, header));
            cursor = cursor
                .checked_add(entry_size)
                .ok_or_else(|| invalid(cursor, "HDF5 symbol-table cursor overflow"))?;
        }
        Ok(())
    }

    // ----- fractal heaps ------------------------------------------------

    /// Read a fractal heap header and resolve every managed direct block in
    /// it to a file address.
    ///
    /// HDF5 1.8 moved "dense" link and attribute storage — what a group gets
    /// once it outgrows compact messages, which for netCDF-4 means any group
    /// with more than eight variables or eight attributes — into a fractal
    /// heap indexed by a version 2 B-tree. Every netCDF-4 CfRadial file
    /// surveyed stores BOTH its links and its global attributes this way, so
    /// without this the root group reads as empty.
    ///
    /// The heap's managed space is a doubling table: `table_width` blocks per
    /// row, rows 0 and 1 of `starting_block_size` and every row after that
    /// twice the one before, addressed by a single linear offset that counts
    /// block headers as well as object bytes. Resolving the blocks up front
    /// turns a heap ID into a slice lookup.
    fn fractal_heap(&self, address: u64) -> Result<FractalHeap> {
        let (offset_size, length_size) = (self.offset_size, self.length_size);
        // 22 fixed bytes, 12 "length" fields, 3 addresses — see the field
        // list in the HDF5 spec, "Fractal Heap Header".
        let header_len = 22 + 12 * length_size + 3 * offset_size;
        let head = self.slice(address, header_len)?;
        if &head[..4] != FRHP_SIGNATURE {
            return Err(invalid(address as usize, "expected FRHP signature"));
        }
        if head[4] != 0 {
            return Err(invalid(
                address as usize,
                format!("fractal heap version {} unsupported (need 0)", head[4]),
            ));
        }
        let heap_id_len = usize::from(read_le_u16(head, 5)?);
        let filter_len = usize::from(read_le_u16(head, 7)?);
        let flags = head[9];
        let max_managed_size = read_uint(head, 10, 4)?;
        // Skip the huge/tiny/free-space bookkeeping: this reader only
        // fetches managed and tiny objects, and never writes.
        let mut at = 14 + 10 * length_size + 2 * offset_size;
        let table_width = usize::from(read_le_u16(head, at)?);
        at += 2;
        let starting_block_size = read_uint(head, at, length_size)?;
        at += length_size;
        let max_direct_block_size = read_uint(head, at, length_size)?;
        at += length_size;
        let max_heap_bits = usize::from(read_le_u16(head, at)?);
        // Skip the starting row count; the CURRENT row count is what the
        // root block actually has.
        at += 4;
        let root_address = read_offset(head, at, offset_size)?;
        at += offset_size;
        let root_rows = usize::from(read_le_u16(head, at)?);

        if filter_len != 0 {
            // A filtered heap stores its direct blocks compressed, with the
            // stored size and filter mask alongside each address. No netCDF
            // or ODIM writer filters a metadata heap; saying so beats
            // decoding the wrong bytes.
            return Err(invalid(
                address as usize,
                "fractal heap with I/O filters on its direct blocks is unsupported",
            ));
        }
        if table_width == 0 || starting_block_size == 0 || max_direct_block_size == 0 {
            return Err(invalid(
                address as usize,
                "degenerate fractal heap geometry",
            ));
        }
        if !starting_block_size.is_power_of_two() || !max_direct_block_size.is_power_of_two() {
            return Err(invalid(
                address as usize,
                "fractal heap block sizes are not powers of two",
            ));
        }
        if max_heap_bits == 0 || max_heap_bits > 64 {
            return Err(invalid(
                address as usize,
                format!("fractal heap maximum heap size of {max_heap_bits} bits is out of range"),
            ));
        }
        // Managed heap ID: one flags byte, then the object's offset in the
        // heap's linear space, then its length. The offset field is as wide
        // as the maximum heap size needs; the length field is as wide as the
        // largest object the heap can hold in one block needs.
        let id_offset_bytes = max_heap_bits.div_ceil(8);
        // `H5HF_hdr_finish_init_phase1`: the length field is the narrower of
        // what the largest direct block's offsets need and what the largest
        // managed object's size needs. For the two heaps netCDF-4 uses —
        // link storage (64 KiB blocks, 4 KiB objects) and attribute storage
        // (the same) — that is 2 bytes, giving the 7- and 8-byte heap IDs
        // their name and attribute B-tree records carry.
        let id_length_bytes = log2_floor(max_direct_block_size)
            .div_ceil(8)
            .min(limit_enc_size(max_managed_size));
        if heap_id_len < 1 + id_offset_bytes + id_length_bytes {
            return Err(invalid(
                address as usize,
                format!("fractal heap ID length {heap_id_len} is too short for its own geometry"),
            ));
        }
        // Rows at or past this index hold indirect blocks, not direct ones.
        let max_direct_rows =
            log2_floor(max_direct_block_size) - log2_floor(starting_block_size) + 2;

        let mut heap = FractalHeap {
            heap_id_len,
            id_offset_bytes,
            id_length_bytes,
            checksummed_blocks: flags & 0x02 != 0,
            table_width,
            starting_block_size,
            max_direct_rows,
            blocks: Vec::new(),
        };
        if root_address != UNDEFINED_ADDR {
            if root_rows == 0 {
                // A heap small enough to have never doubled: the root block
                // IS the first direct block.
                heap.blocks.push(HeapBlock {
                    heap_offset: 0,
                    size: usize::try_from(starting_block_size)
                        .map_err(|_| invalid(0, "fractal heap block size overflows usize"))?,
                    address: root_address,
                });
            } else {
                self.collect_heap_blocks(&mut heap, root_address, root_rows, 0, 0)?;
            }
        }
        Ok(heap)
    }

    /// Walk one indirect block, recording its direct-block children and
    /// descending into its indirect ones.
    fn collect_heap_blocks(
        &self,
        heap: &mut FractalHeap,
        address: u64,
        rows: usize,
        block_offset: u64,
        depth: usize,
    ) -> Result<()> {
        if depth > MAX_FRACTAL_HEAP_DEPTH {
            return Err(invalid(
                address_to_usize(address)?,
                "fractal heap indirect blocks nested too deep",
            ));
        }
        let offset_width = heap.id_offset_bytes;
        let prefix = 5 + self.offset_size + offset_width;
        if self.slice(address, 4)? != FHIB_SIGNATURE {
            return Err(invalid(
                address_to_usize(address)?,
                "expected FHIB signature",
            ));
        }
        let mut cursor = address_to_usize(address)?
            .checked_add(prefix)
            .ok_or_else(|| invalid(0, "fractal heap indirect block cursor overflow"))?;
        let mut row_start = block_offset;
        for row in 0..rows {
            let row_block_size = heap.row_block_size(row)?;
            for column in 0..heap.table_width {
                let child_offset =
                    row_start
                        .checked_add((column as u64).checked_mul(row_block_size).ok_or_else(
                            || invalid(cursor, "fractal heap column offset overflow"),
                        )?)
                        .ok_or_else(|| invalid(cursor, "fractal heap child offset overflow"))?;
                let child = read_offset(self.bytes, cursor, self.offset_size)?;
                cursor = cursor
                    .checked_add(self.offset_size)
                    .ok_or_else(|| invalid(cursor, "fractal heap entry cursor overflow"))?;
                if child == UNDEFINED_ADDR {
                    continue; // a block this heap has not needed yet
                }
                if heap.blocks.len() >= MAX_FRACTAL_HEAP_BLOCKS {
                    return Err(invalid(
                        address_to_usize(address)?,
                        "fractal heap has too many blocks",
                    ));
                }
                if row < heap.max_direct_rows {
                    heap.blocks.push(HeapBlock {
                        heap_offset: child_offset,
                        size: usize::try_from(row_block_size).map_err(|_| {
                            invalid(cursor, "fractal heap block size overflows usize")
                        })?,
                        address: child,
                    });
                } else {
                    // A child indirect block spans `row_block_size` bytes;
                    // its own row count follows from that span and the width
                    // of the first row (HDF5 spec, "Fractal Heap Indirect
                    // Block": nrows = log2(size) - log2(start * width) + 1).
                    let span = log2_floor(row_block_size);
                    let base = log2_floor(
                        heap.starting_block_size
                            .checked_mul(heap.table_width as u64)
                            .ok_or_else(|| invalid(cursor, "fractal heap row span overflow"))?,
                    );
                    if span < base {
                        return Err(invalid(
                            cursor,
                            "fractal heap child indirect block is short",
                        ));
                    }
                    self.collect_heap_blocks(
                        heap,
                        child,
                        span - base + 1,
                        child_offset,
                        depth + 1,
                    )?;
                }
            }
            row_start = row_start
                .checked_add(
                    row_block_size
                        .checked_mul(heap.table_width as u64)
                        .ok_or_else(|| invalid(cursor, "fractal heap row size overflow"))?,
                )
                .ok_or_else(|| invalid(cursor, "fractal heap row offset overflow"))?;
        }
        Ok(())
    }

    /// Fetch one object out of a fractal heap by its heap ID.
    fn heap_object(&self, heap: &FractalHeap, id: &[u8]) -> Result<Vec<u8>> {
        let flags = *id.first().ok_or_else(|| truncated(0, 1, 0))?;
        match flags & 0x30 {
            0x00 => {}
            0x20 => {
                // Tiny object: the data IS the heap ID, after the flags
                // byte. The short form holds its length in the low nibble.
                let len = usize::from(flags & 0x0F) + 1;
                return Ok(checked_range(id, 1, len)?.to_vec());
            }
            other => {
                return Err(invalid(
                    0,
                    format!("fractal heap object type {other:#04x} unsupported"),
                ));
            }
        }
        let offset = read_uint(id, 1, heap.id_offset_bytes)?;
        let length = usize::try_from(read_uint(
            id,
            1 + heap.id_offset_bytes,
            heap.id_length_bytes,
        )?)
        .map_err(|_| invalid(0, "fractal heap object length overflows usize"))?;
        if length > MAX_HDF5_ATTRIBUTE_BYTES {
            return Err(invalid(0, "fractal heap object is too large"));
        }
        let block = heap
            .blocks
            .iter()
            .find(|block| {
                offset >= block.heap_offset && offset - block.heap_offset < block.size as u64
            })
            .ok_or_else(|| invalid(0, format!("fractal heap offset {offset} is in no block")))?;
        let within = usize::try_from(offset - block.heap_offset)
            .map_err(|_| invalid(0, "fractal heap block offset overflows usize"))?;
        // A managed heap offset counts the block's own header, so the block
        // address plus the difference lands on the object directly.
        let start = block
            .address
            .checked_add(within as u64)
            .ok_or_else(|| invalid(0, "fractal heap object address overflow"))?;
        if within
            .checked_add(length)
            .is_none_or(|end| end > block.size)
        {
            return Err(invalid(0, "fractal heap object runs past its block"));
        }
        // Objects begin after the direct block's own header, so an offset
        // that lands inside it is a corrupt ID rather than a short object.
        let block_header =
            5 + self.offset_size + heap.id_offset_bytes + usize::from(heap.checksummed_blocks) * 4;
        if within < block_header {
            return Err(invalid(
                address_to_usize(block.address)?,
                "fractal heap object overlaps its block header",
            ));
        }
        if self.slice(block.address, 4)? != FHDB_SIGNATURE {
            return Err(invalid(
                address_to_usize(block.address)?,
                "expected FHDB signature",
            ));
        }
        Ok(self.slice(start, length)?.to_vec())
    }

    // ----- version 2 B-trees --------------------------------------------

    /// Collect every record in a version 2 B-tree, as raw record bytes.
    ///
    /// Dense link and attribute storage indexes its fractal heap with one of
    /// these; each record carries the heap ID of one link or one attribute.
    /// Only the heap IDs matter here, so records come back unparsed and the
    /// caller slices out the field it needs.
    fn btree_v2_records(&self, address: u64) -> Result<Vec<Vec<u8>>> {
        let (offset_size, length_size) = (self.offset_size, self.length_size);
        let header_len = 20 + offset_size + length_size;
        let head = self.slice(address, header_len)?;
        if &head[..4] != BTHD_SIGNATURE {
            return Err(invalid(address as usize, "expected BTHD signature"));
        }
        if head[4] != 0 {
            return Err(invalid(
                address as usize,
                format!("version 2 B-tree header version {} unsupported", head[4]),
            ));
        }
        let node_size = usize::try_from(read_uint(head, 6, 4)?)
            .map_err(|_| invalid(0, "B-tree node size overflows usize"))?;
        let record_size = usize::from(read_le_u16(head, 10)?);
        let depth = usize::from(read_le_u16(head, 12)?);
        let root_address = read_offset(head, 16, offset_size)?;
        let root_records = usize::from(read_le_u16(head, 16 + offset_size)?);
        if record_size == 0 || node_size <= BTREE_V2_NODE_PREFIX {
            return Err(invalid(address as usize, "degenerate version 2 B-tree"));
        }
        if depth > MAX_BTREE_V2_DEPTH {
            return Err(invalid(
                address as usize,
                format!("version 2 B-tree depth {depth} exceeds {MAX_BTREE_V2_DEPTH}"),
            ));
        }
        let mut records = Vec::new();
        if root_address == UNDEFINED_ADDR || root_records == 0 {
            return Ok(records);
        }
        let shape = BTreeV2Shape::new(node_size, record_size, depth, offset_size)?;
        let mut visited = BTreeSet::new();
        self.walk_btree_v2_node(
            root_address,
            depth,
            root_records,
            &shape,
            &mut records,
            &mut visited,
        )?;
        Ok(records)
    }

    fn walk_btree_v2_node(
        &self,
        address: u64,
        depth: usize,
        record_count: usize,
        shape: &BTreeV2Shape,
        out: &mut Vec<Vec<u8>>,
        visited: &mut BTreeSet<u64>,
    ) -> Result<()> {
        if !visited.insert(address) {
            return Err(invalid(
                address_to_usize(address)?,
                "cycle in version 2 B-tree",
            ));
        }
        if visited.len() > MAX_BTREE_NODES {
            return Err(invalid(0, "version 2 B-tree has too many nodes"));
        }
        let expected = if depth == 0 {
            BTLF_SIGNATURE
        } else {
            BTIN_SIGNATURE
        };
        if self.slice(address, 4)? != expected {
            return Err(invalid(
                address_to_usize(address)?,
                format!("expected {} signature", String::from_utf8_lossy(expected)),
            ));
        }
        if record_count > shape.max_records_per_node {
            return Err(invalid(
                address_to_usize(address)?,
                format!("version 2 B-tree node declares {record_count} records"),
            ));
        }
        let mut cursor = address_to_usize(address)?
            .checked_add(6)
            .ok_or_else(|| invalid(0, "version 2 B-tree cursor overflow"))?;
        for _ in 0..record_count {
            if out.len() >= MAX_BTREE_V2_RECORDS {
                return Err(invalid(0, "version 2 B-tree holds too many records"));
            }
            out.push(self.slice(cursor as u64, shape.record_size)?.to_vec());
            cursor = cursor
                .checked_add(shape.record_size)
                .ok_or_else(|| invalid(cursor, "version 2 B-tree record cursor overflow"))?;
        }
        if depth == 0 {
            return Ok(());
        }
        // Internal node: one more child pointer than it has records.
        let child_nrec_size = shape.max_nrec_size;
        let subtree_size = if depth > 1 {
            shape.cumulative_nrec_size(depth - 1)?
        } else {
            0
        };
        for _ in 0..=record_count {
            let child = read_offset(self.bytes, cursor, self.offset_size)?;
            cursor = cursor
                .checked_add(self.offset_size)
                .ok_or_else(|| invalid(cursor, "version 2 B-tree child cursor overflow"))?;
            let child_records = usize::try_from(read_uint(self.bytes, cursor, child_nrec_size)?)
                .map_err(|_| invalid(cursor, "B-tree child record count overflows usize"))?;
            cursor = cursor
                .checked_add(child_nrec_size + subtree_size)
                .ok_or_else(|| invalid(cursor, "version 2 B-tree child cursor overflow"))?;
            if child == UNDEFINED_ADDR || child_records == 0 {
                continue;
            }
            self.walk_btree_v2_node(child, depth - 1, child_records, shape, out, visited)?;
        }
        Ok(())
    }

    fn local_heap_data(&self, address: u64) -> Result<u64> {
        let head = self.slice(address, 8 + 2 * self.length_size + self.offset_size)?;
        if &head[..4] != b"HEAP" {
            return Err(invalid(address as usize, "expected HEAP signature"));
        }
        read_offset(head, 8 + 2 * self.length_size, self.offset_size)
    }

    fn parse_object_header(&self, address: u64) -> Result<ObjectHeader> {
        // Version 2 headers announce themselves with a signature; version 1
        // headers have none and start with the version byte.
        if self
            .slice(address, OHDR_SIGNATURE.len())
            .is_ok_and(|sig| sig == OHDR_SIGNATURE)
        {
            return self.parse_object_header_v2(address);
        }
        let head = self.slice(address, 16)?;
        if head[0] != 1 {
            return Err(invalid(
                address as usize,
                format!("object header version {} is unsupported", head[0]),
            ));
        }
        let total_messages = u16::from_le_bytes([head[2], head[3]]) as usize;
        if total_messages > MAX_OBJECT_MESSAGES {
            return Err(invalid(
                address_to_usize(address)?,
                format!(
                    "HDF5 object header declares {total_messages} messages (limit {MAX_OBJECT_MESSAGES})"
                ),
            ));
        }
        let block_size = u32::from_le_bytes([head[8], head[9], head[10], head[11]]) as usize;
        if block_size > MAX_OBJECT_MESSAGE_BYTES {
            return Err(invalid(
                address_to_usize(address)?,
                "HDF5 object-header message block is too large",
            ));
        }
        let mut messages = Vec::with_capacity(total_messages);
        // (start, length) message blocks; the first follows 4 pad bytes.
        let first_block = address_to_usize(address)?
            .checked_add(16)
            .ok_or_else(|| invalid(0, "HDF5 object-header address overflow"))?;
        let mut blocks = vec![(first_block, block_size)];
        let mut scheduled_blocks = BTreeSet::from([first_block]);
        let mut block_index = 0;
        let mut message_bytes = 0usize;
        while block_index < blocks.len() && messages.len() < total_messages {
            let (start, len) = blocks[block_index];
            block_index += 1;
            let mut cursor = start;
            let end = start
                .checked_add(len)
                .ok_or_else(|| invalid(start, "HDF5 object-header block overflow"))?;
            self.bytes
                .get(start..end)
                .ok_or_else(|| truncated(start, len, self.bytes.len()))?;
            while cursor
                .checked_add(8)
                .is_some_and(|header_end| header_end <= end)
                && messages.len() < total_messages
            {
                let header = self.slice(cursor as u64, 8)?;
                let kind = u16::from_le_bytes([header[0], header[1]]);
                let size = u16::from_le_bytes([header[2], header[3]]) as usize;
                if header[4] & MESSAGE_FLAG_SHARED != 0 {
                    return Err(invalid(
                        cursor,
                        format!("HDF5 shared object header message {kind:#06x} unsupported"),
                    ));
                }
                let body_start = cursor
                    .checked_add(8)
                    .ok_or_else(|| invalid(cursor, "HDF5 message address overflow"))?;
                let body_end = body_start
                    .checked_add(size)
                    .ok_or_else(|| invalid(body_start, "HDF5 message size overflow"))?;
                if body_end > end {
                    return Err(truncated(cursor, 8 + size, end.saturating_sub(cursor)));
                }
                let body = self.slice(body_start as u64, size)?.to_vec();
                if kind == 0x0010 {
                    // Continuation: offset + length of the next block.
                    let offset = read_offset(&body, 0, self.offset_size)?;
                    let length = read_offset(&body, self.offset_size, self.length_size)?;
                    let offset = address_to_usize(offset)?;
                    let length = usize::try_from(length)
                        .map_err(|_| invalid(cursor, "HDF5 continuation length overflows usize"))?;
                    if length > MAX_OBJECT_MESSAGE_BYTES {
                        return Err(invalid(cursor, "HDF5 continuation block is too large"));
                    }
                    if blocks.len() >= MAX_HEADER_BLOCKS {
                        return Err(invalid(
                            address_to_usize(address)?,
                            "HDF5 object header has too many continuation blocks",
                        ));
                    }
                    if !scheduled_blocks.insert(offset) {
                        return Err(invalid(offset, "cycle in HDF5 object-header continuations"));
                    }
                    blocks.push((offset, length));
                } else {
                    message_bytes = message_bytes.checked_add(size).ok_or_else(|| {
                        invalid(cursor, "HDF5 object-header message size overflow")
                    })?;
                    if message_bytes > MAX_OBJECT_MESSAGE_BYTES {
                        return Err(invalid(
                            address_to_usize(address)?,
                            "HDF5 object header contains too much message data",
                        ));
                    }
                    messages.push(Message { kind, body });
                }
                cursor = body_end;
            }
        }
        Ok(ObjectHeader { messages })
    }

    /// Version 2 object header ("OHDR"), HDF5 spec section IV.A.2.
    ///
    /// Wire layout (all little-endian):
    /// `OHDR` (4) | version=2 (1) | flags (1) |
    /// [access/mod/change/birth times, 4×u32, when flags bit 5] |
    /// [max-compact/min-dense attribute counts, 2×u16, when flags bit 4] |
    /// size-of-chunk-0 (1/2/4/8 bytes per flags bits 0-1) | messages |
    /// checksum (u32, Jenkins lookup3 over the chunk from the signature on).
    ///
    /// Messages: type (u8 — v1 uses u16), size (u16), flags (u8),
    /// [creation order (u16) when header flags bit 2], body — with NO
    /// inter-message 8-byte alignment (v1 pads). A trailing gap smaller
    /// than one message header may precede the checksum. Continuation
    /// messages (0x0010) point at "OCHK" blocks: signature (4) | messages |
    /// checksum (u32), whose stored length INCLUDES signature and checksum.
    fn parse_object_header_v2(&self, address: u64) -> Result<ObjectHeader> {
        let head = self.slice(address, 6)?;
        let version = head[4];
        if version != 2 {
            return Err(invalid(
                address as usize,
                format!("OHDR object header version {version} unsupported (need 2)"),
            ));
        }
        let flags = head[5];
        let address = address_to_usize(address)?;
        let mut cursor = address
            .checked_add(6)
            .ok_or_else(|| invalid(address, "HDF5 v2 header address overflow"))?;
        if flags & 0x20 != 0 {
            cursor = cursor
                .checked_add(16)
                .ok_or_else(|| invalid(cursor, "HDF5 v2 timestamp fields overflow"))?;
        }
        if flags & 0x10 != 0 {
            cursor = cursor
                .checked_add(4)
                .ok_or_else(|| invalid(cursor, "HDF5 v2 attribute fields overflow"))?;
        }
        let size_width = 1usize << (flags & 0x03);
        let chunk0_size = usize::try_from(read_uint(self.bytes, cursor, size_width)?)
            .map_err(|_| invalid(cursor, "HDF5 v2 chunk size overflows usize"))?;
        if chunk0_size > MAX_OBJECT_MESSAGE_BYTES {
            return Err(invalid(cursor, "HDF5 v2 header message block is too large"));
        }
        cursor = cursor
            .checked_add(size_width)
            .ok_or_else(|| invalid(cursor, "HDF5 v2 header cursor overflow"))?;
        // Creation-order tracking widens every message header by 2 bytes.
        let message_header = if flags & 0x04 != 0 { 6 } else { 4 };
        let mut messages = Vec::new();
        // (message region start, message region length, chunk start for the
        // checksum). Chunk 0's checksummed span begins at the signature.
        let mut blocks = vec![(cursor, chunk0_size, address)];
        let mut scheduled_blocks = BTreeSet::from([address]);
        let mut block_index = 0;
        let mut message_bytes = 0usize;
        while block_index < blocks.len() {
            if blocks.len() > MAX_HEADER_BLOCKS {
                return Err(invalid(
                    address,
                    "HDF5 v2 header has too many continuation blocks",
                ));
            }
            let (start, len, chunk_start) = blocks[block_index];
            block_index += 1;
            let end = start
                .checked_add(len)
                .ok_or_else(|| invalid(start, "HDF5 v2 message block overflow"))?;
            if chunk_start > end {
                return Err(invalid(chunk_start, "invalid HDF5 v2 checksum span"));
            }
            let stored = self.slice(end as u64, 4)?;
            let stored = u32::from_le_bytes(stored.try_into().expect("4 bytes"));
            let computed = jenkins_lookup3(self.slice(chunk_start as u64, end - chunk_start)?);
            if stored != computed {
                return Err(invalid(
                    chunk_start,
                    format!(
                        "HDF5 v2 object header checksum mismatch (stored {stored:#010x}, computed {computed:#010x})"
                    ),
                ));
            }
            let mut cursor = start;
            // Stop on the trailing gap: any leftover space smaller than one
            // message header is padding before the checksum.
            while cursor
                .checked_add(message_header)
                .is_some_and(|header_end| header_end <= end)
            {
                let header = self.slice(cursor as u64, message_header)?;
                let kind = u16::from(header[0]);
                let size = u16::from_le_bytes([header[1], header[2]]) as usize;
                // header[3] = message flags; header[4..6] = creation order.
                if header[3] & MESSAGE_FLAG_SHARED != 0 {
                    return Err(invalid(
                        cursor,
                        format!("HDF5 shared object header message {kind:#06x} unsupported"),
                    ));
                }
                let body_start = cursor
                    .checked_add(message_header)
                    .ok_or_else(|| invalid(cursor, "HDF5 v2 message address overflow"))?;
                let body_end = body_start
                    .checked_add(size)
                    .ok_or_else(|| invalid(body_start, "HDF5 v2 message size overflow"))?;
                if body_end > end {
                    return Err(truncated(cursor, message_header + size, end - cursor));
                }
                let body = self.slice(body_start as u64, size)?.to_vec();
                if kind == 0x0010 {
                    let offset = address_to_usize(read_offset(&body, 0, self.offset_size)?)?;
                    let length =
                        usize::try_from(read_uint(&body, self.offset_size, self.length_size)?)
                            .map_err(|_| invalid(cursor, "HDF5 v2 continuation length overflow"))?;
                    if length < 8 {
                        return Err(invalid(cursor, "HDF5 v2 continuation block too short"));
                    }
                    if length > MAX_OBJECT_MESSAGE_BYTES {
                        return Err(invalid(cursor, "HDF5 v2 continuation block is too large"));
                    }
                    if self.slice(offset as u64, 4)? != OCHK_SIGNATURE {
                        return Err(invalid(offset, "expected OCHK signature"));
                    }
                    if !scheduled_blocks.insert(offset) {
                        return Err(invalid(offset, "cycle in HDF5 v2 header continuations"));
                    }
                    let message_start = offset
                        .checked_add(4)
                        .ok_or_else(|| invalid(offset, "HDF5 v2 continuation address overflow"))?;
                    // Message region excludes the signature and checksum.
                    blocks.push((message_start, length - 8, offset));
                } else {
                    if messages.len() >= MAX_OBJECT_MESSAGES {
                        return Err(invalid(
                            address,
                            "HDF5 v2 object header has too many messages",
                        ));
                    }
                    message_bytes = message_bytes
                        .checked_add(size)
                        .ok_or_else(|| invalid(cursor, "HDF5 v2 message byte count overflow"))?;
                    if message_bytes > MAX_OBJECT_MESSAGE_BYTES {
                        return Err(invalid(
                            address,
                            "HDF5 v2 object header contains too much message data",
                        ));
                    }
                    messages.push(Message { kind, body });
                }
                cursor = body_end;
            }
        }
        Ok(ObjectHeader { messages })
    }

    // ----- messages -----------------------------------------------------

    fn parse_dataspace(&self, body: &[u8]) -> Result<Vec<usize>> {
        let version = *body.first().ok_or_else(|| truncated(0, 1, 0))?;
        let rank = *body.get(1).ok_or_else(|| truncated(1, 1, body.len()))? as usize;
        if rank > MAX_DATASPACE_RANK {
            return Err(invalid(
                1,
                format!("HDF5 dataspace rank {rank} exceeds {MAX_DATASPACE_RANK}"),
            ));
        }
        let dims_start: usize = match version {
            1 => 8, // version, rank, flags, reserved[5]
            2 => 4, // version, rank, flags, type
            other => {
                return Err(invalid(0, format!("dataspace version {other} unsupported")));
            }
        };
        let mut dims = Vec::with_capacity(rank);
        for index in 0..rank {
            let at = index
                .checked_mul(self.length_size)
                .and_then(|value| dims_start.checked_add(value))
                .ok_or_else(|| invalid(dims_start, "HDF5 dataspace cursor overflow"))?;
            let dim = usize::try_from(read_offset(body, at, self.length_size)?)
                .map_err(|_| invalid(at, "HDF5 dimension overflows usize"))?;
            if dim > MAX_DATASPACE_DIM {
                return Err(invalid(
                    at,
                    format!("HDF5 dimension {dim} exceeds {MAX_DATASPACE_DIM}"),
                ));
            }
            dims.push(dim);
        }
        Ok(dims)
    }

    fn parse_datatype(&self, body: &[u8]) -> Result<Datatype> {
        if body.len() < 8 {
            return Err(truncated(0, 8, body.len()));
        }
        let class = body[0] & 0x0F;
        let bits = u32::from_le_bytes([body[1], body[2], body[3], 0]);
        let size = u32::from_le_bytes([body[4], body[5], body[6], body[7]]) as usize;
        let big_endian = bits & 1 != 0;
        match class {
            0 if (1..=8).contains(&size) => Ok(Datatype {
                class: DtClass::Int {
                    signed: bits & (1 << 3) != 0,
                },
                size,
                big_endian,
            }),
            1 if matches!(size, 4 | 8) => Ok(Datatype {
                class: DtClass::Float,
                size,
                big_endian,
            }),
            3 if size <= MAX_HDF5_ATTRIBUTE_BYTES => Ok(Datatype {
                class: DtClass::FixedString,
                size,
                big_endian: false,
            }),
            9 if bits & 0x0F == 1 && size <= MAX_HDF5_ATTRIBUTE_BYTES => Ok(Datatype {
                class: DtClass::VlenString,
                size,
                big_endian: false,
            }),
            other => Err(invalid(
                0,
                format!("HDF5 datatype class {other} unsupported"),
            )),
        }
    }

    fn parse_layout(&self, body: &[u8]) -> Result<Layout> {
        let version = *body.first().ok_or_else(|| truncated(0, 1, 0))?;
        if version != 3 {
            return Err(invalid(
                0,
                format!("data layout message version {version} unsupported (need v3)"),
            ));
        }
        let class = *body.get(1).ok_or_else(|| truncated(1, 1, body.len()))?;
        match class {
            0 => {
                let size = usize::from(read_le_u16(body, 2)?);
                if size > MAX_HDF5_DATASET_BYTES {
                    return Err(invalid(2, "HDF5 compact dataset is too large"));
                }
                let end = 4usize
                    .checked_add(size)
                    .ok_or_else(|| invalid(4, "HDF5 compact layout size overflow"))?;
                let data = body
                    .get(4..end)
                    .ok_or_else(|| truncated(4, size, body.len()))?;
                Ok(Layout::Compact(data.to_vec()))
            }
            1 => Ok(Layout::Contiguous {
                address: read_offset(body, 2, self.offset_size)?,
                size: read_offset(body, 2 + self.offset_size, self.length_size)?,
            }),
            2 => {
                let dimensionality =
                    *body.get(2).ok_or_else(|| truncated(2, 1, body.len()))? as usize;
                if dimensionality == 0 || dimensionality > MAX_DATASPACE_RANK + 1 {
                    return Err(invalid(2, "invalid HDF5 chunk dimensionality"));
                }
                let btree_address = read_offset(body, 3, self.offset_size)?;
                let mut chunk_dims = Vec::with_capacity(dimensionality);
                for index in 0..dimensionality {
                    let at = index
                        .checked_mul(4)
                        .and_then(|value| 3usize.checked_add(self.offset_size)?.checked_add(value))
                        .ok_or_else(|| invalid(3, "HDF5 chunk-dimension cursor overflow"))?;
                    let dim = body
                        .get(at..at + 4)
                        .ok_or_else(|| truncated(at, 4, body.len()))?;
                    let dim = u32::from_le_bytes(dim.try_into().expect("4 bytes")) as usize;
                    if dim == 0 || dim > MAX_DATASPACE_DIM {
                        return Err(invalid(at, "invalid HDF5 chunk dimension"));
                    }
                    chunk_dims.push(dim);
                }
                // The trailing entry is the element size; drop it.
                chunk_dims.pop();
                Ok(Layout::Chunked {
                    btree_address,
                    chunk_dims,
                })
            }
            other => Err(invalid(0, format!("data layout class {other} unsupported"))),
        }
    }

    fn parse_filter_pipeline(&self, body: &[u8]) -> Result<Vec<Filter>> {
        let version = *body.first().ok_or_else(|| truncated(0, 1, 0))?;
        let count = *body.get(1).ok_or_else(|| truncated(1, 1, body.len()))? as usize;
        if count > MAX_HDF5_FILTERS {
            return Err(invalid(
                1,
                format!("HDF5 filter count {count} exceeds {MAX_HDF5_FILTERS}"),
            ));
        }
        let mut filters = Vec::with_capacity(count);
        let mut cursor = match version {
            1 => 8,
            2 => 2,
            other => {
                return Err(invalid(
                    0,
                    format!("filter pipeline version {other} unsupported"),
                ));
            }
        };
        for _ in 0..count {
            let id = read_le_u16(body, cursor)?;
            let has_name = version == 1 || id >= 256;
            let name_len = if has_name {
                usize::from(read_le_u16(
                    body,
                    cursor
                        .checked_add(2)
                        .ok_or_else(|| invalid(cursor, "HDF5 filter cursor overflow"))?,
                )?)
            } else {
                0
            };
            let after_id = cursor
                .checked_add(if has_name { 4 } else { 2 })
                .ok_or_else(|| invalid(cursor, "HDF5 filter cursor overflow"))?;
            let value_count = usize::from(read_le_u16(
                body,
                after_id
                    .checked_add(2)
                    .ok_or_else(|| invalid(after_id, "HDF5 filter cursor overflow"))?,
            )?);
            if value_count > MAX_HDF5_FILTER_VALUES {
                return Err(invalid(after_id, "HDF5 filter has too many client values"));
            }
            let mut at = after_id
                .checked_add(4)
                .ok_or_else(|| invalid(after_id, "HDF5 filter cursor overflow"))?;
            if name_len > 0 {
                let padded_name = if version == 1 {
                    name_len
                        .checked_add(7)
                        .map(|value| value / 8 * 8)
                        .ok_or_else(|| invalid(at, "HDF5 filter name length overflow"))?
                } else {
                    name_len
                };
                checked_range(body, at, padded_name)?;
                at = at
                    .checked_add(padded_name)
                    .ok_or_else(|| invalid(at, "HDF5 filter name cursor overflow"))?;
            }
            let mut client_values = Vec::with_capacity(value_count);
            for index in 0..value_count {
                let value_at = index
                    .checked_mul(4)
                    .and_then(|value| at.checked_add(value))
                    .ok_or_else(|| invalid(at, "HDF5 filter value cursor overflow"))?;
                let v = checked_range(body, value_at, 4)?;
                client_values.push(u32::from_le_bytes(v.try_into().expect("4 bytes")));
            }
            at = value_count
                .checked_mul(4)
                .and_then(|value| at.checked_add(value))
                .ok_or_else(|| invalid(at, "HDF5 filter value length overflow"))?;
            if version == 1 && value_count % 2 == 1 {
                checked_range(body, at, 4)?;
                at = at
                    .checked_add(4)
                    .ok_or_else(|| invalid(at, "HDF5 filter padding overflow"))?;
            }
            filters.push(Filter { id, client_values });
            cursor = at;
        }
        Ok(filters)
    }

    /// Parse one attribute message body; returns the value when the
    /// attribute's name matches.
    fn parse_attribute(&self, body: &[u8], wanted: &str) -> Result<Option<H5Attr>> {
        let raw = self.split_attribute(body)?;
        if raw.name != wanted {
            return Ok(None);
        }
        self.decode_attribute(&raw).map(Some)
    }

    /// Split an attribute message into its name, datatype bytes, dimensions
    /// and element bytes, without deciding whether the datatype is one this
    /// reader can convert.
    ///
    /// Keeping the split separate from the conversion is what lets
    /// [`Self::attrs`] enumerate an object whose attributes include a
    /// datatype outside this parser's scope, and lets
    /// [`Self::attr_object_references`] read one such datatype in its own
    /// narrow way.
    fn split_attribute<'b>(&self, body: &'b [u8]) -> Result<RawAttribute<'b>> {
        let version = *body.first().ok_or_else(|| truncated(0, 1, 0))?;
        if !(1..=3).contains(&version) {
            return Err(invalid(
                0,
                format!("attribute version {version} unsupported"),
            ));
        }
        let header_len = if version == 3 { 9 } else { 8 };
        if body.len() < header_len {
            return Err(truncated(0, header_len, body.len()));
        }
        let flags = body[1];
        if version >= 2 && flags & 0x03 != 0 {
            return Err(invalid(
                0,
                "shared attribute datatype/dataspace unsupported",
            ));
        }
        let name_size = usize::from(read_le_u16(body, 2)?);
        let dt_size = usize::from(read_le_u16(body, 4)?);
        let ds_size = usize::from(read_le_u16(body, 6)?);
        let mut cursor = header_len;
        let pad = |len: usize| -> Result<usize> {
            if version == 1 {
                len.checked_add(7)
                    .map(|value| value / 8 * 8)
                    .ok_or_else(|| invalid(0, "HDF5 attribute padding overflow"))
            } else {
                Ok(len)
            }
        };
        let name_bytes = checked_range(body, cursor, name_size)?;
        let name = name_bytes
            .split(|byte| *byte == 0)
            .next()
            .map(String::from_utf8_lossy)
            .unwrap_or_default();
        cursor = cursor
            .checked_add(pad(name_size)?)
            .ok_or_else(|| invalid(cursor, "HDF5 attribute name cursor overflow"))?;
        let datatype = checked_range(body, cursor, dt_size)?;
        cursor = cursor
            .checked_add(pad(dt_size)?)
            .ok_or_else(|| invalid(cursor, "HDF5 attribute datatype cursor overflow"))?;
        let dims = self.parse_dataspace(checked_range(body, cursor, ds_size)?)?;
        cursor = cursor
            .checked_add(pad(ds_size)?)
            .ok_or_else(|| invalid(cursor, "HDF5 attribute dataspace cursor overflow"))?;
        let data = body
            .get(cursor..)
            .ok_or_else(|| truncated(cursor, 0, body.len()))?;
        Ok(RawAttribute {
            name: name.into_owned(),
            datatype,
            dims,
            data,
        })
    }

    fn decode_attribute(&self, raw: &RawAttribute<'_>) -> Result<H5Attr> {
        let dtype = self.parse_datatype(raw.datatype)?;
        let count = checked_product(&raw.dims, "HDF5 attribute element count")?.max(1);
        checked_allocation_bytes(
            count,
            dtype.size,
            MAX_HDF5_ATTRIBUTE_BYTES,
            "HDF5 attribute",
        )?;
        self.attr_value(&dtype, count, raw.data)
    }

    /// Decode a variable-length sequence of object references, one sequence
    /// per element of the attribute's dataspace.
    ///
    /// On disk each element is a length, then a global heap reference to the
    /// sequence's bytes; the bytes are object header addresses, since an
    /// HDF5 object reference IS the address of the object's header. A
    /// zero-length sequence carries no heap reference and comes back empty.
    fn decode_reference_sequence(&self, raw: &RawAttribute<'_>) -> Result<Vec<Vec<u64>>> {
        let head = checked_range(raw.datatype, 0, 8)?;
        if head[0] & 0x0F != 9 || head[1] & 0x0F != 0 {
            return Err(invalid(0, "attribute is not a variable-length sequence"));
        }
        let descriptor_size = u32::from_le_bytes([head[4], head[5], head[6], head[7]]) as usize;
        let base = raw
            .datatype
            .get(8..)
            .ok_or_else(|| truncated(8, 8, raw.datatype.len()))?;
        let base_head = checked_range(base, 0, 8)?;
        if base_head[0] & 0x0F != 7 {
            return Err(invalid(
                0,
                "sequence element type is not an object reference",
            ));
        }
        let reference_size =
            u32::from_le_bytes([base_head[4], base_head[5], base_head[6], base_head[7]]) as usize;
        if reference_size == 0 || reference_size > 8 {
            return Err(invalid(0, "object reference size is out of range"));
        }
        if descriptor_size < 4 + self.offset_size + 4 {
            return Err(invalid(0, "variable-length descriptor is too short"));
        }
        let count = checked_product(&raw.dims, "HDF5 attribute element count")?.max(1);
        if count > MAX_DATASPACE_RANK {
            return Err(invalid(0, "reference attribute has too many elements"));
        }
        let mut out = Vec::with_capacity(count);
        for index in 0..count {
            let element = checked_range(raw.data, index * descriptor_size, descriptor_size)?;
            let length = usize::try_from(read_uint(element, 0, 4)?)
                .map_err(|_| invalid(0, "reference sequence length overflows usize"))?;
            let collection = read_offset(element, 4, self.offset_size)?;
            if length == 0 || collection == 0 || collection == UNDEFINED_ADDR {
                out.push(Vec::new());
                continue;
            }
            if length > MAX_DATASPACE_RANK {
                return Err(invalid(0, "reference sequence is implausibly long"));
            }
            let heap_index = u32::try_from(read_uint(element, 4 + self.offset_size, 4)?)
                .map_err(|_| invalid(0, "global heap index overflows u32"))?;
            let bytes = self.global_heap_object(collection, heap_index)?;
            let mut addresses = Vec::with_capacity(length);
            for slot in 0..length {
                addresses.push(read_offset(&bytes, slot * reference_size, reference_size)?);
            }
            out.push(addresses);
        }
        Ok(out)
    }

    fn attr_value(&self, dtype: &Datatype, count: usize, data: &[u8]) -> Result<H5Attr> {
        match dtype.class {
            DtClass::FixedString => {
                let bytes = data.get(..dtype.size.min(data.len())).unwrap_or_default();
                let text = bytes.split(|byte| *byte == 0).next().unwrap_or_default();
                Ok(H5Attr::Str(String::from_utf8_lossy(text).into_owned()))
            }
            DtClass::VlenString => {
                // Element: u32 byte length + global heap reference
                // (collection address + u32 object index).
                if data.len() < 4 + self.offset_size + 4 {
                    return Err(truncated(0, 4 + self.offset_size + 4, data.len()));
                }
                let collection = read_offset(data, 4, self.offset_size)?;
                let index = u32::from_le_bytes(
                    data[4 + self.offset_size..4 + self.offset_size + 4]
                        .try_into()
                        .expect("4 bytes"),
                );
                let object = self.global_heap_object(collection, index)?;
                let text = object.split(|byte| *byte == 0).next().unwrap_or_default();
                Ok(H5Attr::Str(String::from_utf8_lossy(text).into_owned()))
            }
            DtClass::Int { signed } => {
                let mut values = Vec::with_capacity(count);
                for index in 0..count {
                    let raw = data
                        .get(index * dtype.size..(index + 1) * dtype.size)
                        .ok_or_else(|| truncated(index * dtype.size, dtype.size, data.len()))?;
                    values.push(read_int(raw, signed, dtype.big_endian));
                }
                Ok(if count == 1 {
                    H5Attr::I64(values[0])
                } else {
                    H5Attr::I64Array(values)
                })
            }
            DtClass::Float => {
                let mut values = Vec::with_capacity(count);
                for index in 0..count {
                    let raw = data
                        .get(index * dtype.size..(index + 1) * dtype.size)
                        .ok_or_else(|| truncated(index * dtype.size, dtype.size, data.len()))?;
                    values.push(read_float(raw, dtype.big_endian)?);
                }
                Ok(if count == 1 {
                    H5Attr::F64(values[0])
                } else {
                    H5Attr::F64Array(values)
                })
            }
        }
    }

    fn global_heap_object(&self, collection: u64, index: u32) -> Result<Vec<u8>> {
        let head = self.slice(collection, 8 + self.length_size)?;
        if &head[..4] != b"GCOL" {
            return Err(invalid(collection as usize, "expected GCOL signature"));
        }
        let total = read_offset(head, 8, self.length_size)? as usize;
        let mut cursor = collection as usize + 8 + self.length_size;
        let end = collection as usize + total;
        while cursor + 8 + self.length_size <= end {
            let object_index = u16::from_le_bytes([self.bytes[cursor], self.bytes[cursor + 1]]);
            let size = read_offset(self.bytes, cursor + 8, self.length_size)? as usize;
            if object_index == 0 {
                break; // free space marker terminates the collection
            }
            let data_start = cursor + 8 + self.length_size;
            if object_index as u32 == index {
                return Ok(self.slice(data_start as u64, size)?.to_vec());
            }
            cursor = data_start + size.div_ceil(8) * 8;
        }
        Err(invalid(
            collection as usize,
            format!("global heap object {index} not found"),
        ))
    }

    // ----- chunked data -------------------------------------------------

    /// Assemble a chunked dataset.
    ///
    /// A chunk that was never written has no record in the index — HDF5
    /// allocates chunks on first write — so the buffer starts as the fill
    /// value everywhere and only the chunks the file actually carries are
    /// copied over it. Starting from zero instead would return 0.0 for
    /// every unallocated chunk, which for a radar moment is an echo the
    /// file never recorded.
    fn read_chunked(
        &self,
        btree_address: u64,
        chunk_dims: &[usize],
        dims: &[usize],
        element_size: usize,
        filters: &[Filter],
        fill: Option<&[u8]>,
    ) -> Result<Vec<u8>> {
        if chunk_dims.len() != dims.len() {
            return Err(invalid(
                0,
                "HDF5 chunk dimensionality does not match dataset rank",
            ));
        }
        let elements = checked_product(dims, "HDF5 chunked dataset element count")?;
        let total = checked_allocation_bytes(
            elements,
            element_size,
            MAX_HDF5_DATASET_BYTES,
            "HDF5 chunked dataset",
        )?;
        let mut out = filled_buffer(total, fill);
        if btree_address == UNDEFINED_ADDR {
            return Ok(out); // dataset never written: fill value throughout
        }
        let mut chunks = Vec::new();
        let mut visited_nodes = BTreeSet::new();
        self.collect_chunks(
            btree_address,
            chunk_dims.len() + 1,
            &mut chunks,
            &mut visited_nodes,
            0,
        )?;
        let chunk_elements = checked_product(chunk_dims, "HDF5 chunk element count")?;
        let chunk_bytes = checked_allocation_bytes(
            chunk_elements,
            element_size,
            MAX_HDF5_DATASET_BYTES,
            "HDF5 chunk",
        )?;
        for chunk in chunks {
            if chunk.stored_size > MAX_HDF5_DATASET_BYTES {
                return Err(invalid(
                    address_to_usize(chunk.address)?,
                    "HDF5 stored chunk is too large",
                ));
            }
            let stored = self.slice(chunk.address, chunk.stored_size)?;
            let raw = apply_inverse_filters(
                stored,
                filters,
                chunk.filter_mask,
                element_size,
                chunk_bytes,
            )?;
            if raw.len() < chunk_bytes {
                return Err(invalid(
                    chunk.address as usize,
                    "decoded chunk shorter than chunk dimensions",
                ));
            }
            copy_chunk(
                &mut out,
                &raw,
                dims,
                chunk_dims,
                &chunk.offsets,
                element_size,
            );
        }
        Ok(out)
    }

    fn collect_chunks(
        &self,
        node_address: u64,
        key_dims: usize,
        out: &mut Vec<ChunkRef>,
        visited: &mut BTreeSet<u64>,
        depth: usize,
    ) -> Result<()> {
        if depth > MAX_BTREE_DEPTH {
            return Err(invalid(0, "HDF5 chunk B-tree nested too deep"));
        }
        if !visited.insert(node_address) {
            return Err(invalid(
                address_to_usize(node_address)?,
                "cycle in HDF5 chunk B-tree",
            ));
        }
        if visited.len() > MAX_BTREE_NODES {
            return Err(invalid(0, "HDF5 chunk B-tree too large"));
        }
        let node = self.slice(node_address, 8 + 2 * self.offset_size)?;
        if &node[..4] != b"TREE" {
            return Err(invalid(node_address as usize, "expected TREE signature"));
        }
        if node[4] != 1 {
            return Err(invalid(node_address as usize, "expected chunk B-tree node"));
        }
        let level = node[5];
        let entries = u16::from_le_bytes([node[6], node[7]]) as usize;
        let key_size = 8 + 8 * key_dims;
        let mut cursor = address_to_usize(node_address)?
            .checked_add(8 + 2 * self.offset_size)
            .ok_or_else(|| invalid(0, "HDF5 chunk B-tree cursor overflow"))?;
        for _ in 0..entries {
            let key = self.slice(cursor as u64, key_size)?;
            let stored_size = u32::from_le_bytes(key[..4].try_into().expect("4 bytes")) as usize;
            let filter_mask = u32::from_le_bytes(key[4..8].try_into().expect("4 bytes"));
            let mut offsets = Vec::with_capacity(key_dims.saturating_sub(1));
            for dim in 0..key_dims.saturating_sub(1) {
                let at = 8 + dim * 8;
                let offset = u64::from_le_bytes(key[at..at + 8].try_into().expect("8 bytes"));
                offsets.push(usize::try_from(offset).map_err(|_| {
                    invalid(
                        address_to_usize(node_address).unwrap_or(0),
                        "HDF5 chunk offset overflows usize",
                    )
                })?);
            }
            cursor = cursor
                .checked_add(key_size)
                .ok_or_else(|| invalid(cursor, "HDF5 chunk B-tree key overflow"))?;
            let child = read_offset(self.bytes, cursor, self.offset_size)?;
            cursor = cursor
                .checked_add(self.offset_size)
                .ok_or_else(|| invalid(cursor, "HDF5 chunk B-tree child overflow"))?;
            if level == 0 {
                if out.len() >= MAX_DATA_CHUNKS {
                    return Err(invalid(0, "HDF5 dataset has too many chunks"));
                }
                out.push(ChunkRef {
                    address: child,
                    stored_size,
                    filter_mask,
                    offsets,
                });
            } else {
                self.collect_chunks(child, key_dims, out, visited, depth + 1)?;
            }
        }
        Ok(())
    }

    fn slice(&self, address: u64, len: usize) -> Result<&'a [u8]> {
        let start = address_to_usize(address)?;
        let end = start
            .checked_add(len)
            .ok_or_else(|| invalid(start, "HDF5 byte range overflow"))?;
        self.bytes
            .get(start..end)
            .ok_or_else(|| truncated(start, len, self.bytes.len()))
    }
}

/// Signature, version, type and checksum: the bytes of a version 2 B-tree
/// node that are not records or child pointers.
const BTREE_V2_NODE_PREFIX: usize = 10;

/// One managed direct block of a fractal heap, resolved to a file address.
struct HeapBlock {
    /// Offset of the block's FIRST byte in the heap's linear address space.
    /// That space counts block headers as well as object bytes, which is why
    /// a heap ID's offset can be subtracted from this and added to
    /// [`Self::address`] directly.
    heap_offset: u64,
    size: usize,
    address: u64,
}

/// A fractal heap, read far enough to fetch objects by heap ID.
struct FractalHeap {
    /// Heap ID width in bytes, as the heap header declares it. Dense link
    /// storage uses 7, dense attribute storage 8, so the record readers ask
    /// the heap rather than assuming either.
    heap_id_len: usize,
    id_offset_bytes: usize,
    id_length_bytes: usize,
    checksummed_blocks: bool,
    table_width: usize,
    starting_block_size: u64,
    /// First doubling-table row whose entries are indirect blocks.
    max_direct_rows: usize,
    blocks: Vec<HeapBlock>,
}

impl FractalHeap {
    /// Block size of doubling-table row `row`: rows 0 and 1 are the starting
    /// size and every row after doubles (HDF5 spec, "Fractal Heap Header",
    /// Table Width / Starting Block Size).
    fn row_block_size(&self, row: usize) -> Result<u64> {
        if row < 2 {
            return Ok(self.starting_block_size);
        }
        let shift =
            u32::try_from(row - 1).map_err(|_| invalid(0, "fractal heap row index overflow"))?;
        self.starting_block_size
            .checked_shl(shift)
            .filter(|size| *size >> shift == self.starting_block_size)
            .ok_or_else(|| invalid(0, "fractal heap row size overflow"))
    }
}

/// The per-depth field widths a version 2 B-tree's child pointers use.
///
/// HDF5 does not store these: it recomputes them from the node size, the
/// record size and the tree depth, so a reader has to reproduce the same
/// arithmetic or it will read child pointers at the wrong stride. This is
/// `H5B2_hdr_init`'s `node_info` table (H5B2hdr.c), narrowed to the two
/// widths a walk needs.
struct BTreeV2Shape {
    record_size: usize,
    max_records_per_node: usize,
    /// Width of a child's "number of records in this node" field.
    max_nrec_size: usize,
    /// Per depth, the width of a child's "records in this subtree" field.
    cumulative: Vec<usize>,
}

impl BTreeV2Shape {
    fn new(node_size: usize, record_size: usize, depth: usize, offset_size: usize) -> Result<Self> {
        let leaf_max = (node_size - BTREE_V2_NODE_PREFIX) / record_size;
        if leaf_max == 0 {
            return Err(invalid(0, "version 2 B-tree leaf holds no records"));
        }
        let max_nrec_size = limit_enc_size(leaf_max as u64);
        let mut cumulative = vec![limit_enc_size(leaf_max as u64)];
        let mut cumulative_records = leaf_max as u64;
        let mut max_records_per_node = leaf_max;
        for _ in 1..=depth {
            let per_child = record_size
                .checked_add(offset_size)
                .and_then(|value| value.checked_add(*cumulative.last().expect("seeded")))
                .ok_or_else(|| invalid(0, "version 2 B-tree child stride overflow"))?;
            let available = node_size
                .checked_sub(BTREE_V2_NODE_PREFIX + max_nrec_size)
                .filter(|available| *available >= per_child)
                .ok_or_else(|| invalid(0, "version 2 B-tree internal node holds no records"))?;
            let internal_max = available / per_child;
            max_records_per_node = max_records_per_node.max(internal_max);
            // Saturating on purpose: an absurd depth saturates the count,
            // which pins the encoded width at its 8-byte maximum — exactly
            // what HDF5's own `H5VM_limit_enc_size` does with a huge limit.
            cumulative_records = (internal_max as u64)
                .saturating_add(1)
                .saturating_mul(cumulative_records)
                .saturating_add(internal_max as u64);
            cumulative.push(limit_enc_size(cumulative_records));
        }
        Ok(Self {
            record_size,
            max_records_per_node,
            max_nrec_size,
            cumulative,
        })
    }

    fn cumulative_nrec_size(&self, depth: usize) -> Result<usize> {
        self.cumulative
            .get(depth)
            .copied()
            .ok_or_else(|| invalid(0, "version 2 B-tree depth outside its own shape"))
    }
}

/// Bytes HDF5 uses to encode a count that cannot exceed `value`
/// (`H5VM_limit_enc_size`).
fn limit_enc_size(value: u64) -> usize {
    (log2_floor(value) / 8) + 1
}

/// `floor(log2(value))`, with zero mapped to zero the way HDF5's
/// `H5VM_log2_gen` does.
fn log2_floor(value: u64) -> usize {
    if value == 0 {
        0
    } else {
        63 - value.leading_zeros() as usize
    }
}

/// An attribute message split into its parts, before anything decides
/// whether the datatype is one this reader converts.
struct RawAttribute<'b> {
    name: String,
    datatype: &'b [u8],
    dims: Vec<usize>,
    data: &'b [u8],
}

struct ObjectHeader {
    messages: Vec<Message>,
}

struct Message {
    kind: u16,
    body: Vec<u8>,
}

#[derive(Clone, Copy, Debug)]
enum DtClass {
    Int { signed: bool },
    Float,
    FixedString,
    VlenString,
}

#[derive(Clone, Copy, Debug)]
struct Datatype {
    class: DtClass,
    size: usize,
    big_endian: bool,
}

impl Datatype {
    /// Convert a raw element buffer into the closest [`H5Data`] storage.
    fn convert(&self, raw: &[u8]) -> Result<H5Data> {
        match self.class {
            DtClass::Int { signed: false } if self.size == 1 => Ok(H5Data::U8(raw.to_vec())),
            // netCDF-4 stores NC_CHAR as a one-byte fixed string, with the
            // string length carried as the array's last dimension — so a
            // CfRadial `sweep_mode(sweep, string_length_32)` arrives here as
            // a (sweep x 32) array of size-1 strings. The bytes ARE the
            // characters.
            DtClass::FixedString if self.size == 1 => Ok(H5Data::Chars(raw.to_vec())),
            DtClass::Int { signed: false } if self.size == 2 => Ok(H5Data::U16(
                raw.chunks_exact(2)
                    .map(|pair| {
                        if self.big_endian {
                            u16::from_be_bytes([pair[0], pair[1]])
                        } else {
                            u16::from_le_bytes([pair[0], pair[1]])
                        }
                    })
                    .collect(),
            )),
            DtClass::Int { signed } => Ok(H5Data::F64(
                raw.chunks_exact(self.size)
                    .map(|chunk| read_int(chunk, signed, self.big_endian) as f64)
                    .collect(),
            )),
            DtClass::Float if self.size == 4 => Ok(H5Data::F32(
                raw.chunks_exact(4)
                    .map(|quad| {
                        let bits = if self.big_endian {
                            u32::from_be_bytes(quad.try_into().expect("4 bytes"))
                        } else {
                            u32::from_le_bytes(quad.try_into().expect("4 bytes"))
                        };
                        f32::from_bits(bits)
                    })
                    .collect(),
            )),
            DtClass::Float if self.size == 8 => Ok(H5Data::F64(
                raw.chunks_exact(8)
                    .map(|oct| {
                        let bits = if self.big_endian {
                            u64::from_be_bytes(oct.try_into().expect("8 bytes"))
                        } else {
                            u64::from_le_bytes(oct.try_into().expect("8 bytes"))
                        };
                        f64::from_bits(bits)
                    })
                    .collect(),
            )),
            _ => Err(invalid(0, "unsupported dataset element type")),
        }
    }
}

enum Layout {
    Compact(Vec<u8>),
    Contiguous {
        address: u64,
        size: u64,
    },
    Chunked {
        btree_address: u64,
        chunk_dims: Vec<usize>,
    },
}

struct Filter {
    id: u16,
    client_values: Vec<u32>,
}

struct ChunkRef {
    address: u64,
    stored_size: usize,
    filter_mask: u32,
    offsets: Vec<usize>,
}

/// Run the inverse filter pipeline over one stored chunk. Filters apply in
/// reverse pipeline order on read: deflate (id 1) inflates, shuffle (id 2)
/// de-interleaves byte planes. `filter_mask` bit N set = filter N skipped.
fn apply_inverse_filters(
    stored: &[u8],
    filters: &[Filter],
    filter_mask: u32,
    element_size: usize,
    max_output: usize,
) -> Result<Vec<u8>> {
    if stored.len() > MAX_HDF5_DATASET_BYTES {
        return Err(invalid(0, "HDF5 stored filter input is too large"));
    }
    let mut data = stored.to_vec();
    for (index, filter) in filters.iter().enumerate().rev() {
        if filter_mask & (1 << index) != 0 {
            continue;
        }
        match filter.id {
            1 => {
                // gzip/deflate (zlib stream per the HDF5 deflate filter).
                let mut decoder = ZlibDecoder::new(&data[..]);
                let mut inflated = Vec::new();
                let mut chunk = [0u8; 64 * 1024];
                loop {
                    let remaining = max_output.saturating_sub(inflated.len());
                    if remaining == 0 {
                        let mut probe = [0u8; 1];
                        let count = decoder
                            .read(&mut probe)
                            .map_err(|err| invalid(0, format!("HDF5 deflate chunk: {err}")))?;
                        if count != 0 {
                            return Err(invalid(
                                0,
                                format!("HDF5 deflate chunk expands beyond {max_output} bytes"),
                            ));
                        }
                        break;
                    }
                    let read_len = remaining.min(chunk.len());
                    let count = decoder
                        .read(&mut chunk[..read_len])
                        .map_err(|err| invalid(0, format!("HDF5 deflate chunk: {err}")))?;
                    if count == 0 {
                        break;
                    }
                    inflated.try_reserve(count).map_err(|err| {
                        invalid(0, format!("cannot reserve HDF5 deflate output: {err}"))
                    })?;
                    inflated.extend_from_slice(&chunk[..count]);
                }
                data = inflated;
            }
            2 => {
                let size = filter
                    .client_values
                    .first()
                    .copied()
                    .map(|v| v as usize)
                    .unwrap_or(element_size)
                    .max(1);
                data = unshuffle(&data, size);
            }
            other => {
                return Err(invalid(0, format!("HDF5 filter id {other} unsupported")));
            }
        }
        if data.len() > max_output {
            return Err(invalid(
                0,
                format!("HDF5 filter output exceeds {max_output} bytes"),
            ));
        }
    }
    if data.len() > max_output {
        return Err(invalid(
            0,
            format!("HDF5 filter output exceeds {max_output} bytes"),
        ));
    }
    Ok(data)
}

/// The one element's worth of bytes a dataset reads back where the file
/// wrote nothing, from its fill value message.
///
/// HDF5 stores the fill value in the dataset's OWN datatype and byte order
/// (HDF5 File Format Specification v3.0, section IV.A.2.f "Fill Value
/// (old)" and IV.A.2.g "Fill Value"), so the bytes returned here are an
/// element pattern that can be repeated across a buffer as-is.
///
/// `None` means "no fill value in this file", and the HDF5 default applies:
/// all zero bytes. That default is not a guess — it is what the library
/// itself returns for `H5D_FILL_VALUE_DEFAULT` — and it is also what a
/// dataset marked `H5D_FILL_VALUE_UNDEFINED` gets here, where HDF5 promises
/// nothing about the contents and zero is as good an answer as any.
///
/// The three versions of message 0x0005 differ only in how they say whether
/// a value follows:
///
/// * v1 always carries the size and the value.
/// * v2 carries them only when its "fill value defined" byte is 1.
/// * v3 replaces the three timing/defined bytes with one flags byte, and
///   carries them only when bit 5 ("fill value defined") is set; bit 4 is
///   the "undefined" marker.
///
/// The deprecated message 0x0004 is a bare size and value with no version
/// byte at all. Nothing this decade writes it, but reading it costs two
/// lines and not reading it would leave exactly one more way for a file to
/// be silently misread as zeros.
fn parse_fill_value(kind: u16, body: &[u8]) -> Result<Option<Vec<u8>>> {
    /// Size (4 bytes) followed by that many value bytes.
    fn sized_value(body: &[u8], at: usize) -> Result<Option<Vec<u8>>> {
        let size = u32::from_le_bytes(checked_range(body, at, 4)?.try_into().expect("4 bytes"));
        let size = usize::try_from(size).map_err(|_| invalid(at, "HDF5 fill value too large"))?;
        if size == 0 {
            return Ok(None); // declared, but empty: the default fill
        }
        if size > MAX_HDF5_ATTRIBUTE_BYTES {
            return Err(invalid(
                at,
                format!("HDF5 fill value of {size} bytes is implausible"),
            ));
        }
        Ok(Some(checked_range(body, at + 4, size)?.to_vec()))
    }

    if kind == 0x0004 {
        return sized_value(body, 0);
    }
    match read_u8(body, 0)? {
        1 => sized_value(body, 4),
        2 => {
            if read_u8(body, 3)? == 1 {
                sized_value(body, 4)
            } else {
                Ok(None)
            }
        }
        3 => {
            const FILL_VALUE_DEFINED: u8 = 1 << 5;
            if read_u8(body, 1)? & FILL_VALUE_DEFINED != 0 {
                sized_value(body, 2)
            } else {
                Ok(None)
            }
        }
        version => Err(invalid(
            0,
            format!("HDF5 fill value message version {version} unsupported"),
        )),
    }
}

/// A `byte_len` buffer holding `fill` repeated end to end, or zeros when the
/// dataset defines no fill value.
///
/// `fill` is one element wide (the caller checks that against the datatype),
/// so repeating it is exactly what HDF5 does when it materialises a plane
/// the writer never touched.
fn filled_buffer(byte_len: usize, fill: Option<&[u8]>) -> Vec<u8> {
    let mut out = vec![0u8; byte_len];
    let Some(pattern) = fill else {
        return out;
    };
    if pattern.is_empty() || pattern.iter().all(|byte| *byte == 0) {
        return out; // the zeros are already right
    }
    for element in out.chunks_mut(pattern.len()) {
        let width = element.len().min(pattern.len());
        element[..width].copy_from_slice(&pattern[..width]);
    }
    out
}

/// Inverse of the HDF5 shuffle filter: byte plane k holds byte k of every
/// element; re-interleave.
fn unshuffle(data: &[u8], element_size: usize) -> Vec<u8> {
    if element_size <= 1 || !data.len().is_multiple_of(element_size) {
        return data.to_vec();
    }
    let count = data.len() / element_size;
    let mut out = vec![0u8; data.len()];
    for plane in 0..element_size {
        for element in 0..count {
            out[element * element_size + plane] = data[plane * count + element];
        }
    }
    out
}

/// Copy one decoded chunk into the dataset buffer, clipping edge chunks.
fn copy_chunk(
    out: &mut [u8],
    chunk: &[u8],
    dims: &[usize],
    chunk_dims: &[usize],
    offsets: &[usize],
    element_size: usize,
) {
    // Treat the dataset as (outer, row) where row = innermost dimension —
    // sufficient for the 1-D/2-D arrays polar volumes use; higher ranks
    // copy via the same row loop with composite outer indices.
    let rank = dims.len();
    if rank == 0 || chunk_dims.len() != rank || offsets.len() < rank {
        return;
    }
    let row_len = dims[rank - 1];
    let chunk_row_len = chunk_dims[rank - 1];
    let row_offset = offsets[rank - 1];
    let copy_cols = chunk_row_len.min(row_len.saturating_sub(row_offset));
    if copy_cols == 0 {
        return;
    }
    // Number of rows in the chunk = product of all but the last chunk dim.
    let chunk_rows: usize = chunk_dims[..rank - 1].iter().product::<usize>().max(1);
    for chunk_row in 0..chunk_rows {
        // Decompose the chunk row into per-dimension indices.
        let mut remaining = chunk_row;
        let mut out_index = 0usize;
        let mut in_bounds = true;
        for dim in 0..rank - 1 {
            let stride: usize = chunk_dims[dim + 1..rank - 1]
                .iter()
                .product::<usize>()
                .max(1);
            let local = remaining / stride;
            remaining %= stride;
            let global = offsets[dim] + local;
            if global >= dims[dim] {
                in_bounds = false;
                break;
            }
            let out_stride: usize = dims[dim + 1..].iter().product();
            out_index += global * out_stride;
        }
        if !in_bounds {
            continue;
        }
        out_index += row_offset;
        let src = chunk_row * chunk_row_len * element_size;
        let dst = out_index * element_size;
        let len = copy_cols * element_size;
        if src + len <= chunk.len() && dst + len <= out.len() {
            out[dst..dst + len].copy_from_slice(&chunk[src..src + len]);
        }
    }
}

fn heap_string(bytes: &[u8], heap_data: u64, name_offset: u64) -> Result<String> {
    let start = (heap_data + name_offset) as usize;
    let tail = bytes
        .get(start..)
        .ok_or_else(|| truncated(start, 1, bytes.len()))?;
    let name = tail.split(|byte| *byte == 0).next().unwrap_or_default();
    Ok(String::from_utf8_lossy(name).into_owned())
}

fn read_u8(bytes: &[u8], at: usize) -> Result<u8> {
    bytes
        .get(at)
        .copied()
        .ok_or_else(|| truncated(at, 1, bytes.len()))
}

fn checked_range(bytes: &[u8], at: usize, len: usize) -> Result<&[u8]> {
    let end = at
        .checked_add(len)
        .ok_or_else(|| invalid(at, "HDF5 byte range overflow"))?;
    bytes
        .get(at..end)
        .ok_or_else(|| truncated(at, len, bytes.len()))
}

fn read_le_u16(bytes: &[u8], at: usize) -> Result<u16> {
    let raw = checked_range(bytes, at, 2)?;
    Ok(u16::from_le_bytes([raw[0], raw[1]]))
}

fn address_to_usize(address: u64) -> Result<usize> {
    usize::try_from(address).map_err(|_| invalid(0, "HDF5 address overflows usize"))
}

fn checked_product(values: &[usize], context: &'static str) -> Result<usize> {
    values.iter().try_fold(1usize, |product, value| {
        product
            .checked_mul(*value)
            .ok_or_else(|| invalid(0, format!("{context} overflow")))
    })
}

fn checked_allocation_bytes(
    count: usize,
    element_size: usize,
    limit: usize,
    context: &'static str,
) -> Result<usize> {
    let bytes = count
        .checked_mul(element_size)
        .ok_or_else(|| invalid(0, format!("{context} byte-size overflow")))?;
    if bytes > limit {
        return Err(invalid(
            0,
            format!("{context} requires {bytes} bytes (limit {limit})"),
        ));
    }
    Ok(bytes)
}

/// Little-endian unsigned integer of `size` bytes (HDF5 metadata is always
/// little-endian).
fn read_offset(bytes: &[u8], at: usize, size: usize) -> Result<u64> {
    let raw = checked_range(bytes, at, size)?;
    let mut value = 0u64;
    for (index, byte) in raw.iter().enumerate() {
        value |= u64::from(*byte) << (8 * index);
    }
    // Map a size-4 undefined address (all ones) to the canonical sentinel.
    if size < 8 && value == (1u64 << (8 * size)) - 1 {
        return Ok(UNDEFINED_ADDR);
    }
    Ok(value)
}

/// Little-endian unsigned integer of `size` bytes WITHOUT the undefined-
/// address sentinel mapping of [`read_offset`] — for sizes and lengths,
/// where an all-ones value is a value, not "undefined".
fn read_uint(bytes: &[u8], at: usize, size: usize) -> Result<u64> {
    let raw = checked_range(bytes, at, size)?;
    let mut value = 0u64;
    for (index, byte) in raw.iter().enumerate() {
        value |= u64::from(*byte) << (8 * index);
    }
    Ok(value)
}

/// Bob Jenkins' lookup3 `hashlittle` over little-endian words — the
/// H5_checksum_lookup3 metadata checksum used by v2 object headers and
/// their continuation blocks (and other 1.8+ structures).
fn jenkins_lookup3(data: &[u8]) -> u32 {
    let init = 0xdead_beef_u32.wrapping_add(data.len() as u32);
    let (mut a, mut b, mut c) = (init, init, init);
    let word = |chunk: &[u8]| u32::from_le_bytes(chunk.try_into().expect("4 bytes"));
    let mut rest = data;
    while rest.len() > 12 {
        a = a.wrapping_add(word(&rest[0..4]));
        b = b.wrapping_add(word(&rest[4..8]));
        c = c.wrapping_add(word(&rest[8..12]));
        // mix(a, b, c)
        a = a.wrapping_sub(c) ^ c.rotate_left(4);
        c = c.wrapping_add(b);
        b = b.wrapping_sub(a) ^ a.rotate_left(6);
        a = a.wrapping_add(c);
        c = c.wrapping_sub(b) ^ b.rotate_left(8);
        b = b.wrapping_add(a);
        a = a.wrapping_sub(c) ^ c.rotate_left(16);
        c = c.wrapping_add(b);
        b = b.wrapping_sub(a) ^ a.rotate_left(19);
        a = a.wrapping_add(c);
        c = c.wrapping_sub(b) ^ b.rotate_left(4);
        b = b.wrapping_add(a);
        rest = &rest[12..];
    }
    if rest.is_empty() {
        // hashlittle: a zero-length tail skips the final mix entirely.
        return c;
    }
    // The 1..=12 byte tail reads as three zero-padded words (the C switch
    // adds only the bytes present, which is the same thing).
    let mut tail = [0u8; 12];
    tail[..rest.len()].copy_from_slice(rest);
    a = a.wrapping_add(word(&tail[0..4]));
    b = b.wrapping_add(word(&tail[4..8]));
    c = c.wrapping_add(word(&tail[8..12]));
    // final(a, b, c)
    c = (c ^ b).wrapping_sub(b.rotate_left(14));
    a = (a ^ c).wrapping_sub(c.rotate_left(11));
    b = (b ^ a).wrapping_sub(a.rotate_left(25));
    c = (c ^ b).wrapping_sub(b.rotate_left(16));
    a = (a ^ c).wrapping_sub(c.rotate_left(4));
    b = (b ^ a).wrapping_sub(a.rotate_left(14));
    c = (c ^ b).wrapping_sub(b.rotate_left(24));
    c
}

fn read_int(raw: &[u8], signed: bool, big_endian: bool) -> i64 {
    let mut value = 0u64;
    if big_endian {
        for byte in raw {
            value = (value << 8) | u64::from(*byte);
        }
    } else {
        for (index, byte) in raw.iter().enumerate() {
            value |= u64::from(*byte) << (8 * index);
        }
    }
    if signed && !raw.is_empty() && raw.len() < 8 {
        let sign_bit = 1u64 << (8 * raw.len() - 1);
        if value & sign_bit != 0 {
            value |= !((1u64 << (8 * raw.len())) - 1);
        }
    }
    value as i64
}

fn read_float(raw: &[u8], big_endian: bool) -> Result<f64> {
    match raw.len() {
        4 => {
            let bits = if big_endian {
                u32::from_be_bytes(raw.try_into().expect("4 bytes"))
            } else {
                u32::from_le_bytes(raw.try_into().expect("4 bytes"))
            };
            Ok(f64::from(f32::from_bits(bits)))
        }
        8 => {
            let bits = if big_endian {
                u64::from_be_bytes(raw.try_into().expect("8 bytes"))
            } else {
                u64::from_le_bytes(raw.try_into().expect("8 bytes"))
            };
            Ok(f64::from_bits(bits))
        }
        other => Err(invalid(0, format!("float width {other} unsupported"))),
    }
}

fn invalid(offset: usize, reason: impl Into<String>) -> NexradError {
    NexradError::InvalidMessage {
        offset,
        reason: reason.into(),
    }
}

fn truncated(offset: usize, needed: usize, available: usize) -> NexradError {
    NexradError::Truncated {
        what: "HDF5 structure",
        offset,
        needed,
        available,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parser(bytes: &[u8]) -> H5File<'_> {
        H5File {
            bytes,
            offset_size: 8,
            length_size: 8,
            objects: BTreeMap::new(),
            budget_total: MAX_HDF5_TOTAL_DATASET_BYTES,
            budget_left: Cell::new(MAX_HDF5_TOTAL_DATASET_BYTES),
        }
    }

    #[test]
    fn magic_sniffer_matches_signature_only() {
        assert!(looks_like_hdf5_bytes(b"\x89HDF\r\n\x1a\nrest"));
        assert!(!looks_like_hdf5_bytes(b"\x89HDF\r\n\x1a"));
        assert!(!looks_like_hdf5_bytes(b"CDF\x01...."));
        assert!(!looks_like_hdf5_bytes(b"AR2V0006."));
    }

    #[test]
    fn unshuffle_reinterleaves_byte_planes() {
        // Two u16 elements 0x0201, 0x0403 shuffled = planes [01 03][02 04].
        let shuffled = [0x01, 0x03, 0x02, 0x04];
        assert_eq!(unshuffle(&shuffled, 2), vec![0x01, 0x02, 0x03, 0x04]);
        // Non-multiple lengths pass through untouched.
        assert_eq!(unshuffle(&[1, 2, 3], 2), vec![1, 2, 3]);
    }

    #[test]
    fn read_int_sign_extends_little_and_big_endian() {
        assert_eq!(read_int(&[0xFF], true, false), -1);
        assert_eq!(read_int(&[0xFF], false, false), 255);
        assert_eq!(read_int(&[0xFE, 0xFF], true, false), -2);
        assert_eq!(read_int(&[0xFF, 0xFE], true, true), -2);
        assert_eq!(read_int(&[0x2A, 0, 0, 0, 0, 0, 0, 0], false, false), 42);
    }

    #[test]
    fn undefined_addresses_normalize_across_offset_sizes() {
        assert_eq!(
            read_offset(&[0xFF, 0xFF, 0xFF, 0xFF], 0, 4).unwrap(),
            UNDEFINED_ADDR
        );
        assert_eq!(read_offset(&[0x10, 0, 0, 0], 0, 4).unwrap(), 0x10);
    }

    /// Unlike `read_offset`, `read_uint` must NOT map all-ones to the
    /// undefined sentinel — 0xFF is a legal 1-byte chunk size.
    #[test]
    fn read_uint_keeps_all_ones_values() {
        assert_eq!(read_uint(&[0xFF], 0, 1).unwrap(), 0xFF);
        assert_eq!(read_uint(&[0xFF, 0xFF], 0, 2).unwrap(), 0xFFFF);
        assert_eq!(read_uint(&[0x83, 0x01], 0, 2).unwrap(), 0x0183);
    }

    #[test]
    fn truncated_messages_return_errors_instead_of_indexing() {
        let file = parser(&[]);
        assert!(file.parse_attribute(&[1], "name").is_err());
        let Err(_) = file.parse_layout(&[3, 0]) else {
            panic!("truncated compact layout must fail");
        };
        let Err(_) = file.parse_filter_pipeline(&[1, 1]) else {
            panic!("truncated filter pipeline must fail");
        };
    }

    /// An attribute this reader cannot split does not hide the ones after
    /// it.
    ///
    /// `DIMENSION_LIST` is how a netCDF-4 variable names its axes, and a
    /// variable that comes back with no dimension ids drops out of the
    /// `(time, range)` field filter and vanishes from the volume — with no
    /// error, because losing a field is not something the decode can see.
    /// So the search past an undecodable attribute has to keep going, the
    /// way [`H5File::attrs`] does.
    #[test]
    fn an_undecodable_attribute_does_not_hide_the_dimension_list_after_it() {
        // A v1 object header with two attribute messages: one at an
        // attribute version this reader refuses, then a well-formed
        // DIMENSION_LIST holding one empty sequence.
        let mut bytes = vec![0u8; 112];
        bytes[0] = 1; // object header version
        bytes[2..4].copy_from_slice(&2u16.to_le_bytes()); // message count
        bytes[8..12].copy_from_slice(&96u32.to_le_bytes()); // block size

        bytes[16..18].copy_from_slice(&0x000Cu16.to_le_bytes()); // attribute
        bytes[18..20].copy_from_slice(&8u16.to_le_bytes());
        bytes[24] = 9; // attribute version 9: unsplittable

        bytes[32..34].copy_from_slice(&0x000Cu16.to_le_bytes()); // attribute
        bytes[34..36].copy_from_slice(&72u16.to_le_bytes());
        bytes[40] = 1; // attribute version
        bytes[42..44].copy_from_slice(&15u16.to_le_bytes()); // name size
        bytes[44..46].copy_from_slice(&16u16.to_le_bytes()); // datatype size
        bytes[46..48].copy_from_slice(&16u16.to_le_bytes()); // dataspace size
        bytes[48..63].copy_from_slice(b"DIMENSION_LIST\0");
        bytes[64] = 9; // datatype class 9 = variable-length
        bytes[65] = 0; // ... of sequences, not strings
        bytes[68..72].copy_from_slice(&16u32.to_le_bytes()); // descriptor size
        bytes[72] = 7; // base class 7 = object reference
        bytes[76..80].copy_from_slice(&8u32.to_le_bytes()); // 8-byte addresses
        bytes[80] = 1; // dataspace version
        bytes[81] = 1; // rank
        bytes[88..96].copy_from_slice(&1u64.to_le_bytes()); // one element
        // The descriptor at [96..112] stays zero: a zero-length sequence,
        // which needs no global heap behind it.

        let mut file = parser(&bytes);
        file.objects.insert("/var".to_owned(), 0);
        assert_eq!(
            file.attr_object_references("/var", "DIMENSION_LIST"),
            Some(vec![Vec::new()]),
            "the attribute before it must be skipped, not end the search"
        );
    }

    /// A dataset whose elements live in OTHER files is refused, not read as
    /// one the writer never allocated.
    ///
    /// External storage pairs an external data files message (0x0007) with a
    /// contiguous layout whose address is UNDEFINED — the same marker a
    /// never-written dataset carries. Without the 0x0007 check the two are
    /// indistinguishable, and a dataset whose data exists, in a file beside
    /// this one, would come back as a whole plane of fill value.
    #[test]
    fn external_data_storage_is_refused_not_read_as_fill() {
        // A v1 object header with four messages: a rank-1 dataspace of two
        // elements, an f4 datatype, external data files, and a contiguous
        // layout at the undefined address.
        let mut bytes = vec![0u8; 104];
        bytes[0] = 1; // object header version
        bytes[2..4].copy_from_slice(&4u16.to_le_bytes()); // message count
        bytes[8..12].copy_from_slice(&88u32.to_le_bytes()); // block size

        bytes[16..18].copy_from_slice(&0x0001u16.to_le_bytes()); // dataspace
        bytes[18..20].copy_from_slice(&16u16.to_le_bytes());
        bytes[24] = 1; // dataspace version
        bytes[25] = 1; // rank
        bytes[32..40].copy_from_slice(&2u64.to_le_bytes()); // dim 0

        bytes[40..42].copy_from_slice(&0x0003u16.to_le_bytes()); // datatype
        bytes[42..44].copy_from_slice(&8u16.to_le_bytes());
        bytes[48] = 1; // class 1 = IEEE float
        bytes[52..56].copy_from_slice(&4u32.to_le_bytes()); // 4 bytes wide

        bytes[56..58].copy_from_slice(&0x0007u16.to_le_bytes()); // external
        bytes[58..60].copy_from_slice(&8u16.to_le_bytes());

        bytes[72..74].copy_from_slice(&0x0008u16.to_le_bytes()); // data layout
        bytes[74..76].copy_from_slice(&24u16.to_le_bytes());
        bytes[80] = 3; // layout version
        bytes[81] = 1; // contiguous
        bytes[82..90].copy_from_slice(&UNDEFINED_ADDR.to_le_bytes());
        bytes[90..98].copy_from_slice(&8u64.to_le_bytes()); // stored size

        let mut file = parser(&bytes);
        file.objects.insert("/external".to_owned(), 0);
        let Err(err) = file.dataset("/external") else {
            panic!("external storage must not decode");
        };
        assert!(err.to_string().contains("external data files"), "{err}");
    }

    /// A message that says "my body is somewhere else" is refused, not read
    /// where it lies.
    ///
    /// The shared flag means the body is a pointer into the file's
    /// shared-message table. Reading it inline would hand
    /// [`H5File::parse_datatype`] a heap address and get a plausible-looking
    /// datatype out of it — the class and size fields would parse, they
    /// would just be somebody else's bytes.
    #[test]
    fn a_shared_object_header_message_is_refused_not_read_inline() {
        // A v1 object header carrying one message: a DATATYPE (0x0003) with
        // the shared flag set and a 16-byte body.
        let mut bytes = vec![0u8; 40];
        bytes[0] = 1; // object header version
        bytes[2..4].copy_from_slice(&1u16.to_le_bytes()); // message count
        bytes[8..12].copy_from_slice(&24u32.to_le_bytes()); // block size
        bytes[16..18].copy_from_slice(&0x0003u16.to_le_bytes()); // datatype
        bytes[18..20].copy_from_slice(&16u16.to_le_bytes()); // body size
        bytes[20] = MESSAGE_FLAG_SHARED;

        let file = parser(&bytes);
        let Err(err) = file.parse_object_header(0) else {
            panic!("a shared message must fail");
        };
        assert!(err.to_string().contains("shared"), "{err}");
    }

    #[test]
    fn v1_object_header_rejects_continuation_cycle() {
        let mut bytes = vec![0u8; 40];
        bytes[0] = 1;
        bytes[2..4].copy_from_slice(&1u16.to_le_bytes());
        bytes[8..12].copy_from_slice(&24u32.to_le_bytes());
        bytes[16..18].copy_from_slice(&0x0010u16.to_le_bytes());
        bytes[18..20].copy_from_slice(&16u16.to_le_bytes());
        bytes[24..32].copy_from_slice(&16u64.to_le_bytes());
        bytes[32..40].copy_from_slice(&24u64.to_le_bytes());

        let file = parser(&bytes);
        let Err(err) = file.parse_object_header(0) else {
            panic!("continuation cycle must fail");
        };
        assert!(err.to_string().contains("cycle"));
    }

    #[test]
    fn btree_walks_reject_self_references() {
        let mut group = vec![0u8; 40];
        group[..4].copy_from_slice(b"TREE");
        group[5] = 1;
        group[6..8].copy_from_slice(&1u16.to_le_bytes());
        let file = parser(&group);
        let mut entries = Vec::new();
        let mut visited = BTreeSet::new();
        let err = file
            .collect_group_entries(0, &mut entries, &mut visited, 0)
            .expect_err("group B-tree cycle must fail");
        assert!(err.to_string().contains("cycle"));

        let mut chunks = vec![0u8; 48];
        chunks[..4].copy_from_slice(b"TREE");
        chunks[4] = 1;
        chunks[5] = 1;
        chunks[6..8].copy_from_slice(&1u16.to_le_bytes());
        let file = parser(&chunks);
        let mut refs = Vec::new();
        let mut visited = BTreeSet::new();
        let err = file
            .collect_chunks(0, 1, &mut refs, &mut visited, 0)
            .expect_err("chunk B-tree cycle must fail");
        assert!(err.to_string().contains("cycle"));
    }

    /// Build a chain of single-entry v1 B-tree INTERNAL nodes, each pointing
    /// at the next: the crafted shape that turns tree depth into recursion
    /// depth. Patching three bytes of a real chunked sweep and appending such
    /// a chain is all it takes to build one. `node_type` 0 = group (symbol
    /// table), 1 = chunk index; their keys differ, hence `key_size`.
    fn btree_chain(node_type: u8, links: usize, key_size: usize) -> Vec<u8> {
        const OFFSET_SIZE: usize = 8;
        let header = 8 + 2 * OFFSET_SIZE; // signature, type, level, count, siblings
        let stride = header + key_size + OFFSET_SIZE;
        let mut bytes = vec![0u8; stride * links];
        for index in 0..links {
            let at = index * stride;
            bytes[at..at + 4].copy_from_slice(b"TREE");
            bytes[at + 4] = node_type;
            bytes[at + 5] = 1; // internal node, so every link recurses
            bytes[at + 6..at + 8].copy_from_slice(&1u16.to_le_bytes());
            let child = ((index + 1) * stride) as u64;
            let child_at = at + header + key_size;
            bytes[child_at..child_at + OFFSET_SIZE].copy_from_slice(&child.to_le_bytes());
        }
        bytes
    }

    #[test]
    fn btree_walks_reject_crafted_depth() {
        let chunks = btree_chain(1, 200, 8 + 8);
        let file = parser(&chunks);
        let err = file
            .collect_chunks(0, 1, &mut Vec::new(), &mut BTreeSet::new(), 0)
            .expect_err("a 200-link chunk B-tree chain must fail");
        assert!(err.to_string().contains("too deep"), "{err}");

        let groups = btree_chain(0, 200, 8);
        let file = parser(&groups);
        let err = file
            .collect_group_entries(0, &mut Vec::new(), &mut BTreeSet::new(), 0)
            .expect_err("a 200-link group B-tree chain must fail");
        assert!(err.to_string().contains("too deep"), "{err}");
    }

    /// Why the depth cap exists at all. Unbounded recursion here overflows
    /// the stack, and a stack overflow is a hard process kill — not a panic,
    /// so `catch_unwind` cannot turn it into an error dialog. A GUI decodes a
    /// dropped file on a worker thread, which gets 2 MiB by default; this
    /// runs 20 000 links through a quarter of that and requires the walk to
    /// come back with an error instead of taking the process with it.
    #[test]
    fn a_deep_btree_chain_cannot_overflow_a_small_stack() {
        let chain = btree_chain(1, 20_000, 8 + 8);
        let outcome = std::thread::Builder::new()
            .stack_size(256 * 1024)
            .spawn(move || {
                let file = parser(&chain);
                file.collect_chunks(0, 1, &mut Vec::new(), &mut BTreeSet::new(), 0)
                    .map_err(|err| err.to_string())
            })
            .expect("spawn decoding thread")
            .join()
            .expect("the decoding thread must survive the file");
        assert!(
            outcome.is_err_and(|message| message.contains("too deep")),
            "a crafted chain must error, not recurse"
        );
    }

    /// Per-dataset ceilings bound one plane; a volume is a loop over planes.
    #[test]
    fn the_decode_budget_bounds_the_sum_of_datasets() {
        const SYNTH: &[u8] = include_bytes!("../tests/data/odim_pvol_synth.h5");
        const PLANE_BYTES: usize = 36 * 25; // 8-bit plane of the fixture

        let file = H5File::open_within_budget(SYNTH, PLANE_BYTES).expect("open");
        file.dataset("/dataset1/data1/data")
            .expect("the first plane fits the budget");
        let err = file
            .dataset("/dataset1/data2/data")
            .expect_err("the SUM of planes must be bounded, not just each one");
        assert!(err.to_string().contains("decode budget"), "{err}");

        // Every plane the fixture carries, under the real ceiling: nothing
        // is rejected.
        let file = H5File::open(SYNTH).expect("open");
        let planes = file.child_names("/dataset1");
        assert!(planes.len() > 2, "the fixture must have several planes");
        for plane in planes.iter().filter(|name| name.starts_with("data")) {
            file.dataset(&format!("/dataset1/{plane}/data"))
                .unwrap_or_else(|err| panic!("{plane} must decode: {err}"));
        }
    }

    /// The heap-ID field widths this parser derives are the ones HDF5 itself
    /// uses for the two heaps netCDF-4 writes.
    ///
    /// These widths are never stored — every reader recomputes them from the
    /// heap header — so getting them wrong reads each heap ID at the wrong
    /// stride and fetches somebody else's bytes. HDF5 fixes the inputs in
    /// `H5Gdense.c` (links: max index 32 bits, 64 KiB direct blocks, 4 KiB
    /// managed objects) and `H5Apkg.h` (attributes: max index 40 bits, same
    /// block and object sizes); the widths they imply are what give link
    /// records their 7-byte heap IDs and attribute records their 8-byte
    /// ones, which is checkable against any real file.
    #[test]
    fn heap_id_field_widths_match_hdf5s_own_link_and_attribute_heaps() {
        let length_width = |max_direct: u64, max_managed: u64| {
            log2_floor(max_direct)
                .div_ceil(8)
                .min(limit_enc_size(max_managed))
        };
        // Links: 1 flags byte + 4 offset bytes + 2 length bytes = 7.
        assert_eq!(32usize.div_ceil(8), 4, "link heap offset field");
        assert_eq!(
            length_width(64 * 1024, 4 * 1024),
            2,
            "link heap length field"
        );
        // Attributes: 1 + 5 + 2 = 8.
        assert_eq!(40usize.div_ceil(8), 5, "attribute heap offset field");
        assert_eq!(
            length_width(64 * 1024, 4 * 1024),
            2,
            "attribute heap length field"
        );
        // `limit_enc_size` is HDF5's `H5VM_limit_enc_size`: one byte per
        // eight bits of the largest value, counting from one.
        assert_eq!(limit_enc_size(0), 1);
        assert_eq!(limit_enc_size(255), 1);
        assert_eq!(limit_enc_size(256), 2);
        assert_eq!(limit_enc_size(u64::MAX), 8);
        assert_eq!(log2_floor(0), 0);
        assert_eq!(log2_floor(1), 0);
        assert_eq!(log2_floor(65_536), 16);
    }

    /// A fractal heap's doubling table: two rows at the starting size, then
    /// double every row.
    ///
    /// Off by one row here shifts every block's place in the heap's linear
    /// address space, so heap IDs resolve into the wrong block entirely.
    #[test]
    fn fractal_heap_rows_double_after_the_second() {
        let heap = FractalHeap {
            heap_id_len: 7,
            id_offset_bytes: 4,
            id_length_bytes: 2,
            checksummed_blocks: true,
            table_width: 4,
            starting_block_size: 512,
            max_direct_rows: 9,
            blocks: Vec::new(),
        };
        let sizes: Vec<u64> = (0..6)
            .map(|row| heap.row_block_size(row).unwrap())
            .collect();
        assert_eq!(sizes, [512, 512, 1_024, 2_048, 4_096, 8_192]);
        // A row index that would shift past 64 bits is refused rather than
        // wrapping to a small, plausible-looking size.
        assert!(heap.row_block_size(64).is_err());
    }

    /// The version 2 B-tree child-pointer widths match HDF5's `node_info`
    /// table for a real link-name index.
    ///
    /// Like the heap ID widths, these are recomputed rather than stored, and
    /// they decide the stride of the child pointers in an internal node. A
    /// wrong stride walks into the middle of an address and descends to a
    /// garbage node — which, on a tree one level deep, is exactly the shape
    /// a netCDF-4 root group with more than about forty variables has.
    #[test]
    fn btree_v2_child_pointer_widths_match_hdf5s_node_info() {
        // A link-name B-tree: 512-byte nodes, 11-byte records (4-byte name
        // hash + 7-byte heap ID), one level of internal nodes.
        let shape = BTreeV2Shape::new(512, 11, 1, 8).expect("shape");
        // Leaf: (512 - 10 prefix) / 11.
        assert_eq!(shape.max_records_per_node, 45);
        assert_eq!(shape.record_size, 11);
        // 45 records needs one byte to count.
        assert_eq!(shape.max_nrec_size, 1);
        assert_eq!(shape.cumulative_nrec_size(0).unwrap(), 1);
        // Internal: (512 - (10 + 1)) / (11 + 8 address + 1) = 25 records,
        // so a subtree holds at most (25 + 1) * 45 + 25 = 1195 records,
        // which needs two bytes.
        assert_eq!(shape.cumulative_nrec_size(1).unwrap(), 2);
        // A depth the tree never declared is an error, not a silent zero.
        assert!(shape.cumulative_nrec_size(2).is_err());
        // A node too small to hold one record is refused.
        assert!(BTreeV2Shape::new(12, 11, 0, 8).is_err());
    }

    /// Every shape the fill value message comes in, against the layouts
    /// the spec gives them.
    ///
    /// The versions differ only in how they announce that a value follows,
    /// and reading the announcement wrong is the difference between the
    /// file's own no-data marker and a plane of zeros that looks like data.
    #[test]
    fn fill_value_messages_are_read_at_every_version() {
        let value = (-9999.0f32).to_le_bytes();
        let mut body = Vec::new();

        // Version 1: version, allocation time, write time, defined, then
        // size and value unconditionally.
        body.extend_from_slice(&[1, 2, 2, 1]);
        body.extend_from_slice(&4u32.to_le_bytes());
        body.extend_from_slice(&value);
        assert_eq!(
            parse_fill_value(0x0005, &body).unwrap(),
            Some(value.to_vec())
        );

        // Version 2 with the defined byte set, then cleared: the same
        // header, but the second one carries nothing after it.
        body[0] = 2;
        assert_eq!(
            parse_fill_value(0x0005, &body).unwrap(),
            Some(value.to_vec())
        );
        assert_eq!(parse_fill_value(0x0005, &[2, 2, 2, 0]).unwrap(), None);

        // Version 3: one flags byte, bit 5 = "fill value defined".
        let mut v3 = vec![3, 0b0010_0000];
        v3.extend_from_slice(&4u32.to_le_bytes());
        v3.extend_from_slice(&value);
        assert_eq!(parse_fill_value(0x0005, &v3).unwrap(), Some(value.to_vec()));
        // Bit 4 is "undefined", and neither bit set is "the default" —
        // both mean zeros here.
        assert_eq!(parse_fill_value(0x0005, &[3, 0b0001_0000]).unwrap(), None);
        assert_eq!(parse_fill_value(0x0005, &[3, 0]).unwrap(), None);

        // The deprecated message: a bare size and value.
        let mut old = 4u32.to_le_bytes().to_vec();
        old.extend_from_slice(&value);
        assert_eq!(
            parse_fill_value(0x0004, &old).unwrap(),
            Some(value.to_vec())
        );
        assert_eq!(parse_fill_value(0x0004, &0u32.to_le_bytes()).unwrap(), None);

        // A version this reader does not know refuses rather than guessing
        // which bytes are the value.
        assert!(parse_fill_value(0x0005, &[9, 0, 0, 1]).is_err());
        // A size field that overruns the message is truncation, not a fill
        // value of whatever happens to follow in memory.
        let mut short = vec![3, 0b0010_0000];
        short.extend_from_slice(&64u32.to_le_bytes());
        assert!(parse_fill_value(0x0005, &short).is_err());
    }

    #[test]
    fn a_filled_buffer_repeats_one_element_and_defaults_to_zero() {
        let fill = (-9999.0f32).to_le_bytes();
        let buffer = filled_buffer(12, Some(&fill));
        assert_eq!(buffer.len(), 12);
        for element in buffer.chunks_exact(4) {
            assert_eq!(f32::from_le_bytes(element.try_into().unwrap()), -9999.0);
        }
        // No fill value declared: the HDF5 default is zero, which is what
        // ODIM's `undetect` planes have always relied on.
        assert_eq!(filled_buffer(4, None), vec![0u8; 4]);
        assert_eq!(filled_buffer(4, Some(&[0, 0])), vec![0u8; 4]);
        assert!(filled_buffer(0, Some(&fill)).is_empty());
    }

    /// Storage HDF5 never allocated reads back as the dataset's declared
    /// fill value, in both of the two ways a file can leave it out.
    ///
    /// `/reflectivity` is contiguous with an UNDEFINED data address and
    /// `/velocity` is chunked with no chunk records at all; `/spectrum_width`
    /// is chunked with one of its two chunks written, so it pins that the
    /// seeded buffer is still overwritten where real data exists. Read with
    /// netCDF4-python 1.7.4: 0 of 48, 0 of 48 and 24 of 48 gates written.
    #[test]
    fn unallocated_storage_reads_back_as_the_declared_fill_value() {
        const FILL_FIXTURE: &[u8] =
            include_bytes!("../tests/data/cfrad.unwritten_storage.netcdf4.nc");

        let file = H5File::open(FILL_FIXTURE).expect("open the fill fixture");
        let values = |path: &str| match file.dataset(path).expect("dataset").data {
            H5Data::F32(values) => values,
            other => panic!("{path} should be f4, not {other:?}"),
        };

        for path in ["/reflectivity", "/velocity"] {
            let plane = values(path);
            assert_eq!(plane.len(), 48, "{path}");
            assert!(
                plane.iter().all(|value| *value == -9999.0),
                "{path} was never written, so every gate is the fill value"
            );
        }

        let width = values("/spectrum_width");
        assert_eq!(width.len(), 48);
        // Rays 0-3 are the allocated chunk: 0.5, 1.0, ... 12.0.
        for (index, value) in width[..24].iter().enumerate() {
            assert_eq!(*value, (index + 1) as f32 * 0.5, "written gate {index}");
        }
        assert!(
            width[24..].iter().all(|value| *value == -9999.0),
            "the chunk the writer never allocated must not come back as data"
        );
    }

    /// Jenkins lookup3 (hashlittle) known-answer vectors. The 30-byte
    /// phrase with init 0 is the published lookup3 self-test value; the
    /// shorter vectors pin every tail-length branch class (empty, <4,
    /// exactly 12 = one full block, 13 = block + 1-byte tail) and were
    /// cross-checked against real HDF5 v2 header checksums (AEMET espdg
    /// PVOL fixture) with an independent Python implementation.
    #[test]
    fn jenkins_lookup3_matches_reference_vectors() {
        assert_eq!(jenkins_lookup3(b""), 0xdead_beef);
        assert_eq!(
            jenkins_lookup3(b"Four score and seven years ago"),
            0x1777_0551
        );
        assert_eq!(jenkins_lookup3(b"abc"), 0x0e39_7631);
        assert_eq!(jenkins_lookup3(b"0123456789ab"), 0x1065_e50a);
        assert_eq!(jenkins_lookup3(b"0123456789abc"), 0x7351_ce56);
    }
}

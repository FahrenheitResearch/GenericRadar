//! The ASCII `key=value` blocks that frame a Vaisala RVP8/RVP900 time-series
//! record.
//!
//! A TS record is one `rvp8PulseInfo` block describing the acquisition,
//! followed by one `rvp8PulseHdr` block plus a run of packed I/Q bytes for
//! every transmitted pulse. Both blocks have the same shape:
//!
//! ```text
//! rvp8PulseInfo start
//! iVersion=4
//! fWavelengthCM=11.08
//! iRangeMask=21845 21845 ... 0
//! rvp8PulseInfo end
//! ```
//!
//! Values are free-form text: a single number, a run of whitespace-separated
//! numbers (the range mask, the two-channel noise floors), or a bare string
//! (the site and task names). Keys are dotted for nested structures
//! (`taskID.sTaskName`, `RX[0].fBurstMag`).
//!
//! Newer processors write the same blocks under the name `rvptsPulseInfo` /
//! `rvptsPulseHdr`. Both spellings are accepted, because the body is
//! identical and a reader that knew only one name would reject half the
//! archive.
//!
//! This module deliberately borrows out of the record rather than copying:
//! a long time-series file holds hundreds of thousands of pulse headers, and
//! allocating a map per pulse is the difference between a fast read and a
//! slow one.

use super::IqError;

/// The two spellings of the acquisition-description block.
pub const INFO_TAGS: [&str; 2] = ["rvp8PulseInfo", "rvptsPulseInfo"];
/// The two spellings of the per-pulse block.
pub const PULSE_TAGS: [&str; 2] = ["rvp8PulseHdr", "rvptsPulseHdr"];

/// One parsed `key=value` block, borrowed from the record bytes.
///
/// Field order is preserved, which keeps a diagnostic dump of an unfamiliar
/// record readable in the order its producer wrote it.
#[derive(Debug)]
pub struct Block<'a> {
    fields: Vec<(&'a str, &'a str)>,
    /// Offset of the first byte after the block's terminating newline.
    pub end: usize,
}

impl<'a> Block<'a> {
    /// Raw text of a field, or `None` when the producer did not write it.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<&'a str> {
        self.fields
            .iter()
            .find(|(name, _)| *name == key)
            .map(|(_, value)| *value)
    }

    /// Raw text of a required field.
    pub fn text(&self, key: &'static str) -> Result<&'a str, IqError> {
        self.get(key).ok_or(IqError::MissingField { key })
    }

    /// A required field parsed as an integer.
    pub fn int(&self, key: &'static str) -> Result<i64, IqError> {
        let text = self.text(key)?;
        text.trim().parse().map_err(|_| IqError::BadField {
            key,
            value: text.to_owned(),
        })
    }

    /// An optional field parsed as an integer, absent when not written.
    pub fn opt_int(&self, key: &'static str) -> Result<Option<i64>, IqError> {
        match self.get(key) {
            None => Ok(None),
            Some(text) => text
                .trim()
                .parse()
                .map(Some)
                .map_err(|_| IqError::BadField {
                    key,
                    value: text.to_owned(),
                }),
        }
    }

    /// A required field parsed as a float.
    pub fn float(&self, key: &'static str) -> Result<f32, IqError> {
        let text = self.text(key)?;
        text.trim().parse().map_err(|_| IqError::BadField {
            key,
            value: text.to_owned(),
        })
    }

    /// An optional field parsed as a float, with a fallback when absent.
    pub fn float_or(&self, key: &'static str, fallback: f32) -> Result<f32, IqError> {
        match self.get(key) {
            None => Ok(fallback),
            Some(text) => text.trim().parse().map_err(|_| IqError::BadField {
                key,
                value: text.to_owned(),
            }),
        }
    }

    /// A required field parsed as a whitespace-separated list of floats.
    pub fn float_list(&self, key: &'static str) -> Result<Vec<f32>, IqError> {
        let text = self.text(key)?;
        text.split_ascii_whitespace()
            .map(|token| {
                token.parse().map_err(|_| IqError::BadField {
                    key,
                    value: text.to_owned(),
                })
            })
            .collect()
    }

    /// A required field parsed as a whitespace-separated list of integers.
    pub fn int_list(&self, key: &'static str) -> Result<Vec<i64>, IqError> {
        let text = self.text(key)?;
        text.split_ascii_whitespace()
            .map(|token| {
                token.parse().map_err(|_| IqError::BadField {
                    key,
                    value: text.to_owned(),
                })
            })
            .collect()
    }
}

/// Whether `raw` opens with one of `tags`' start lines.
#[must_use]
pub fn starts_with_block(raw: &[u8], tags: &[&str]) -> bool {
    tags.iter()
        .any(|tag| raw.starts_with(format!("{tag} start").as_bytes()))
}

/// How far past a block's opening line its terminator may be.
///
/// The blocks are small — the largest field by far is `iRangeMask`, 512
/// numbers of at most five digits — and a real one runs under two kilobytes.
/// The bound matters because everything after a pulse header is BINARY
/// sample data: without it, a record whose framing has slipped would send the
/// terminator search across hundreds of megabytes of I/Q for every pulse.
const MAX_BLOCK_BYTES: usize = 64 * 1024;

/// Read the block that begins at `offset`.
///
/// The opening line must be exactly `<tag> start`; anything else is reported
/// rather than searched past, because a record whose framing has slipped is
/// one whose pulse boundaries can no longer be trusted either.
///
/// Only the block's own lines are required to be text. The bytes that follow
/// it are packed I/Q and are emphatically not UTF-8, so validity is checked a
/// line at a time rather than over the rest of the record.
pub fn read_block<'a>(
    raw: &'a [u8],
    offset: usize,
    tags: &[&str],
    what: &'static str,
) -> Result<Block<'a>, IqError> {
    let rest = raw
        .get(offset..)
        .filter(|rest| !rest.is_empty())
        .ok_or(IqError::Truncated {
            what,
            offset,
            needed: 1,
            available: 0,
        })?;

    let mut lines = LineReader {
        bytes: &rest[..rest.len().min(MAX_BLOCK_BYTES)],
        base: offset,
        cursor: 0,
    };
    // A first line with no newline after it cannot be a block opener, but it
    // is very likely to be the head of a file that simply is not a time
    // series. Judge it by its leading bytes so that case is told what it is
    // rather than being called truncated.
    let Some((first, _)) = lines.next_line(what)? else {
        return Err(not_a_block(what, offset, rest));
    };
    let tag = tags
        .iter()
        .find(|tag| first == format!("{tag} start"))
        .ok_or_else(|| IqError::MissingBlock {
            what,
            offset,
            found: first.chars().take(40).collect(),
        })?;
    let terminator = format!("{tag} end");

    let mut fields = Vec::new();
    loop {
        let Some((line, end)) = lines.next_line(what)? else {
            return Err(IqError::UnterminatedBlock { what, offset });
        };
        if line == terminator {
            return Ok(Block { fields, end });
        }
        if line.is_empty() {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            // A line that is neither `key=value` nor the terminator means the
            // framing assumption is wrong, and the pulse stride that follows
            // is computed from these fields. Stopping here beats decoding
            // gates from the middle of somebody else's record.
            return Err(IqError::MalformedLine {
                what,
                line: line.chars().take(60).collect(),
            });
        };
        fields.push((key.trim(), value.trim()));
    }
}

/// Report bytes that are not a block at all, quoting what was there.
///
/// The quoted head is lossy on purpose: these bytes may be any file a user
/// dropped on a radar viewer, and the point of the message is to show them
/// what the reader saw.
fn not_a_block(what: &'static str, offset: usize, rest: &[u8]) -> IqError {
    const QUOTED_BYTES: usize = 40;
    let head = &rest[..rest.len().min(QUOTED_BYTES)];
    IqError::MissingBlock {
        what,
        offset,
        found: String::from_utf8_lossy(head).into_owned(),
    }
}

/// Splits borrowed bytes into text lines while tracking record offsets.
struct LineReader<'a> {
    bytes: &'a [u8],
    base: usize,
    cursor: usize,
}

impl<'a> LineReader<'a> {
    /// The next line and the absolute offset just past its newline.
    ///
    /// `Ok(None)` means the window ran out before a newline-terminated line
    /// could be produced; the caller decides whether that is truncation or an
    /// unterminated block.
    fn next_line(&mut self, what: &'static str) -> Result<Option<(&'a str, usize)>, IqError> {
        if self.cursor >= self.bytes.len() {
            return Ok(None);
        }
        let remainder = &self.bytes[self.cursor..];
        let Some(index) = remainder.iter().position(|byte| *byte == b'\n') else {
            // No newline inside the window. A block line always ends in one,
            // so this is the end of what can be read here rather than a short
            // final line worth salvaging.
            self.cursor = self.bytes.len();
            return Ok(None);
        };
        let line_offset = self.base + self.cursor;
        self.cursor += index + 1;
        let raw_line = &remainder[..index];
        let trimmed = raw_line.strip_suffix(b"\r").unwrap_or(raw_line);
        let line = std::str::from_utf8(trimmed).map_err(|_| IqError::NotText {
            what,
            offset: line_offset,
        })?;
        Ok(Some((line, self.base + self.cursor)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "rvp8PulseInfo start\niVersion=4\nfWavelengthCM=11.08\n\
         fNoiseDBm=-80.5555 -80.5955\ntaskID.sTaskName=Ascope_DEFAULT\n\
         rvp8PulseInfo end\nTRAILING";

    #[test]
    fn reads_fields_and_reports_where_the_block_ended() {
        let block = read_block(SAMPLE.as_bytes(), 0, &INFO_TAGS, "pulse info").unwrap();
        assert_eq!(block.int("iVersion").unwrap(), 4);
        assert_eq!(block.float("fWavelengthCM").unwrap(), 11.08);
        assert_eq!(block.text("taskID.sTaskName").unwrap(), "Ascope_DEFAULT");
        assert_eq!(
            block.float_list("fNoiseDBm").unwrap(),
            vec![-80.5555, -80.5955]
        );
        assert_eq!(&SAMPLE[block.end..], "TRAILING");
    }

    #[test]
    fn accepts_the_rvpts_spelling() {
        let text = "rvptsPulseHdr start\niNumVecs=250\nrvptsPulseHdr end\n";
        let block = read_block(text.as_bytes(), 0, &PULSE_TAGS, "pulse header").unwrap();
        assert_eq!(block.int("iNumVecs").unwrap(), 250);
    }

    #[test]
    fn tolerates_carriage_returns() {
        let text = "rvp8PulseHdr start\r\niNumVecs=7\r\nrvp8PulseHdr end\r\n";
        let block = read_block(text.as_bytes(), 0, &PULSE_TAGS, "pulse header").unwrap();
        assert_eq!(block.int("iNumVecs").unwrap(), 7);
    }

    #[test]
    fn a_missing_field_names_the_key() {
        let block = read_block(SAMPLE.as_bytes(), 0, &INFO_TAGS, "pulse info").unwrap();
        let error = block.int("iNumVecs").unwrap_err();
        assert!(error.to_string().contains("iNumVecs"), "{error}");
    }

    #[test]
    fn a_non_numeric_field_reports_the_text_it_saw() {
        let text = "rvp8PulseHdr start\niNumVecs=lots\nrvp8PulseHdr end\n";
        let block = read_block(text.as_bytes(), 0, &PULSE_TAGS, "pulse header").unwrap();
        let error = block.int("iNumVecs").unwrap_err();
        assert!(error.to_string().contains("lots"), "{error}");
    }

    #[test]
    fn an_unterminated_block_is_an_error_not_a_partial_read() {
        let text = "rvp8PulseInfo start\niVersion=4\n";
        let error = read_block(text.as_bytes(), 0, &INFO_TAGS, "pulse info").unwrap_err();
        assert!(
            matches!(error, IqError::UnterminatedBlock { .. }),
            "{error}"
        );
    }

    #[test]
    fn a_wrong_opening_line_is_reported_with_what_was_there() {
        let error = read_block(b"AR2V0006.473", 0, &INFO_TAGS, "pulse info").unwrap_err();
        assert!(error.to_string().contains("AR2V0006"), "{error}");
    }

    #[test]
    fn a_block_followed_by_binary_samples_still_parses() {
        // Regression: the bytes after a pulse header are packed I/Q and are
        // not text. An earlier version validated the whole remaining buffer
        // as UTF-8 before reading a single line, which parsed every synthetic
        // fixture and failed on the first real record.
        let mut raw = b"rvp8PulseHdr start\niNumVecs=250\nrvp8PulseHdr end\n".to_vec();
        raw.extend_from_slice(&[0xFF, 0xFE, 0x80, 0x00, 0xC0, 0xAF]);
        let block = read_block(&raw, 0, &PULSE_TAGS, "pulse header").unwrap();
        assert_eq!(block.int("iNumVecs").unwrap(), 250);
        assert_eq!(&raw[block.end..], &[0xFF, 0xFE, 0x80, 0x00, 0xC0, 0xAF]);
    }

    #[test]
    fn a_non_text_byte_inside_a_block_is_reported_against_its_line() {
        let mut raw = b"rvp8PulseHdr start\niNumVecs=2".to_vec();
        raw.extend_from_slice(&[0xFF, b'\n']);
        raw.extend_from_slice(b"rvp8PulseHdr end\n");
        let error = read_block(&raw, 0, &PULSE_TAGS, "pulse header").unwrap_err();
        assert!(matches!(error, IqError::NotText { .. }), "{error}");
    }

    #[test]
    fn the_terminator_search_is_bounded_rather_than_scanning_the_whole_record() {
        // An unterminated header in front of a large record must not send the
        // search across every sample byte in the file.
        let mut raw = b"rvp8PulseHdr start\niNumVecs=250\n".to_vec();
        raw.resize(raw.len() + 4 * 1024 * 1024, b'x');
        let error = read_block(&raw, 0, &PULSE_TAGS, "pulse header").unwrap_err();
        assert!(
            matches!(error, IqError::UnterminatedBlock { .. }),
            "{error}"
        );
    }

    #[test]
    fn starts_with_block_recognises_both_spellings() {
        assert!(starts_with_block(b"rvp8PulseInfo start\n", &INFO_TAGS));
        assert!(starts_with_block(b"rvptsPulseInfo start\n", &INFO_TAGS));
        assert!(!starts_with_block(b"AR2V0006.473", &INFO_TAGS));
    }
}

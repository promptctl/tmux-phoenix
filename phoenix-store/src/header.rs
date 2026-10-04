//! The on-disk file format (ARCHITECTURE.md §7): the header is the
//! generation's identity — `format_version, header_len, origin,
//! captured_at, tag, body_len, checksum` — followed by the codec's body.
//! The header carries its own length so `tag` can vary and a listing still
//! reads headers without decoding a body. An unknown `format_version` is
//! refused loudly, never guessed; the one before this is read through its
//! own decoder.
//!
//! Layout, little-endian throughout:
//!
//! ```text
//! magic "PHNX" | format_version u32 | header_len u32 | <header_len bytes> | body
//!     where the header bytes are: origin (u8 0 = unrecorded, 1 = pid u32 + start_time i64)
//!                                 captured_at i64
//!                                 tag (u8 0 = none, 1 = str)
//!                                 body_len u64
//!                                 checksum u64
//! ```
//!
//! The format before it was fixed at 32 bytes: `magic | format_version u32
//! | captured_at i64 | body_len u64 | checksum u64`.

use phoenix_core::{OffsetDateTime, Origin, ServerId, Snapshot};

use crate::binary::{Reader, Writer};
use crate::blob_store::BlobStore;
use crate::checksum::fnv1a_64;
use crate::codec;
use crate::codec_v1;
use crate::error::StoreError;
use crate::tag::Tag;
use crate::version::FormatVersion;

const MAGIC: [u8; 4] = *b"PHNX";
/// Magic, format version and header length: what every version shares
/// before its own header begins.
const PREAMBLE_LEN: usize = 4 + 4;
const V1_HEADER_LEN: usize = 4 + 4 + 8 + 8 + 8;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Header {
    pub format_version: FormatVersion,
    pub origin: Origin,
    pub captured_at: OffsetDateTime,
    pub tag: Option<Tag>,
    pub body_len: u64,
    pub checksum: u64,
    /// Where the body starts in the file.
    pub body_start: usize,
}

pub fn encode_file(
    snapshot: &Snapshot,
    tag: Option<&Tag>,
    blobs: &BlobStore,
) -> Result<Vec<u8>, StoreError> {
    let body = codec::encode_body(snapshot, blobs)?;

    let mut header = Writer::new();
    match snapshot.origin {
        Origin::BeforeOriginWasRecorded => header.write_u8(0),
        Origin::Recorded(ServerId { pid, start_time }) => {
            header.write_u8(1);
            header.write_u32(pid);
            header.write_i64(start_time);
        }
    }
    header.write_i64(snapshot.captured_at.unix_timestamp());
    match tag {
        None => header.write_u8(0),
        Some(tag) => {
            header.write_u8(1);
            header.write_str(tag.as_str());
        }
    }
    header.write_u64(body.len() as u64);
    header.write_u64(fnv1a_64(&body));
    let header = header.into_bytes();

    let mut out = Vec::with_capacity(PREAMBLE_LEN + 4 + header.len() + body.len());
    out.extend_from_slice(&MAGIC);
    out.extend_from_slice(&FormatVersion::CURRENT.0.to_le_bytes());
    out.extend_from_slice(&(header.len() as u32).to_le_bytes());
    out.extend_from_slice(&header);
    out.extend_from_slice(&body);
    Ok(out)
}

fn le_u32(bytes: &[u8]) -> u32 {
    u32::from_le_bytes(bytes.try_into().expect("four bytes"))
}

fn le_u64(bytes: &[u8]) -> u64 {
    u64::from_le_bytes(bytes.try_into().expect("eight bytes"))
}

/// Header-only decode (for a fast `list`) — does not touch or validate the
/// body's checksum.
pub fn decode_header(bytes: &[u8]) -> Result<Header, StoreError> {
    if bytes.len() < PREAMBLE_LEN {
        return Err(StoreError::Truncated);
    }
    if bytes[0..4] != MAGIC {
        return Err(StoreError::BadMagic);
    }
    let format_version = FormatVersion(le_u32(&bytes[4..8]));
    match format_version {
        FormatVersion::BEFORE_ORIGIN => decode_v1_header(bytes),
        FormatVersion::CURRENT => decode_current_header(bytes),
        FormatVersion(found) => Err(StoreError::UnsupportedFormatVersion { found }),
    }
}

fn decode_v1_header(bytes: &[u8]) -> Result<Header, StoreError> {
    if bytes.len() < V1_HEADER_LEN {
        return Err(StoreError::Truncated);
    }
    Ok(Header {
        format_version: FormatVersion::BEFORE_ORIGIN,
        origin: Origin::BeforeOriginWasRecorded,
        captured_at: OffsetDateTime::from_unix_timestamp(le_u64(&bytes[8..16]) as i64),
        tag: None,
        body_len: le_u64(&bytes[16..24]),
        checksum: le_u64(&bytes[24..32]),
        body_start: V1_HEADER_LEN,
    })
}

fn decode_current_header(bytes: &[u8]) -> Result<Header, StoreError> {
    let header_len = *bytes
        .get(PREAMBLE_LEN..PREAMBLE_LEN + 4)
        .map(le_u32)
        .as_ref()
        .ok_or(StoreError::Truncated)? as usize;
    let body_start = PREAMBLE_LEN + 4 + header_len;
    let header = bytes
        .get(PREAMBLE_LEN + 4..body_start)
        .ok_or(StoreError::Truncated)?;
    let mut r = Reader::new(header);
    let origin = match r.read_u8()? {
        0 => Origin::BeforeOriginWasRecorded,
        1 => Origin::Recorded(ServerId {
            pid: r.read_u32()?,
            start_time: r.read_i64()?,
        }),
        value => {
            return Err(StoreError::InvalidTag {
                what: "Origin",
                value,
            })
        }
    };
    let captured_at = OffsetDateTime::from_unix_timestamp(r.read_i64()?);
    let tag = match r.read_u8()? {
        0 => None,
        1 => Some(Tag::parse(r.read_str()?).ok_or(StoreError::InvalidName)?),
        value => {
            return Err(StoreError::InvalidTag {
                what: "option",
                value,
            })
        }
    };
    let body_len = r.read_u64()?;
    let checksum = r.read_u64()?;
    if !r.remaining().is_empty() {
        return Err(StoreError::Truncated);
    }
    Ok(Header {
        format_version: FormatVersion::CURRENT,
        origin,
        captured_at,
        tag,
        body_len,
        checksum,
        body_start,
    })
}

/// Full decode: validates magic, format version, length, and checksum
/// before handing the body to the decoder for its format.
pub fn decode_file(bytes: &[u8], blobs: &BlobStore) -> Result<Snapshot, StoreError> {
    let header = decode_header(bytes)?;
    let body = &bytes[header.body_start..];
    if body.len() as u64 != header.body_len {
        return Err(StoreError::Truncated);
    }
    if fnv1a_64(body) != header.checksum {
        return Err(StoreError::ChecksumMismatch);
    }
    match header.format_version {
        FormatVersion::BEFORE_ORIGIN => codec_v1::decode_body(body, header.captured_at, blobs),
        _ => codec::decode_body(body, header.origin, header.captured_at, blobs),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{sample_snapshot, TestDir};

    #[test]
    fn round_trips_through_the_full_file_format_with_and_without_a_tag() {
        let dir = TestDir::new("header-round-trip");
        let snapshot = sample_snapshot(1_700_000_000);
        for tag in [None, Tag::parse("before-upgrade")] {
            let bytes = encode_file(&snapshot, tag.as_ref(), &dir.blobs()).unwrap();
            assert_eq!(decode_file(&bytes, &dir.blobs()).unwrap(), snapshot);
            let header = decode_header(&bytes).unwrap();
            assert_eq!(header.tag, tag);
            assert_eq!(header.origin, snapshot.origin);
            assert_eq!(header.captured_at, snapshot.captured_at);
            assert_eq!(header.format_version, FormatVersion::CURRENT);
        }
    }

    #[test]
    fn rejects_bad_magic() {
        let dir = TestDir::new("header-bad-magic");
        let mut bytes = encode_file(&sample_snapshot(1), None, &dir.blobs()).unwrap();
        bytes[0] = b'X';
        assert!(matches!(
            decode_file(&bytes, &dir.blobs()),
            Err(StoreError::BadMagic)
        ));
    }

    #[test]
    fn rejects_unsupported_format_version_without_touching_the_body() {
        let dir = TestDir::new("header-unsupported-version");
        let mut bytes = encode_file(&sample_snapshot(1), None, &dir.blobs()).unwrap();
        bytes[4..8].copy_from_slice(&999u32.to_le_bytes());
        assert!(matches!(
            decode_header(&bytes).unwrap_err(),
            StoreError::UnsupportedFormatVersion { found: 999 }
        ));
    }

    #[test]
    fn rejects_corrupted_body_via_checksum_mismatch_and_a_truncated_file() {
        let dir = TestDir::new("header-corrupt");
        let bytes = encode_file(&sample_snapshot(1), None, &dir.blobs()).unwrap();

        let mut flipped = bytes.clone();
        let last = flipped.len() - 1;
        flipped[last] ^= 0xff;
        assert!(matches!(
            decode_file(&flipped, &dir.blobs()),
            Err(StoreError::ChecksumMismatch)
        ));

        assert!(matches!(
            decode_file(&bytes[..bytes.len() - 5], &dir.blobs()),
            Err(StoreError::Truncated)
        ));
        assert!(matches!(
            decode_header(&bytes[..10]),
            Err(StoreError::Truncated)
        ));
    }
}

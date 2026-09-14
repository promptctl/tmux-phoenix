//! Header checksum (DESIGN.md §7: "versioned + checksummed header"). Not a
//! cryptographic hash — this only needs to catch truncation/corruption on a
//! local disk, not defend against tampering. FNV-1a is std-only, well
//! specified, and trivial to get right, unlike hand-rolling CRC32.

const FNV_OFFSET_BASIS: u64 = 0xcbf29ce484222325;
const FNV_PRIME: u64 = 0x100000001b3;

pub fn fnv1a_64(bytes: &[u8]) -> u64 {
    let mut hash = FNV_OFFSET_BASIS;
    for &byte in bytes {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

/// A wider (128-bit) content-addressing key for the blob store
/// (tmux-content-dos.2), distinct in purpose from [`fnv1a_64`]'s header
/// checksum: a checksum only needs to catch corruption, but two different
/// blobs colliding here would silently return the wrong pane content for
/// one of them, so this needs real collision resistance. No crypto-hash
/// crate is available in this environment (see DESIGN.md's implementation
/// notes), so this combines two independent FNV-1a-64 passes — hashing the
/// bytes plain for the low half, and hashing a fixed salt prepended to the
/// bytes for the high half, decorrelating the two — into 128 bits. For a
/// single-user, non-adversarial content store (not a security boundary),
/// that's adequate collision resistance without hand-rolling SHA-256.
const HIGH_HALF_SALT: &[u8] = b"tmux-phoenix-content-hash-v1";

pub fn content_hash(bytes: &[u8]) -> [u8; 16] {
    let low = fnv1a_64(bytes);
    let mut salted = Vec::with_capacity(HIGH_HALF_SALT.len() + bytes.len());
    salted.extend_from_slice(HIGH_HALF_SALT);
    salted.extend_from_slice(bytes);
    let high = fnv1a_64(&salted);

    let mut out = [0u8; 16];
    out[..8].copy_from_slice(&low.to_le_bytes());
    out[8..].copy_from_slice(&high.to_le_bytes());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_input_is_the_offset_basis() {
        assert_eq!(fnv1a_64(b""), FNV_OFFSET_BASIS);
    }

    #[test]
    fn is_deterministic() {
        assert_eq!(fnv1a_64(b"hello world"), fnv1a_64(b"hello world"));
    }

    #[test]
    fn different_input_yields_different_hash() {
        assert_ne!(fnv1a_64(b"hello"), fnv1a_64(b"world"));
    }

    #[test]
    fn single_bit_flip_changes_the_hash() {
        assert_ne!(fnv1a_64(b"snapshot"), fnv1a_64(b"snapshoT"));
    }

    #[test]
    fn content_hash_is_deterministic() {
        assert_eq!(
            content_hash(b"scrollback text"),
            content_hash(b"scrollback text")
        );
    }

    #[test]
    fn content_hash_differs_for_different_content() {
        assert_ne!(content_hash(b"pane one"), content_hash(b"pane two"));
    }

    #[test]
    fn content_hash_high_and_low_halves_are_not_trivially_equal() {
        // A sanity check against an implementation bug where the salt
        // application accidentally has no effect.
        let hash = content_hash(b"some content");
        let low = &hash[..8];
        let high = &hash[8..];
        assert_ne!(low, high);
    }
}

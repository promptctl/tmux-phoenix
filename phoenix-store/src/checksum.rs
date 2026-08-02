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
}

//! Hand-rolled binary primitives the snapshot codec builds on. No external
//! serialization crate is available in this environment (see DESIGN.md §4's
//! implementation note); this stands in for what `rmp-serde`/`serde` would
//! otherwise give us. Little-endian throughout; strings and byte-runs are
//! length-prefixed with a `u32` (a session/window/pane tree is never
//! remotely close to 4GiB of text).

use crate::error::StoreError;

#[derive(Default)]
pub struct Writer {
    buf: Vec<u8>,
}

impl Writer {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn into_bytes(self) -> Vec<u8> {
        self.buf
    }

    pub fn write_u8(&mut self, v: u8) {
        self.buf.push(v);
    }

    pub fn write_u32(&mut self, v: u32) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    pub fn write_i64(&mut self, v: i64) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    pub fn write_u64(&mut self, v: u64) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    pub fn write_bytes(&mut self, v: &[u8]) {
        self.write_u32(v.len() as u32);
        self.buf.extend_from_slice(v);
    }

    pub fn write_str(&mut self, v: &str) {
        self.write_bytes(v.as_bytes());
    }

    pub fn write_vec<T>(&mut self, items: &[T], mut write_item: impl FnMut(&mut Self, &T)) {
        self.write_u32(items.len() as u32);
        for item in items {
            write_item(self, item);
        }
    }

    /// Like [`Self::write_vec`], but for a known-length iterator rather than
    /// a slice — `phoenix_core::NonEmpty<T>` exposes `iter()` + `len()`, not
    /// `as_slice()`.
    pub fn write_seq<T>(
        &mut self,
        len: usize,
        items: impl Iterator<Item = T>,
        mut write_item: impl FnMut(&mut Self, T),
    ) {
        self.write_u32(len as u32);
        for item in items {
            write_item(self, item);
        }
    }

    pub fn write_option<T>(&mut self, item: &Option<T>, mut write_some: impl FnMut(&mut Self, &T)) {
        match item {
            None => self.write_u8(0),
            Some(v) => {
                self.write_u8(1);
                write_some(self, v);
            }
        }
    }
}

pub struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    fn take(&mut self, len: usize) -> Result<&'a [u8], StoreError> {
        let end = self.pos.checked_add(len).ok_or(StoreError::Truncated)?;
        let slice = self.buf.get(self.pos..end).ok_or(StoreError::Truncated)?;
        self.pos = end;
        Ok(slice)
    }

    pub fn read_u8(&mut self) -> Result<u8, StoreError> {
        Ok(self.take(1)?[0])
    }

    pub fn read_u32(&mut self) -> Result<u32, StoreError> {
        let bytes: [u8; 4] = self.take(4)?.try_into().expect("take(4) returns 4 bytes");
        Ok(u32::from_le_bytes(bytes))
    }

    pub fn read_i64(&mut self) -> Result<i64, StoreError> {
        let bytes: [u8; 8] = self.take(8)?.try_into().expect("take(8) returns 8 bytes");
        Ok(i64::from_le_bytes(bytes))
    }

    pub fn read_u64(&mut self) -> Result<u64, StoreError> {
        let bytes: [u8; 8] = self.take(8)?.try_into().expect("take(8) returns 8 bytes");
        Ok(u64::from_le_bytes(bytes))
    }

    pub fn read_bytes(&mut self) -> Result<&'a [u8], StoreError> {
        let len = self.read_u32()? as usize;
        self.take(len)
    }

    pub fn read_str(&mut self) -> Result<String, StoreError> {
        let bytes = self.read_bytes()?;
        std::str::from_utf8(bytes)
            .map(str::to_string)
            .map_err(|_| StoreError::InvalidUtf8)
    }

    pub fn read_vec<T>(
        &mut self,
        mut read_item: impl FnMut(&mut Self) -> Result<T, StoreError>,
    ) -> Result<Vec<T>, StoreError> {
        let len = self.read_u32()? as usize;
        (0..len).map(|_| read_item(self)).collect()
    }

    pub fn read_option<T>(
        &mut self,
        read_some: impl FnOnce(&mut Self) -> Result<T, StoreError>,
    ) -> Result<Option<T>, StoreError> {
        match self.read_u8()? {
            0 => Ok(None),
            1 => Ok(Some(read_some(self)?)),
            value => Err(StoreError::InvalidOptionTag { value }),
        }
    }

    /// Everything not yet consumed — used to confirm a decode consumed
    /// exactly the body, no more and no less.
    pub fn remaining(&self) -> &'a [u8] {
        &self.buf[self.pos..]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_every_primitive() {
        let mut w = Writer::new();
        w.write_u8(7);
        w.write_u32(0xdead_beef);
        w.write_i64(-12345);
        w.write_str("hello");
        w.write_option(&Some(9u8), |w, v| w.write_u8(*v));
        w.write_option(&None::<u8>, |w, v| w.write_u8(*v));
        w.write_vec(&[1u32, 2, 3], |w, v| w.write_u32(*v));

        let bytes = w.into_bytes();
        let mut r = Reader::new(&bytes);
        assert_eq!(r.read_u8().unwrap(), 7);
        assert_eq!(r.read_u32().unwrap(), 0xdead_beef);
        assert_eq!(r.read_i64().unwrap(), -12345);
        assert_eq!(r.read_str().unwrap(), "hello");
        assert_eq!(r.read_option(|r| r.read_u8()).unwrap(), Some(9));
        assert_eq!(r.read_option(|r| r.read_u8()).unwrap(), None);
        assert_eq!(r.read_vec(|r| r.read_u32()).unwrap(), vec![1, 2, 3]);
        assert!(r.remaining().is_empty());
    }

    #[test]
    fn reading_past_the_end_is_truncated_not_a_panic() {
        let mut r = Reader::new(&[1, 2]);
        assert!(matches!(r.read_u32(), Err(StoreError::Truncated)));
    }

    #[test]
    fn invalid_utf8_is_rejected() {
        let mut w = Writer::new();
        w.write_bytes(&[0xff, 0xfe]);
        let bytes = w.into_bytes();
        let mut r = Reader::new(&bytes);
        assert!(matches!(r.read_str(), Err(StoreError::InvalidUtf8)));
    }
}

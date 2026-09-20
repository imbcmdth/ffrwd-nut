//! NUT's byte primitives: the variable-length integers every field is coded
//! in, the CRC32 that closes every packet, and the cursor the demuxer reads
//! them with.
//!
//! The cursor works over a slice and answers [`Need`] rather than reading
//! past the end, which is what lets one parser serve both front ends: over a
//! packet body that is already whole, a `Need` is a malformed packet; over
//! the bytes a socket has handed over so far, it means wait.

use crate::error::{bail, Error};

/// NUT's CRC polynomial.
const CRC_POLY: u32 = 0x04C1_1DB7;

const fn crc_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = (i as u32) << 24;
        let mut bit = 0;
        while bit < 8 {
            c = if c & 0x8000_0000 != 0 {
                (c << 1) ^ CRC_POLY
            } else {
                c << 1
            };
            bit += 1;
        }
        table[i] = c;
        i += 1;
    }
    table
}

static CRC_TABLE: [u32; 256] = crc_table();

/// NUT's CRC32: polynomial 0x04C11DB7 taken most significant bit first,
/// starting at zero, with no final inversion.
pub fn crc32(data: &[u8]) -> u32 {
    let mut c: u32 = 0;
    for &b in data {
        c = (c << 8) ^ CRC_TABLE[usize::from(((c >> 24) as u8) ^ b)];
    }
    c
}

/// A variable-length integer never needs more than 10 bytes for 64 bits.
const MAX_V_BYTES: usize = 10;

/// Largest byte string a `vb` field may carry, so a damaged length cannot ask
/// for an unbounded allocation.
const MAX_VB_LEN: u64 = 1 << 20;

/// What was missing when a parse ran out of bytes. Over a whole packet body
/// it is the message the packet is refused with; over a stream it is what to
/// say if the stream ends there instead of going on.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Need {
    /// A field ran out partway through, at this position in the stream.
    Field { at: u64 },
    /// A run of bytes came up short.
    Bytes {
        what: &'static str,
        want: usize,
        got: usize,
    },
}

impl Need {
    /// The error this becomes where there are no more bytes coming.
    pub(crate) fn ended(self) -> Error {
        match self {
            Need::Field { at } => Error::format(format!("NUT stream ends mid-field at byte {at}")),
            Need::Bytes { what, want, got } => Error::format(format!(
                "NUT stream ends inside {what}: got {got} of {want} bytes"
            )),
        }
    }
}

/// Why a parse stopped: the bytes are wrong, or there are not enough of them.
#[derive(Debug)]
pub(crate) enum Halt {
    Bad(Error),
    Need(Need),
}

impl From<Error> for Halt {
    fn from(error: Error) -> Halt {
        Halt::Bad(error)
    }
}

impl From<Need> for Halt {
    fn from(need: Need) -> Halt {
        Halt::Need(need)
    }
}

/// What a parse answers: the value, or why it stopped.
pub(crate) type Parse<T> = Result<T, Halt>;

/// A cursor over buffered bytes that stops at the end rather than reading
/// past it. `base` is where `bytes[0]` sits in the stream, so a message names
/// a position a caller can find.
pub(crate) struct Cursor<'a> {
    bytes: &'a [u8],
    at: usize,
    base: u64,
}

impl<'a> Cursor<'a> {
    pub(crate) fn new(bytes: &'a [u8], base: u64) -> Cursor<'a> {
        Cursor { bytes, at: 0, base }
    }

    /// How far this has read.
    pub(crate) fn at(&self) -> usize {
        self.at
    }

    /// The stream position this has read to.
    pub(crate) fn pos(&self) -> u64 {
        self.base + self.at as u64
    }

    /// What has been read since `from`, for a checksum over it.
    pub(crate) fn since(&self, from: usize) -> &'a [u8] {
        &self.bytes[from..self.at]
    }

    pub(crate) fn u8(&mut self) -> Parse<u8> {
        match self.bytes.get(self.at) {
            Some(byte) => {
                self.at += 1;
                Ok(*byte)
            }
            None => Err(Need::Field { at: self.pos() }.into()),
        }
    }

    pub(crate) fn u32(&mut self) -> Parse<u32> {
        let bytes = self.take(4, "a 32-bit field")?;
        Ok(u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    /// `count` bytes, named for the message if they are not all there.
    pub(crate) fn take(&mut self, count: usize, what: &'static str) -> Parse<&'a [u8]> {
        let end = self.at.checked_add(count).ok_or(Halt::Need(Need::Bytes {
            what,
            want: count,
            got: self.bytes.len() - self.at,
        }))?;
        match self.bytes.get(self.at..end) {
            Some(slice) => {
                self.at = end;
                Ok(slice)
            }
            None => Err(Need::Bytes {
                what,
                want: count,
                got: self.bytes.len() - self.at,
            }
            .into()),
        }
    }

    /// A `v` field: 7 bits per byte, most significant group first, high bit
    /// set on every byte but the last.
    pub(crate) fn v(&mut self) -> Parse<u64> {
        let mut value: u64 = 0;
        for _ in 0..MAX_V_BYTES {
            let b = self.u8()?;
            if value > (u64::MAX >> 7) {
                bail!(
                    format: "NUT variable-length integer ending at byte {} overflows 64 bits",
                    self.pos()
                );
            }
            value = (value << 7) | u64::from(b & 0x7f);
            if b & 0x80 == 0 {
                return Ok(value);
            }
        }
        bail!(
            format: "NUT variable-length integer at byte {} runs past {MAX_V_BYTES} bytes",
            self.pos()
        )
    }

    /// An `s` field: a `v` biased so both signs are cheap to code.
    pub(crate) fn s(&mut self) -> Parse<i64> {
        let raw = self.v()?;
        if raw >= u64::MAX / 2 {
            bail!(format: "NUT signed integer at byte {} overflows 64 bits", self.pos());
        }
        let v = raw + 1;
        Ok(if v & 1 != 0 {
            -((v >> 1) as i64)
        } else {
            (v >> 1) as i64
        })
    }

    /// A `vb` field: a length, then that many bytes.
    pub(crate) fn vb(&mut self, what: &'static str) -> Parse<Vec<u8>> {
        let len = self.v()?;
        if len > MAX_VB_LEN {
            bail!(limit: "NUT {what} claims {len} bytes, more than {MAX_VB_LEN}");
        }
        Ok(self.take(len as usize, what)?.to_vec())
    }
}

/// Appends a `v` field.
pub fn put_v(out: &mut Vec<u8>, value: u64) {
    let mut groups = [0u8; MAX_V_BYTES];
    let mut count = 0;
    let mut left = value;
    loop {
        groups[count] = (left & 0x7f) as u8;
        count += 1;
        left >>= 7;
        if left == 0 {
            break;
        }
    }
    while count > 0 {
        count -= 1;
        out.push(groups[count] | if count > 0 { 0x80 } else { 0 });
    }
}

/// Appends an `s` field.
pub fn put_s(out: &mut Vec<u8>, value: i64) {
    let magnitude = value.unsigned_abs();
    put_v(out, magnitude.saturating_mul(2) - u64::from(value > 0));
}

/// Appends a `vb` field: the length, then the bytes.
pub fn put_vb(out: &mut Vec<u8>, bytes: &[u8]) {
    put_v(out, bytes.len() as u64);
    out.extend_from_slice(bytes);
}

pub fn put_u32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_be_bytes());
}

pub fn put_u64(out: &mut Vec<u8>, value: u64) {
    out.extend_from_slice(&value.to_be_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The value `read` reads out of `bytes`, where they are all there.
    fn read<T>(bytes: &[u8], read: impl FnOnce(&mut Cursor<'_>) -> Parse<T>) -> Result<T, String> {
        let mut cursor = Cursor::new(bytes, 0);
        match read(&mut cursor) {
            Ok(value) => Ok(value),
            Err(Halt::Bad(error)) => Err(error.to_string()),
            Err(Halt::Need(need)) => Err(need.ended().to_string()),
        }
    }

    fn round_trip_v(value: u64) {
        let mut buf = Vec::new();
        put_v(&mut buf, value);
        let mut cursor = Cursor::new(&buf, 0);
        assert_eq!(cursor.v().expect("v round trip"), value, "v {value}");
        // Nothing is left over: the field is exactly as wide as it needs.
        assert_eq!(cursor.at(), buf.len(), "v {value} length");
    }

    #[test]
    fn variable_length_integers_round_trip() {
        for value in [0, 1, 127, 128, 255, 256, 16383, 16384, 65536, u64::MAX] {
            round_trip_v(value);
        }
    }

    #[test]
    fn variable_length_integers_match_ffmpeg_bytes() {
        // Taken from a NUT file ffmpeg wrote: 32767 and 65536 as they appear
        // in the main header.
        let mut buf = Vec::new();
        put_v(&mut buf, 32767);
        assert_eq!(buf, vec![0x81, 0xff, 0x7f]);
        buf.clear();
        put_v(&mut buf, 65536);
        assert_eq!(buf, vec![0x84, 0x80, 0x00]);
    }

    #[test]
    fn signed_integers_round_trip() {
        for value in [0i64, 1, -1, 2, -2, 2048, -2048, i32::MAX as i64] {
            let mut buf = Vec::new();
            put_s(&mut buf, value);
            assert_eq!(read(&buf, |c| c.s()), Ok(value), "s {value} round trip");
        }
    }

    #[test]
    fn byte_strings_round_trip() {
        let mut buf = Vec::new();
        put_vb(&mut buf, b"RGBA");
        assert_eq!(buf, vec![4, b'R', b'G', b'B', b'A']);
        assert_eq!(read(&buf, |c| c.vb("fourcc")), Ok(b"RGBA".to_vec()));
    }

    #[test]
    fn checksum_matches_a_packet_ffmpeg_wrote() {
        // The syncpoint body ffmpeg wrote for a stream starting at pts 0:
        // global_key_pts 0, back_ptr_div16 0. Its checksum is zero, which is
        // what the CRC of two zero bytes comes to.
        assert_eq!(crc32(&[0, 0]), 0);
        // A one-byte message the polynomial does move.
        assert_eq!(crc32(&[0x01]), 0x04C1_1DB7);
    }

    #[test]
    fn a_truncated_field_names_the_stream_end() {
        let err = read(&[0x81u8], |c| c.v()).unwrap_err();
        assert!(err.contains("ends mid-field"), "{err}");
    }

    #[test]
    fn a_truncated_run_of_bytes_names_what_it_was() {
        let err = read(&[4u8, b'R', b'G'], |c| c.vb("a codec tag")).unwrap_err();
        assert_eq!(err, "NUT stream ends inside a codec tag: got 2 of 4 bytes");
    }

    #[test]
    fn a_variable_length_integer_that_never_ends_is_refused() {
        let err = read(&[0x81u8; 16], |c| c.v()).unwrap_err();
        assert!(err.contains("runs past"), "{err}");
    }
}

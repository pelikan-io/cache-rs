//! Block header parsing and serialization.

use crate::error::{DecodeError, NeedBytes};
use core::convert::TryFrom;

pub const HEADER_SIZE: usize = 12;
pub const FLAG_CHAIN_ROOT: u16 = 0b1;
const KNOWN_FLAGS: u16 = FLAG_CHAIN_ROOT;

/// Type of data stored in a ziplist block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Type {
    /// List type
    List = 0,
    /// Hash type
    Hash = 1,
    /// Set type
    Set = 2,
    /// Sorted set type
    Zset = 3,
}

impl TryFrom<u8> for Type {
    type Error = DecodeError;

    fn try_from(v: u8) -> Result<Self, DecodeError> {
        match v {
            0 => Ok(Type::List),
            1 => Ok(Type::Hash),
            2 => Ok(Type::Set),
            3 => Ok(Type::Zset),
            other => Err(DecodeError::UnknownType(other)),
        }
    }
}

/// Block header containing type, format, flags, entry count, and tail offset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlockHeader {
    /// The type of data in this block
    pub type_: Type,
    /// Format version for this type
    pub format: u8,
    /// Flags (e.g., chain root indicator)
    pub flags: u16,
    /// Number of entries in this block
    pub nentry: u32,
    /// Offset to the end of the block (tail)
    pub tail_off: u32,
}

impl BlockHeader {
    /// Parse a block header from a byte buffer.
    ///
    /// The buffer must be at least 12 bytes. Returns an error if the type,
    /// format, or flags are invalid.
    pub fn parse(buf: &[u8]) -> Result<Self, DecodeError> {
        let hdr: &[u8; 12] = buf
            .get(..HEADER_SIZE)
            .and_then(|s| <&[u8; 12]>::try_from(s).ok())
            .ok_or(DecodeError::Truncated)?;

        let type_ = Type::try_from(hdr[0])?;

        if hdr[1] != 0 {
            return Err(DecodeError::UnknownFormat(hdr[1]));
        }

        let flags = u16::from_le_bytes([hdr[2], hdr[3]]);
        if flags & !KNOWN_FLAGS != 0 {
            return Err(DecodeError::ReservedFlags(flags));
        }

        let nentry = u32::from_le_bytes([hdr[4], hdr[5], hdr[6], hdr[7]]);
        let tail_off = u32::from_le_bytes([hdr[8], hdr[9], hdr[10], hdr[11]]);

        Ok(BlockHeader {
            type_,
            format: hdr[1],
            flags,
            nentry,
            tail_off,
        })
    }

    /// Write this header to a mutable byte buffer.
    ///
    /// The buffer must be at least 12 bytes.
    pub fn write_to(&self, buf: &mut [u8]) {
        buf[0] = self.type_ as u8;
        buf[1] = self.format;
        buf[2..4].copy_from_slice(&self.flags.to_le_bytes());
        buf[4..8].copy_from_slice(&self.nentry.to_le_bytes());
        buf[8..12].copy_from_slice(&self.tail_off.to_le_bytes());
    }

    /// Initialize an empty block header in the provided buffer.
    ///
    /// Returns the number of bytes written (12) on success, or an error
    /// if the buffer is too small.
    pub fn init_empty(type_: Type, buf: &mut [u8]) -> Result<usize, NeedBytes> {
        if buf.len() < HEADER_SIZE {
            return Err(NeedBytes(HEADER_SIZE));
        }
        BlockHeader {
            type_,
            format: 0,
            flags: 0,
            nentry: 0,
            tail_off: HEADER_SIZE as u32,
        }
        .write_to(buf);
        Ok(HEADER_SIZE)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_block_roundtrip() {
        let mut buf = [0u8; 12];
        assert_eq!(BlockHeader::init_empty(Type::Hash, &mut buf), Ok(12));
        let h = BlockHeader::parse(&buf).unwrap();
        assert_eq!(h.type_, Type::Hash);
        assert_eq!(h.format, 0);
        assert_eq!(h.nentry, 0);
        assert_eq!(h.tail_off, 12);
    }

    #[test]
    fn parse_rejects_truncated() {
        assert_eq!(BlockHeader::parse(&[0u8; 11]), Err(DecodeError::Truncated));
    }

    #[test]
    fn parse_rejects_unknown_type_format_flags() {
        let mut buf = [0u8; 12];
        BlockHeader::init_empty(Type::List, &mut buf).unwrap();
        buf[0] = 9;
        assert_eq!(BlockHeader::parse(&buf), Err(DecodeError::UnknownType(9)));
        buf[0] = 0;
        buf[1] = 1;
        assert_eq!(BlockHeader::parse(&buf), Err(DecodeError::UnknownFormat(1)));
        buf[1] = 0;
        buf[2] = 0x02; // reserved flag bit
        assert!(matches!(
            BlockHeader::parse(&buf),
            Err(DecodeError::ReservedFlags(_))
        ));
    }
}

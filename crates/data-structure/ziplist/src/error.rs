//! Error types for ziplist codec operations.

/// Error type for block decoding operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum DecodeError {
    /// Buffer is too short to contain a complete header or value.
    Truncated,
    /// Unknown block type code encountered.
    UnknownType(u8),
    /// Unknown format code for this type.
    UnknownFormat(u8),
    /// Reserved flag bits are set in the header.
    ReservedFlags(u16),
    /// Block is corrupted or invalid.
    Corrupt,
}

/// Error indicating more bytes are needed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NeedBytes(pub usize);

/// Unit type indicating successful operation or fit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fit;

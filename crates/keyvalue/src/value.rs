//! Value types for cache storage engines.
//!
//! [`Value`] is a borrowed enum for use in APIs (get/insert).
//! [`OwnedValue`] is the heap-owning counterpart for storage.

/// A borrowed value — either a byte slice or a 64-bit unsigned integer.
#[derive(PartialEq, Eq)]
pub enum Value<'a> {
    Bytes(&'a [u8]),
    U64(u64),
}

/// An owned value — either a boxed byte slice or a 64-bit unsigned integer.
#[derive(PartialEq, Eq)]
pub enum OwnedValue {
    Bytes(Box<[u8]>),
    U64(u64),
}

impl Value<'_> {
    /// Returns the byte length of the value payload.
    #[allow(clippy::len_without_is_empty)]
    pub fn len(&self) -> usize {
        match self {
            Self::Bytes(v) => v.len(),
            Self::U64(_) => core::mem::size_of::<u64>(),
        }
    }

    /// Creates an owned copy of this value.
    pub fn to_owned(&self) -> OwnedValue {
        match self {
            Self::Bytes(v) => OwnedValue::Bytes(v.to_vec().into_boxed_slice()),
            Self::U64(v) => OwnedValue::U64(*v),
        }
    }
}

impl OwnedValue {
    /// Borrows this value as a [`Value`] reference.
    pub fn as_value(&self) -> Value<'_> {
        match self {
            Self::Bytes(v) => Value::Bytes(v),
            Self::U64(v) => Value::U64(*v),
        }
    }
}

// -- From conversions --

impl From<u64> for Value<'_> {
    fn from(v: u64) -> Self {
        Self::U64(v)
    }
}

impl<'a> From<&'a [u8]> for Value<'a> {
    fn from(v: &'a [u8]) -> Self {
        Self::Bytes(v)
    }
}

impl<'a> From<&'a str> for Value<'a> {
    fn from(v: &'a str) -> Self {
        Self::Bytes(v.as_bytes())
    }
}

impl<'a, const N: usize> From<&'a [u8; N]> for Value<'a> {
    fn from(v: &'a [u8; N]) -> Self {
        Self::Bytes(v)
    }
}

impl<'a> From<&'a Vec<u8>> for Value<'a> {
    fn from(v: &'a Vec<u8>) -> Self {
        Self::Bytes(v)
    }
}

// -- PartialEq with concrete types --

impl<const N: usize> PartialEq<&[u8; N]> for Value<'_> {
    fn eq(&self, rhs: &&[u8; N]) -> bool {
        matches!(self, Value::Bytes(v) if *v == *rhs)
    }
}

impl<const N: usize> PartialEq<[u8; N]> for Value<'_> {
    fn eq(&self, rhs: &[u8; N]) -> bool {
        matches!(self, Value::Bytes(v) if *v == rhs)
    }
}

impl PartialEq<[u8]> for Value<'_> {
    fn eq(&self, rhs: &[u8]) -> bool {
        matches!(self, Value::Bytes(v) if *v == rhs)
    }
}

impl PartialEq<u64> for Value<'_> {
    fn eq(&self, rhs: &u64) -> bool {
        matches!(self, Value::U64(v) if *v == *rhs)
    }
}

// -- Debug --

impl core::fmt::Debug for Value<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Bytes(v) => write!(f, "{v:?}"),
            Self::U64(v) => write!(f, "{v}"),
        }
    }
}

impl core::fmt::Debug for OwnedValue {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{:?}", self.as_value())
    }
}

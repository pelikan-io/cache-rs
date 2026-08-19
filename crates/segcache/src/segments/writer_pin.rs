//! RAII pin on a segment's writer count.

use crate::segments::SegmentHeader;

/// An RAII guard representing one writer pin on a segment.
///
/// While a `WriterPin` is alive, a reserve→define→publish is in flight on the
/// segment: any drain/evict that claims the segment must wait for
/// `active_writers` to reach zero before parsing its item stream, so it never
/// reads a reserved-but-undefined region and never recycles the segment out
/// from under a not-yet-published write (spec H1/H2).
///
/// Holds a raw pointer rather than a borrow so that the guard (and the
/// `ReservedItem` carrying it) is not lifetime-tied to the cache — the same
/// contract `SegmentGuard` and `RawItem` already have with the segment
/// allocation.
#[derive(Debug)]
pub(crate) struct WriterPin {
    header: *const SegmentHeader,
}

impl WriterPin {
    /// Create a guard for a successfully acquired writer pin.
    ///
    /// # Safety
    ///
    /// - `SegmentHeader::try_pin_writer` must have returned `true` on `header`,
    ///   and ownership of that pin transfers to this guard.
    /// - `header` must point into the `Segments` headers allocation, which
    ///   outlives the guard (a `ReservedItem` is consumed within the same
    ///   `insert`/`cas` call, long before `Segments` is dropped).
    pub(crate) unsafe fn new(header: *const SegmentHeader) -> Self {
        Self { header }
    }
}

impl Drop for WriterPin {
    fn drop(&mut self) {
        // SAFETY: per the constructor contract, the header outlives the guard
        // and this guard owns exactly one pin.
        unsafe { (*self.header).release_writer() };
    }
}

#[cfg(all(test, not(feature = "loom")))]
mod tests {
    use super::*;
    use crate::segments::state::{Metadata, State};
    use crate::segments::SegmentHeader;
    use core::num::NonZeroU32;

    #[test]
    fn writer_pin_guard_releases_on_drop() {
        let h = SegmentHeader::new(NonZeroU32::new(1).unwrap());
        h.store_metadata_for_test(Metadata {
            next: None,
            prev: None,
            state: State::Live,
            tag: 0,
        });

        assert!(h.try_pin_writer());
        assert_eq!(h.active_writers(), 1);
        {
            // SAFETY: try_pin_writer just returned true; `h` outlives the guard.
            let _pin = unsafe { WriterPin::new(&h as *const _) };
            assert_eq!(h.active_writers(), 1);
        }
        assert_eq!(h.active_writers(), 0, "guard drop released the pin");
    }
}

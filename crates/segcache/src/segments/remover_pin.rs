//! RAII pin on a segment's remover count.

use crate::segments::SegmentHeader;

/// An RAII guard representing one remover pin on a segment (a replace/delete
/// mid unlink+decrement of one of its items). While alive, a drain that claims
/// the segment must wait for `active_removers` to reach zero before it parses
/// or reclaims — so the remove's decrement never races the drain's accounting
/// and the segment is never recycled under a pending decrement (item 7f).
///
/// Holds a raw pointer rather than a borrow (same contract as `WriterPin`/
/// `SegmentGuard`) so it is not lifetime-tied to the cache.
#[derive(Debug)]
pub(crate) struct RemoverPin {
    header: *const SegmentHeader,
}

impl RemoverPin {
    /// # Safety
    /// - `SegmentHeader::try_pin_remover` returned `true` on `header`, and
    ///   ownership of that pin transfers to this guard.
    /// - `header` points into the `Segments` headers allocation, which outlives
    ///   the guard.
    pub(crate) unsafe fn new(header: *const SegmentHeader) -> Self {
        Self { header }
    }
}

impl Drop for RemoverPin {
    fn drop(&mut self) {
        // SAFETY: per the constructor contract the header outlives the guard and
        // this guard owns exactly one pin.
        unsafe { (*self.header).release_remover() };
    }
}

#[cfg(all(test, not(feature = "loom")))]
mod tests {
    use super::*;
    use crate::segments::state::{Metadata, State};
    use crate::segments::SegmentHeader;
    use core::num::NonZeroU32;

    #[test]
    fn remover_pin_guard_releases_on_drop() {
        let h = SegmentHeader::new(NonZeroU32::new(1).unwrap());
        h.store_metadata_for_test(Metadata {
            next: None,
            prev: None,
            state: State::Sealed,
            tag: 0,
        });

        assert!(h.try_pin_remover());
        assert_eq!(h.active_removers(), 1);
        {
            // SAFETY: try_pin_remover just returned true; `h` outlives the guard.
            let _pin = unsafe { RemoverPin::new(&h as *const _) };
            assert_eq!(h.active_removers(), 1);
        }
        assert_eq!(h.active_removers(), 0, "guard drop released the pin");
    }
}

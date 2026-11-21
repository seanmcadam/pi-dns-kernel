//! Re-arm a level-triggered NIC line after the handler masked it.
//!
//! The handler stores `masked` and only then masks the device, so the line
//! drops before the GIC end-of-interrupt. The poll stores `masked` clear and
//! only then unmasks. A packet that arrives in that window runs the handler
//! again and sets `masked`. Clearing the flag after the unmask used to forget
//! that, and the line stayed masked for the rest of the boot. The quiet poll
//! was then the only wake, which is why an idle ping sat in the tens of
//! milliseconds after a load run.

use core::sync::atomic::{AtomicBool, Ordering};

/// Unmask until a nested handler has not masked the line again.
///
/// Only the Pi 4 GENET driver calls this. Virt and Pi 5 keep the function
/// so the race is tested on the host. `unmask` must drop the device mask.
/// It may run the handler, which sets `masked`. Returns how many unmask
/// attempts ran, at most 8.
#[cfg_attr(not(feature = "board-pi4"), allow(dead_code))]
pub fn until_clear(masked: &AtomicBool, mut unmask: impl FnMut()) -> u32 {
    let mut passes = 0u32;
    loop {
        masked.store(false, Ordering::Release);
        unmask();
        passes = passes.saturating_add(1);
        if !masked.load(Ordering::Acquire) || passes >= 8 {
            return passes;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_nested_mask_is_unmasked_again() {
        let masked = AtomicBool::new(true);
        let nested = AtomicBool::new(true);
        let passes = until_clear(&masked, || {
            if nested.swap(false, Ordering::AcqRel) {
                masked.store(true, Ordering::Release);
            }
        });
        assert_eq!(passes, 2);
        assert!(!masked.load(Ordering::Relaxed));
    }

    #[test]
    fn an_idle_line_unmasks_once() {
        let masked = AtomicBool::new(false);
        let passes = until_clear(&masked, || {});
        assert_eq!(passes, 1);
        assert!(!masked.load(Ordering::Relaxed));
    }
}

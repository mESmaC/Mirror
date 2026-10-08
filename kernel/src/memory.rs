//! Bootstrap physical-frame allocation from Limine USABLE ranges only.
//! No freeing, heap, page-table ownership, or bootloader-memory reclamation yet.

use limine::memory_map::{Entry, EntryType};

pub const PAGE_SIZE: u64 = 4096;

pub struct FrameAllocator<'a> {
    entries: &'a [&'a Entry],
    region: usize,
    next: u64,
}

impl<'a> FrameAllocator<'a> {
    /// The boot path must keep one allocator for its non-overlapping memory map.
    /// Addresses returned here are physical numbers, not dereferenceable pointers.
    pub fn new(entries: &'a [&'a Entry]) -> Self {
        Self {
            entries,
            region: 0,
            next: 0,
        }
    }

    pub fn allocate(&mut self) -> Option<u64> {
        while let Some(entry) = self.entries.get(self.region) {
            if entry.entry_type == EntryType::USABLE
                && let Some(end) = entry.base.checked_add(entry.length)
                && let Some(aligned_base) = align_up(entry.base)
            {
                // Never allocate physical page zero, even if firmware marks it usable.
                let start = self.next.max(aligned_base).max(PAGE_SIZE);
                if let Some(next) = start.checked_add(PAGE_SIZE)
                    && next <= end
                {
                    self.next = next;
                    return Some(start);
                }
            }
            self.region += 1;
            self.next = 0;
        }
        None
    }
}

fn align_up(value: u64) -> Option<u64> {
    value
        .checked_add(PAGE_SIZE - 1)
        .map(|v| v & !(PAGE_SIZE - 1))
}

/// Check actual, exclusively allocated frames through Limine's direct mapping.
///
/// # Safety
/// Each page must be distinct, unused USABLE RAM, owned by the caller, and fully
/// mapped read/write at offset + physical address. The pages are overwritten.
pub unsafe fn check_frames(first: u64, second: u64, offset: u64) {
    assert_ne!(first, second, "allocator reused a live frame");
    assert_eq!(first % PAGE_SIZE, 0);
    assert_eq!(second % PAGE_SIZE, 0);
    let first = offset.checked_add(first).expect("direct-map overflow") as *mut u64;
    let second = offset.checked_add(second).expect("direct-map overflow") as *mut u64;
    for index in 0..(PAGE_SIZE as usize / size_of::<u64>()) {
        // SAFETY: The caller owns two full, aligned, writable, mapped pages.
        unsafe {
            first
                .add(index)
                .write_volatile(0x4d49_5252_4f52_0000 | index as u64);
            second
                .add(index)
                .write_volatile(0x5445_5354_0000_0000 | index as u64);
        }
    }
    for index in 0..(PAGE_SIZE as usize / size_of::<u64>()) {
        // SAFETY: These are the same exclusively owned mapped pages just written.
        unsafe {
            assert_eq!(
                first.add(index).read_volatile(),
                0x4d49_5252_4f52_0000 | index as u64
            );
            assert_eq!(
                second.add(index).read_volatile(),
                0x5445_5354_0000_0000 | index as u64
            );
        }
    }
}

#[cfg(feature = "boot-smoke")]
pub fn self_test() {
    let entries = [
        Entry {
            base: 0,
            length: 0x1000,
            entry_type: EntryType::USABLE,
        },
        Entry {
            base: 0x1000,
            length: 0x3000,
            entry_type: EntryType::RESERVED,
        },
        Entry {
            base: 0x4001,
            length: 0x2fff,
            entry_type: EntryType::USABLE,
        },
        Entry {
            base: 0x8000,
            length: 0x1000,
            entry_type: EntryType::BOOTLOADER_RECLAIMABLE,
        },
        Entry {
            base: 0xa000,
            length: 0x1000,
            entry_type: EntryType::USABLE,
        },
        Entry {
            base: u64::MAX - 4096,
            length: 8192,
            entry_type: EntryType::USABLE,
        },
    ];
    let refs = entries.each_ref();
    let mut allocator = FrameAllocator::new(&refs);
    assert_eq!(allocator.allocate(), Some(0x5000));
    assert_eq!(allocator.allocate(), Some(0x6000));
    assert_eq!(allocator.allocate(), Some(0xa000));
    assert_eq!(allocator.allocate(), None);
    assert_eq!(allocator.allocate(), None);
    assert_eq!(FrameAllocator::new(&[]).allocate(), None);
    assert_eq!(align_up(u64::MAX), None);
    crate::serial::println!("MIRROR_ALLOCATOR_TESTS_OK");
}

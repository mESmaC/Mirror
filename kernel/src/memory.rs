//! Physical-frame allocation and recycling from Limine USABLE ranges only.
//!
//! A bump allocator works before metadata is available. Once the direct map is
//! established, a two-bit state bitmap tracks every usable frame, including all
//! prior allocations. Its own pages remain reserved. Bootloader-reclaimable RAM
//! stays excluded because the bootstrap stack and responses are still live.

use limine::memory_map::{Entry, EntryType};

pub const PAGE_SIZE: u64 = 4096;

const NEVER_ALLOCATED: u8 = 0;
const ALLOCATED: u8 = 1;
const RECYCLED: u8 = 2;
const METADATA: u8 = 3;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FrameStats {
    pub total_frames: usize,
    pub allocated_frames: usize,
    pub recycled_frames: usize,
    pub metadata_frames: usize,
    pub never_allocated_frames: usize,
}

#[derive(Debug, PartialEq, Eq)]
pub enum RecyclingError {
    AlreadyEnabled,
    NoUsableFrames,
    MetadataTooLarge,
    NoMetadataSpace,
    DirectMapOverflow,
}

#[derive(Debug, PartialEq, Eq)]
pub enum FrameError {
    RecyclingDisabled,
    UnalignedAddress,
    NotUsable,
    NeverAllocated,
    AlreadyFree,
    MetadataReserved,
}

struct Recycling {
    bitmap: *mut u8,
    #[cfg(feature = "boot-smoke")]
    physical: u64,
    #[cfg(feature = "boot-smoke")]
    direct_map_offset: u64,
    stats: FrameStats,
    // A lower bound on the first recycled index; avoids rescanning the bitmap
    // from zero each time when allocations and frees are interleaved.
    recycled_hint: usize,
}

impl Recycling {
    fn state(&self, index: usize) -> u8 {
        assert!(index < self.stats.total_frames);
        // SAFETY: enable_recycling reserves enough bitmap bytes for every index
        // and its caller preserves the exclusive, writable direct mapping.
        let byte = unsafe { self.bitmap.add(index / 4).read() };
        (byte >> (2 * (index % 4))) & 3
    }

    fn set_state(&mut self, index: usize, state: u8) {
        assert!(index < self.stats.total_frames);
        let shift = 2 * (index % 4);
        // SAFETY: The bitmap is exclusively owned, initialized, and mapped for
        // this allocator's lifetime. The checked frame index bounds the byte.
        unsafe {
            let byte = self.bitmap.add(index / 4);
            byte.write((byte.read() & !(3 << shift)) | (state << shift));
        }
    }
}

pub struct FrameAllocator<'a> {
    entries: &'a [&'a Entry],
    region: usize,
    next: u64,
    recycling: Option<Recycling>,
}

impl<'a> FrameAllocator<'a> {
    /// The boot path must keep one allocator for its non-overlapping memory map.
    /// Addresses returned here are physical numbers, not dereferenceable pointers.
    pub fn new(entries: &'a [&'a Entry]) -> Self {
        Self {
            entries,
            region: 0,
            next: 0,
            recycling: None,
        }
    }

    pub fn allocate(&mut self) -> Option<u64> {
        if let Some(recycling) = &mut self.recycling
            && recycling.stats.recycled_frames != 0
        {
            let index = (recycling.recycled_hint..recycling.stats.total_frames)
                .find(|&index| recycling.state(index) == RECYCLED)
                .expect("recycled-frame accounting mismatch");
            recycling.set_state(index, ALLOCATED);
            recycling.recycled_hint = index + 1;
            recycling.stats.recycled_frames -= 1;
            recycling.stats.allocated_frames += 1;
            return Some(self.physical_at(index).expect("invalid recycled index"));
        }
        while let Some(entry) = self.entries.get(self.region) {
            if let Some((base, end)) = usable_bounds(entry) {
                let start = self.next.max(base);
                if let Some(next) = start.checked_add(PAGE_SIZE)
                    && next <= end
                {
                    self.next = next;
                    let index = self.index_of(start);
                    if let Some(recycling) = &mut self.recycling {
                        let index = index.expect("invalid bump frame");
                        if recycling.state(index) == METADATA {
                            continue;
                        }
                        assert_eq!(recycling.state(index), NEVER_ALLOCATED);
                        recycling.set_state(index, ALLOCATED);
                        recycling.stats.allocated_frames += 1;
                        recycling.stats.never_allocated_frames -= 1;
                    }
                    return Some(start);
                }
            }
            self.region += 1;
            self.next = 0;
        }
        None
    }

    /// Reserve state metadata from one remaining contiguous USABLE range.
    /// Each usable frame costs two bits; physical holes consume no bitmap space.
    /// A failure leaves the allocator unchanged. Prior allocations stay live.
    ///
    /// # Safety
    /// This must be the unique allocator for a non-overlapping memory map. Run
    /// on the sole boot CPU with interrupts disabled. Every USABLE page must be
    /// fully mapped writable at `offset + physical`, and that mapping must stay
    /// valid for this allocator's lifetime. No other code may use its unallocated
    /// memory or access the metadata pages. Keep the borrowed memory map live.
    pub unsafe fn enable_recycling(&mut self, offset: u64) -> Result<(), RecyclingError> {
        if self.recycling.is_some() {
            return Err(RecyclingError::AlreadyEnabled);
        }
        let mut total_frames = 0_usize;
        let mut allocated_frames = 0_usize;
        for (region, entry) in self.entries.iter().enumerate() {
            if let Some((base, end)) = usable_bounds(entry) {
                let count = usize::try_from((end - base) / PAGE_SIZE)
                    .map_err(|_| RecyclingError::MetadataTooLarge)?;
                total_frames = total_frames
                    .checked_add(count)
                    .ok_or(RecyclingError::MetadataTooLarge)?;
                let allocated = if region < self.region {
                    count
                } else if region == self.region {
                    ((self.next.clamp(base, end) - base) / PAGE_SIZE) as usize
                } else {
                    0
                };
                allocated_frames = allocated_frames
                    .checked_add(allocated)
                    .ok_or(RecyclingError::MetadataTooLarge)?;
            }
        }
        if total_frames == 0 {
            return Err(RecyclingError::NoUsableFrames);
        }
        let bitmap_bytes = total_frames.div_ceil(4);
        let metadata_bytes = align_up(bitmap_bytes as u64)
            .filter(|&bytes| bytes <= isize::MAX as u64)
            .ok_or(RecyclingError::MetadataTooLarge)?;
        let metadata_frames = (metadata_bytes / PAGE_SIZE) as usize;
        let physical = self
            .entries
            .iter()
            .enumerate()
            .skip(self.region)
            .find_map(|(region, entry)| {
                let (base, end) = usable_bounds(entry)?;
                let start = if region == self.region {
                    self.next.max(base)
                } else {
                    base
                };
                (start.checked_add(metadata_bytes)? <= end).then_some(start)
            })
            .ok_or(RecyclingError::NoMetadataSpace)?;
        let address = offset
            .checked_add(physical)
            .filter(|&address| address != 0)
            .ok_or(RecyclingError::DirectMapOverflow)?;
        address
            .checked_add(metadata_bytes - 1)
            .ok_or(RecyclingError::DirectMapOverflow)?;
        let bitmap = address as *mut u8;
        // SAFETY: The selected pages are a contiguous, previously unallocated
        // USABLE span covered by the caller's direct-map and exclusivity contract.
        // The checked byte count is nonzero and fits in isize.
        unsafe { bitmap.write_bytes(0, metadata_bytes as usize) };
        let metadata_index = self.index_of(physical).expect("metadata in usable RAM");
        let mut recycling = Recycling {
            bitmap,
            #[cfg(feature = "boot-smoke")]
            physical,
            #[cfg(feature = "boot-smoke")]
            direct_map_offset: offset,
            stats: FrameStats {
                total_frames,
                allocated_frames,
                recycled_frames: 0,
                metadata_frames,
                never_allocated_frames: total_frames - allocated_frames - metadata_frames,
            },
            recycled_hint: total_frames,
        };
        // Bump allocation visits the compact usable-frame index in order, even
        // when memory-map entries are not sorted by physical address.
        for index in 0..allocated_frames {
            recycling.set_state(index, ALLOCATED);
        }
        for index in metadata_index..metadata_index + metadata_frames {
            recycling.set_state(index, METADATA);
        }
        self.recycling = Some(recycling);
        Ok(())
    }

    /// Return one allocation to the pool, rejecting invalid state transitions.
    /// The contents are not cleared; callers must initialize frames before
    /// exposing them to a different protection domain.
    ///
    /// # Safety
    /// For an allocated frame, the caller must own it and have ended every live
    /// use, reference, DMA operation, and non-direct-map alias. Withdraw mappings
    /// and invalidate their TLB entries before freeing; a dormant direct-map
    /// alias may remain but must not be used until the frame is allocated again.
    /// Run on the sole boot CPU with interrupts disabled. Invalid frame values
    /// may be submitted to test validation; they never cause a frame write.
    /// Tracking cannot establish ownership or detect a stale free after reuse.
    pub unsafe fn deallocate(&mut self, frame: u64) -> Result<(), FrameError> {
        if self.recycling.is_none() {
            return Err(FrameError::RecyclingDisabled);
        }
        if !frame.is_multiple_of(PAGE_SIZE) {
            return Err(FrameError::UnalignedAddress);
        }
        let index = self.index_of(frame).ok_or(FrameError::NotUsable)?;
        let recycling = self.recycling.as_mut().expect("bitmap initialized");
        match recycling.state(index) {
            NEVER_ALLOCATED => Err(FrameError::NeverAllocated),
            RECYCLED => Err(FrameError::AlreadyFree),
            METADATA => Err(FrameError::MetadataReserved),
            ALLOCATED => {
                recycling.set_state(index, RECYCLED);
                recycling.recycled_hint = recycling.recycled_hint.min(index);
                recycling.stats.allocated_frames -= 1;
                recycling.stats.recycled_frames += 1;
                Ok(())
            }
            _ => unreachable!("two-bit frame state"),
        }
    }

    pub fn stats(&self) -> Option<FrameStats> {
        self.recycling.as_ref().map(|recycling| recycling.stats)
    }

    fn index_of(&self, frame: u64) -> Option<usize> {
        let mut index = 0;
        for entry in self.entries {
            if let Some((base, end)) = usable_bounds(entry) {
                if (base..end).contains(&frame) {
                    return Some(index + ((frame - base) / PAGE_SIZE) as usize);
                }
                index += ((end - base) / PAGE_SIZE) as usize;
            }
        }
        None
    }

    fn physical_at(&self, mut index: usize) -> Option<u64> {
        for entry in self.entries {
            if let Some((base, end)) = usable_bounds(entry) {
                let count = ((end - base) / PAGE_SIZE) as usize;
                if index < count {
                    return Some(base + index as u64 * PAGE_SIZE);
                }
                index -= count;
            }
        }
        None
    }
}

fn usable_bounds(entry: &Entry) -> Option<(u64, u64)> {
    if entry.entry_type != EntryType::USABLE {
        return None;
    }
    let end = entry.base.checked_add(entry.length)? & !(PAGE_SIZE - 1);
    // Physical page zero is never allocated, regardless of the firmware map.
    let base = align_up(entry.base)?.max(PAGE_SIZE);
    (base < end).then_some((base, end))
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
    assert_eq!(allocator.stats(), None);
    // SAFETY: Zero is invalid and recycling is disabled, so no frame is freed.
    unsafe {
        assert_eq!(allocator.deallocate(0), Err(FrameError::RecyclingDisabled));
    }
    assert_eq!(FrameAllocator::new(&[]).allocate(), None);
    assert_eq!(align_up(u64::MAX), None);
    crate::serial::println!("MIRROR_ALLOCATOR_TESTS_OK");
}

/// Exercise recycling on actual RAM with the initialized direct-map contract.
#[cfg(feature = "boot-smoke")]
pub fn self_test_recycling(frames: &mut FrameAllocator<'_>) {
    let before = frames.stats().expect("recycling must be enabled");
    let recycling = frames.recycling.as_ref().expect("frame bitmap");
    let metadata = recycling.physical;
    let offset = recycling.direct_map_offset;
    let virgin = (0..before.total_frames)
        .find(|&index| recycling.state(index) == NEVER_ALLOCATED)
        .and_then(|index| frames.physical_at(index))
        .expect("recycling test needs unallocated RAM");
    let reserved = frames
        .entries
        .iter()
        .filter(|entry| entry.entry_type != EntryType::USABLE)
        .find_map(|entry| {
            let start = align_up(entry.base)?.max(PAGE_SIZE);
            let end = entry.base.checked_add(entry.length)?;
            (start.checked_add(PAGE_SIZE)? <= end).then_some(start)
        })
        .expect("boot map must reserve kernel or bootloader pages");
    // SAFETY: All probes are invalid transitions, so none releases a frame or
    // touches its contents. Zero is excluded even if firmware marks it usable.
    unsafe {
        assert_eq!(frames.deallocate(0), Err(FrameError::NotUsable));
        assert_eq!(frames.deallocate(reserved), Err(FrameError::NotUsable));
        assert_eq!(
            frames.deallocate(metadata),
            Err(FrameError::MetadataReserved)
        );
        assert_eq!(frames.deallocate(virgin), Err(FrameError::NeverAllocated));
        assert_eq!(
            frames.deallocate(virgin + 1),
            Err(FrameError::UnalignedAddress)
        );
    }
    // SAFETY: The existing direct-map contract still holds. A second enable
    // fails before allocating or writing metadata and must preserve all state.
    unsafe {
        assert_eq!(
            frames.enable_recycling(offset),
            Err(RecyclingError::AlreadyEnabled)
        );
    }
    assert_eq!(frames.stats(), Some(before));

    let first = frames.allocate().expect("first recycling test frame");
    let second = frames.allocate().expect("second recycling test frame");
    // SAFETY: These distinct, freshly allocated pages are exclusively owned by
    // the test, and enable_recycling established their writable direct mapping.
    unsafe { check_frames(first, second, offset) };
    assert_eq!(
        frames.stats().unwrap().allocated_frames,
        before.allocated_frames + 2
    );
    // SAFETY: The test owns first and no reference, DMA, or additional mapping
    // exists. check_frames has finished; its dormant direct-map alias is unused.
    unsafe { frames.deallocate(first).expect("release first frame") };
    let after_release = frames.stats().unwrap();
    // SAFETY: This is a rejected double free, not an ownership transfer.
    unsafe { assert_eq!(frames.deallocate(first), Err(FrameError::AlreadyFree)) };
    assert_eq!(frames.stats(), Some(after_release));
    let reused = frames.allocate().expect("recycled test frame");
    // Allocation chooses the earliest recycled index, so any previously recycled
    // frames remaining after our two allocations follow first in bitmap order.
    assert_eq!(reused, first);
    let third = frames.allocate().expect("third recycling test frame");
    assert_ne!(third, first);
    assert_ne!(third, second);
    // SAFETY: Reacquiring first restored exclusive ownership. Both tested frames
    // are live, distinct, and the direct-map contract remains in effect.
    unsafe { check_frames(reused, second, offset) };
    // SAFETY: All three allocations are owned by this test, have no active
    // aliases or references, and will not be accessed after being returned.
    unsafe {
        frames.deallocate(reused).expect("release reused frame");
        frames.deallocate(second).expect("release second frame");
        frames.deallocate(third).expect("release third frame");
    }
    let after = frames.stats().unwrap();
    assert_eq!(after.allocated_frames, before.allocated_frames);
    assert_eq!(after.metadata_frames, before.metadata_frames);
    assert_eq!(after.total_frames, before.total_frames);
    assert_eq!(
        after.total_frames,
        after.allocated_frames
            + after.metadata_frames
            + after.recycled_frames
            + after.never_allocated_frames
    );
    assert_eq!(
        after.recycled_frames + after.never_allocated_frames,
        before.recycled_frames + before.never_allocated_frames
    );
    assert!(after.recycled_frames >= 3);
    crate::serial::println!("MIRROR_FRAME_REUSE_OK");
}

//! Physical-frame allocation and recycling from Limine USABLE ranges only.
//!
//! A bump allocator works before metadata is available. Once the direct map is
//! established, a two-bit state bitmap tracks every usable frame, including all
//! prior allocations. Its own pages remain reserved. Bootloader-reclaimable RAM
//! stays excluded because the bootstrap stack and responses are still live.

use limine::memory_map::{Entry, EntryType};

#[cfg(feature = "boot-smoke")]
use core::sync::atomic::{AtomicI64, Ordering};

pub const PAGE_SIZE: u64 = 4096;
pub const MAX_DMA_PAGES: usize = 4;
pub const DMA32_LIMIT: u64 = 1 << 32;

const NEVER_ALLOCATED: u8 = 0;
const ALLOCATED: u8 = 1;
const RECYCLED: u8 = 2;
const METADATA: u8 = 3;

/// 0054-D5's OomOrder scenario: a kernel-wide budget of net allocations
/// (allocate minus deallocate), armed only in that scenario; negative means
/// disarmed.
#[cfg(feature = "boot-smoke")]
static FRAME_BUDGET: AtomicI64 = AtomicI64::new(-1);

/// Arm the scenario's frame budget (boot-smoke only).
#[cfg(feature = "boot-smoke")]
pub fn arm_frame_budget(frames: i64) {
    FRAME_BUDGET.store(frames, Ordering::Relaxed);
}

/// Lift it before the session's drain (boot-smoke only).
#[cfg(feature = "boot-smoke")]
pub fn lift_frame_budget() {
    FRAME_BUDGET.store(-1, Ordering::Relaxed);
}

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
    InvalidPageCount,
    UnalignedAddress,
    NotUsable,
    NeverAllocated,
    AlreadyFree,
    MetadataReserved,
    /// 0054-D1: a share would push the frame's u16 refcount past 65535.
    ShareLimit,
    /// 0054-D1: a contiguous release requires refcount 1 on every page.
    Shared,
}

#[derive(Debug, PartialEq, Eq)]
pub enum ContiguousError {
    InvalidPageCount,
    RecyclingDisabled,
    OutOfFrames,
}

struct Recycling {
    bitmap: *mut u8,
    /// 0054-D1: the per-frame u16 refcount array, in its own reserved span.
    refcounts: *mut u16,
    /// 0054-D4: the reserve floor in frames, fixed when recycling starts.
    reserve_floor: usize,
    #[cfg(feature = "boot-smoke")]
    physical: u64,
    #[cfg(feature = "boot-smoke")]
    refcounts_physical: u64,
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

    /// The frame's share count (0054-D1): meaningful only while ALLOCATED.
    fn count(&self, index: usize) -> u16 {
        assert!(index < self.stats.total_frames);
        // SAFETY: enable_recycling reserves two bytes per frame and its caller
        // preserves the exclusive, writable direct mapping.
        unsafe { self.refcounts.add(index).read() }
    }

    fn set_count(&mut self, index: usize, value: u16) {
        assert!(index < self.stats.total_frames);
        // SAFETY: The refcount array is exclusively owned, initialized, and
        // mapped for this allocator's lifetime. The checked index bounds it.
        unsafe { self.refcounts.add(index).write(value) };
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
        #[cfg(feature = "boot-smoke")]
        {
            // 0054-D5's OomOrder budget caps net allocations while armed.
            let budget = FRAME_BUDGET.load(Ordering::Relaxed);
            if budget == 0 {
                return None;
            }
            if budget > 0 {
                FRAME_BUDGET.store(budget - 1, Ordering::Relaxed);
            }
        }
        if let Some(recycling) = &mut self.recycling
            && recycling.stats.recycled_frames != 0
        {
            let index = (recycling.recycled_hint..recycling.stats.total_frames)
                .find(|&index| recycling.state(index) == RECYCLED)
                .expect("recycled-frame accounting mismatch");
            recycling.set_state(index, ALLOCATED);
            recycling.set_count(index, 1);
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
                        // Contiguous allocations may reserve never-used pages
                        // ahead of this cursor, as may the metadata itself.
                        if matches!(recycling.state(index), METADATA | ALLOCATED) {
                            continue;
                        }
                        assert_eq!(recycling.state(index), NEVER_ALLOCATED);
                        recycling.set_state(index, ALLOCATED);
                        recycling.set_count(index, 1);
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

    /// Allocate 1..=MAX_DMA_PAGES consecutive pages in one USABLE range, with
    /// the entire run below 4 GiB. Recycling must already be enabled. The scan
    /// examines at most the usable frames below that address ceiling and never
    /// changes allocation state until a complete free run has been found.
    pub fn allocate_contiguous_dma32(&mut self, pages: usize) -> Result<u64, ContiguousError> {
        if !(1..=MAX_DMA_PAGES).contains(&pages) {
            return Err(ContiguousError::InvalidPageCount);
        }
        let recycling = self
            .recycling
            .as_ref()
            .ok_or(ContiguousError::RecyclingDisabled)?;
        let mut index_base = 0usize;
        let mut selected = None;
        for entry in self.entries {
            let Some((base, end)) = usable_bounds(entry) else {
                continue;
            };
            let count = ((end - base) / PAGE_SIZE) as usize;
            let eligible = (end.min(DMA32_LIMIT).saturating_sub(base) / PAGE_SIZE) as usize;
            let mut run = 0usize;
            for offset in 0..eligible {
                if matches!(
                    recycling.state(index_base + offset),
                    NEVER_ALLOCATED | RECYCLED
                ) {
                    run += 1;
                    if run == pages {
                        let first = offset + 1 - pages;
                        selected = Some((base + first as u64 * PAGE_SIZE, index_base + first));
                        break;
                    }
                } else {
                    run = 0;
                }
            }
            if selected.is_some() {
                break;
            }
            index_base += count;
        }
        let (physical, first) = selected.ok_or(ContiguousError::OutOfFrames)?;
        let recycling = self.recycling.as_mut().expect("bitmap initialized");
        for index in first..first + pages {
            match recycling.state(index) {
                NEVER_ALLOCATED => recycling.stats.never_allocated_frames -= 1,
                RECYCLED => recycling.stats.recycled_frames -= 1,
                _ => unreachable!("complete free run was preflighted"),
            }
            recycling.set_state(index, ALLOCATED);
            recycling.set_count(index, 1);
        }
        recycling.stats.allocated_frames += pages;
        // The recycled hint remains a valid lower bound even if this run
        // consumed its target: ordinary allocation skips non-recycled entries.
        Ok(physical)
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
        // 0054-D1: the refcount array lives in its own span beside the bitmap,
        // two bytes per frame, reserved through the same mechanism.
        let refcount_bytes = (total_frames as u64)
            .checked_mul(2)
            .ok_or(RecyclingError::MetadataTooLarge)?;
        let bitmap_span = align_up(bitmap_bytes as u64)
            .filter(|&bytes| bytes <= isize::MAX as u64)
            .ok_or(RecyclingError::MetadataTooLarge)?;
        let refcount_span = align_up(refcount_bytes)
            .filter(|&bytes| bytes <= isize::MAX as u64)
            .ok_or(RecyclingError::MetadataTooLarge)?;
        let metadata_frames = ((bitmap_span + refcount_span) / PAGE_SIZE) as usize;
        // Find one contiguous span per metadata array. The refcount span must
        // not overlap the bitmap span; both start at page-aligned addresses.
        let find_span = |bytes: u64, exclude: Option<(u64, u64)>| {
            self.entries
                .iter()
                .enumerate()
                .skip(self.region)
                .find_map(|(region, entry)| {
                    let (base, end) = usable_bounds(entry)?;
                    let mut start = if region == self.region {
                        self.next.max(base)
                    } else {
                        base
                    };
                    if let Some((excluded_base, excluded_end)) = exclude
                        && start < excluded_end
                        && excluded_base < start.saturating_add(bytes)
                    {
                        start = excluded_end;
                    }
                    (start.checked_add(bytes)? <= end).then_some(start)
                })
        };
        let physical = find_span(bitmap_span, None).ok_or(RecyclingError::NoMetadataSpace)?;
        let refcounts_physical = find_span(refcount_span, Some((physical, physical + bitmap_span)))
            .ok_or(RecyclingError::NoMetadataSpace)?;
        let address = offset
            .checked_add(physical)
            .filter(|&address| address != 0)
            .ok_or(RecyclingError::DirectMapOverflow)?;
        address
            .checked_add(bitmap_span - 1)
            .ok_or(RecyclingError::DirectMapOverflow)?;
        let refcounts_address = offset
            .checked_add(refcounts_physical)
            .filter(|&address| address != 0)
            .ok_or(RecyclingError::DirectMapOverflow)?;
        refcounts_address
            .checked_add(refcount_span - 1)
            .ok_or(RecyclingError::DirectMapOverflow)?;
        let bitmap = address as *mut u8;
        let refcounts = refcounts_address as *mut u16;
        // SAFETY: The selected pages are contiguous, previously unallocated
        // USABLE spans covered by the caller's direct-map and exclusivity
        // contract. The checked byte counts are nonzero and fit in isize.
        unsafe {
            bitmap.write_bytes(0, bitmap_span as usize);
            refcounts.write_bytes(0, (refcount_span / 2) as usize);
        }
        let metadata_index = self.index_of(physical).expect("metadata in usable RAM");
        let refcounts_index = self
            .index_of(refcounts_physical)
            .expect("refcounts in usable RAM");
        let mut recycling = Recycling {
            bitmap,
            refcounts,
            #[cfg(feature = "boot-smoke")]
            physical,
            #[cfg(feature = "boot-smoke")]
            refcounts_physical,
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
            reserve_floor: reserve_floor(total_frames),
        };
        // Bump allocation visits the compact usable-frame index in order, even
        // when memory-map entries are not sorted by physical address.
        for index in 0..allocated_frames {
            recycling.set_state(index, ALLOCATED);
            // 0054-D1: every frame live at boot holds one reference, so the
            // boot test frames still free (main.rs's early frame checks).
            recycling.set_count(index, 1);
        }
        for index in metadata_index..metadata_index + (bitmap_span / PAGE_SIZE) as usize {
            recycling.set_state(index, METADATA);
        }
        for index in refcounts_index..refcounts_index + (refcount_span / PAGE_SIZE) as usize {
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
    /// Run on the BSP with interrupts disabled and exclusive allocator access.
    /// APs must never use a released frame, even through the shared direct map.
    /// Invalid frame values may be submitted to test validation; they never
    /// cause a frame write.
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
                // 0054-D1: a shared frame recycles only when its count reaches
                // zero; dropping one reference changes no state or accounting.
                let count = recycling.count(index);
                if count > 1 {
                    recycling.set_count(index, count - 1);
                    return Ok(());
                }
                recycling.set_state(index, RECYCLED);
                recycling.recycled_hint = recycling.recycled_hint.min(index);
                recycling.stats.allocated_frames -= 1;
                recycling.stats.recycled_frames += 1;
                // 0054-D5: a real recycle restores the armed scenario budget.
                #[cfg(feature = "boot-smoke")]
                if FRAME_BUDGET.load(Ordering::Relaxed) >= 0 {
                    FRAME_BUDGET.fetch_add(1, Ordering::Relaxed);
                }
                Ok(())
            }
            _ => unreachable!("two-bit frame state"),
        }
    }

    /// Add one reference to an allocated frame (0054-D1): it recycles only
    /// when its count returns to zero. Fails with `ShareLimit` at 65535,
    /// leaving the count unchanged. METADATA and free frames are rejected.
    /// Callers follow deallocate's BSP + IF=0 rule; no memory contents move.
    // Only 0054-D1's tests call this until 0025's shared memory and the ELF
    // loader v2 arrive.
    #[cfg_attr(not(feature = "boot-smoke"), allow(dead_code))]
    pub fn share(&mut self, frame: u64) -> Result<(), FrameError> {
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
                let count = recycling.count(index);
                if count == u16::MAX {
                    return Err(FrameError::ShareLimit);
                }
                recycling.set_count(index, count + 1);
                Ok(())
            }
            _ => unreachable!("two-bit frame state"),
        }
    }

    /// Release a bounded run atomically: every frame is validated before any
    /// bitmap state changes. A failed release retains the entire allocation.
    ///
    /// # Safety
    /// The caller must satisfy deallocate's ownership, quiescence, mapping and
    /// BSP ownership requirements for every page. Use the same allocator that
    /// allocated the run. Invalid ranges may be submitted as validation probes.
    pub unsafe fn deallocate_contiguous(
        &mut self,
        physical: u64,
        pages: usize,
    ) -> Result<(), FrameError> {
        if !(1..=MAX_DMA_PAGES).contains(&pages) {
            return Err(FrameError::InvalidPageCount);
        }
        let recycling = self
            .recycling
            .as_ref()
            .ok_or(FrameError::RecyclingDisabled)?;
        if !physical.is_multiple_of(PAGE_SIZE) {
            return Err(FrameError::UnalignedAddress);
        }
        let mut indices = [0usize; MAX_DMA_PAGES];
        for (offset, index) in indices.iter_mut().take(pages).enumerate() {
            let frame = physical
                .checked_add(offset as u64 * PAGE_SIZE)
                .ok_or(FrameError::NotUsable)?;
            *index = self.index_of(frame).ok_or(FrameError::NotUsable)?;
            match recycling.state(*index) {
                NEVER_ALLOCATED => return Err(FrameError::NeverAllocated),
                RECYCLED => return Err(FrameError::AlreadyFree),
                METADATA => return Err(FrameError::MetadataReserved),
                // 0054-D1: a contiguous run is owned outright only while every
                // page holds exactly one reference.
                ALLOCATED if recycling.count(*index) != 1 => return Err(FrameError::Shared),
                ALLOCATED => {}
                _ => unreachable!("two-bit frame state"),
            }
        }
        let recycling = self.recycling.as_mut().expect("bitmap initialized");
        for &index in indices.iter().take(pages) {
            recycling.set_state(index, RECYCLED);
            recycling.recycled_hint = recycling.recycled_hint.min(index);
        }
        recycling.stats.allocated_frames -= pages;
        recycling.stats.recycled_frames += pages;
        // 0054-D5: a real recycle restores the armed scenario budget.
        #[cfg(feature = "boot-smoke")]
        if FRAME_BUDGET.load(Ordering::Relaxed) >= 0 {
            FRAME_BUDGET.fetch_add(pages as i64, Ordering::Relaxed);
        }
        Ok(())
    }

    pub fn stats(&self) -> Option<FrameStats> {
        self.recycling.as_ref().map(|recycling| recycling.stats)
    }

    /// 0054-D4: the reserve floor in frames, fixed when recycling started.
    pub fn reserve_floor(&self) -> usize {
        self.recycling
            .as_ref()
            .map(|recycling| recycling.reserve_floor)
            .unwrap_or(0)
    }

    /// 0054-D4: preflight one operation's worst-case frame count against the
    /// reserve floor, so no partial operation straddles it. The kernel and
    /// protected classes may draw the floor; any other operation may not start
    /// if it would end below it. Before recycling starts there is no floor.
    pub fn preflight_reserve(&self, pages: usize, may_draw_reserve: bool) -> bool {
        let Some(recycling) = self.recycling.as_ref() else {
            return true;
        };
        let free = recycling.stats.recycled_frames + recycling.stats.never_allocated_frames;
        if may_draw_reserve {
            free >= pages
        } else {
            free >= pages + recycling.reserve_floor
        }
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

/// 0054-D4: the reserve floor in frames — the smaller of 16 MiB and one
/// sixteenth of the usable frames.
fn reserve_floor(total_frames: usize) -> usize {
    (total_frames / 16).min(4096)
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
    self_test_contiguous();
    crate::serial::println!("MIRROR_ALLOCATOR_TESTS_OK");
}

/// Exercise fragmentation and address ceilings using a private state bitmap.
/// Synthetic physical numbers are never dereferenced or given to a device.
#[cfg(feature = "boot-smoke")]
fn self_test_contiguous() {
    const COUNT: usize = 18;
    fn snapshot(frames: &FrameAllocator<'_>) -> [u8; COUNT] {
        core::array::from_fn(|index| frames.recycling.as_ref().unwrap().state(index))
    }
    let entries = [
        Entry {
            base: 0x1000,
            length: 6 * PAGE_SIZE,
            entry_type: EntryType::USABLE,
        },
        Entry {
            base: 0x7000,
            length: PAGE_SIZE,
            entry_type: EntryType::RESERVED,
        },
        Entry {
            base: 0x8000,
            length: 4 * PAGE_SIZE,
            entry_type: EntryType::USABLE,
        },
        Entry {
            base: DMA32_LIMIT - 2 * PAGE_SIZE,
            length: 4 * PAGE_SIZE,
            entry_type: EntryType::USABLE,
        },
        Entry {
            base: DMA32_LIMIT + 3 * PAGE_SIZE,
            length: 4 * PAGE_SIZE,
            entry_type: EntryType::USABLE,
        },
    ];
    let refs = entries.each_ref();
    let mut frames = FrameAllocator::new(&refs);
    assert_eq!(
        frames.allocate_contiguous_dma32(1),
        Err(ContiguousError::RecyclingDisabled)
    );
    let mut bitmap = [0u8; COUNT.div_ceil(4)];
    // 0054-D1: the private count array beside the private bitmap.
    let mut counts = [0u16; COUNT];
    let mut recycling = Recycling {
        bitmap: bitmap.as_mut_ptr(),
        refcounts: counts.as_mut_ptr(),
        physical: 0x1000,
        refcounts_physical: 0,
        direct_map_offset: 0,
        stats: FrameStats {
            total_frames: COUNT,
            allocated_frames: 2,
            recycled_frames: 4,
            metadata_frames: 1,
            never_allocated_frames: 11,
        },
        recycled_hint: 2,
        reserve_floor: 1,
    };
    let initial = [
        METADATA,
        ALLOCATED,
        RECYCLED,
        ALLOCATED,
        NEVER_ALLOCATED,
        RECYCLED,
        NEVER_ALLOCATED,
        RECYCLED,
        NEVER_ALLOCATED,
        NEVER_ALLOCATED,
        RECYCLED,
        NEVER_ALLOCATED,
        NEVER_ALLOCATED,
        NEVER_ALLOCATED,
        NEVER_ALLOCATED,
        NEVER_ALLOCATED,
        NEVER_ALLOCATED,
        NEVER_ALLOCATED,
    ];
    for (index, state) in initial.into_iter().enumerate() {
        recycling.set_state(index, state);
        if state == ALLOCATED {
            recycling.set_count(index, 1);
        }
    }
    // This private bitmap, rather than a mapped synthetic physical address,
    // remains writable and exclusively owned until the allocator is dropped.
    frames.recycling = Some(recycling);
    let before = frames.stats();
    for pages in [0, MAX_DMA_PAGES + 1, usize::MAX] {
        assert_eq!(
            frames.allocate_contiguous_dma32(pages),
            Err(ContiguousError::InvalidPageCount)
        );
    }
    assert_eq!(frames.stats(), before);
    assert_eq!(snapshot(&frames), initial);

    // No run crosses the reserved hole. The first qualifying run mixes recycled
    // and never-used pages, after a fragmented earlier region.
    let three = frames
        .allocate_contiguous_dma32(3)
        .expect("mixed contiguous run");
    assert_eq!(three, 0x8000);
    let before = frames.stats();
    let states = snapshot(&frames);
    let cursor = (frames.region, frames.next);
    assert_eq!(
        frames.allocate_contiguous_dma32(4),
        Err(ContiguousError::OutOfFrames)
    );
    assert_eq!(frames.stats(), before);
    assert_eq!(snapshot(&frames), states);
    assert_eq!((frames.region, frames.next), cursor);
    // SAFETY: This test owns the private bitmap and all synthetic allocations;
    // there are no real frame accesses, DMA operations, or secondary aliases.
    unsafe { frames.deallocate_contiguous(three, 3) }.expect("release synthetic run");
    let four = frames
        .allocate_contiguous_dma32(4)
        .expect("maximum contiguous run");
    assert_eq!(four, 0x8000);
    let two = frames
        .allocate_contiguous_dma32(2)
        .expect("earlier two-page run");
    assert_eq!(two, 0x5000);
    let before = frames.stats();
    let states = snapshot(&frames);
    // SAFETY: The third page is reserved; atomic validation must leave both
    // owned pages live. This changes only the test's private allocator bitmap.
    unsafe {
        assert_eq!(
            frames.deallocate_contiguous(two, 3),
            Err(FrameError::NotUsable)
        );
    }
    assert_eq!(frames.stats(), before);
    assert_eq!(snapshot(&frames), states);

    let edge = frames
        .allocate_contiguous_dma32(2)
        .expect("DMA32 ceiling run");
    assert_eq!(edge + 2 * PAGE_SIZE, DMA32_LIMIT);
    let before = frames.stats();
    let states = snapshot(&frames);
    assert_eq!(
        frames.allocate_contiguous_dma32(3),
        Err(ContiguousError::OutOfFrames)
    );
    // SAFETY: The third page exists above the DMA ceiling but was never
    // allocated. Rejecting it must retain the first two pages atomically.
    unsafe {
        assert_eq!(
            frames.deallocate_contiguous(edge, 3),
            Err(FrameError::NeverAllocated)
        );
    }
    assert_eq!(frames.stats(), before);
    assert_eq!(snapshot(&frames), states);
    // SAFETY: The test owns these synthetic pages; no access uses their contents.
    unsafe { frames.deallocate_contiguous(edge, 2) }.expect("release ceiling run");

    // Drain ordinary allocation while the out-of-order contiguous reservations
    // stay live. This proves the bump cursor skips them rather than returning
    // aliases or asserting when it eventually reaches their bitmap positions.
    let mut live = [0u64; COUNT - 1];
    live[..8].copy_from_slice(&[
        0x2000,
        0x4000,
        two,
        two + PAGE_SIZE,
        four,
        four + PAGE_SIZE,
        four + 2 * PAGE_SIZE,
        four + 3 * PAGE_SIZE,
    ]);
    let mut count = 8;
    while let Some(physical) = frames.allocate() {
        assert_ne!(physical, 0x1000, "metadata was allocated");
        assert!(!live[..count].contains(&physical), "live frame was aliased");
        live[count] = physical;
        count += 1;
    }
    assert_eq!(count, COUNT - 1);
    assert!(live.iter().any(|&physical| physical >= DMA32_LIMIT));
    let before = frames.stats();
    let states = snapshot(&frames);
    assert_eq!(
        frames.allocate_contiguous_dma32(1),
        Err(ContiguousError::OutOfFrames)
    );
    assert_eq!(frames.stats(), before);
    assert_eq!(snapshot(&frames), states);
    for physical in live {
        // SAFETY: Every synthetic allocation is unique and test-owned, and this
        // private bitmap is the only memory accessed by the release operation.
        unsafe { frames.deallocate(physical) }.expect("reclaim synthetic frame");
    }
    assert_eq!(
        frames.stats(),
        Some(FrameStats {
            total_frames: COUNT,
            allocated_frames: 0,
            recycled_frames: COUNT - 1,
            metadata_frames: 1,
            never_allocated_frames: 0,
        })
    );
    crate::serial::println!("MIRROR_CONTIGUOUS_TESTS_OK");
}

/// Exercise recycling on actual RAM with the initialized direct-map contract.
#[cfg(feature = "boot-smoke")]
pub fn self_test_recycling(frames: &mut FrameAllocator<'_>) {
    let before = frames.stats().expect("recycling must be enabled");
    let recycling = frames.recycling.as_ref().expect("frame bitmap");
    let metadata = recycling.physical;
    let refcounts_metadata = recycling.refcounts_physical;
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
        assert_eq!(
            frames.deallocate(refcounts_metadata),
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

    // 0054 acceptance 3: the floor, fixed at enable_recycling, refuses a
    // non-protected operation that would end below it, while the kernel's own
    // allocation still succeeds; the frame counts are restored exactly.
    let floor = frames.reserve_floor();
    assert_eq!(floor, 4096.min(before.total_frames / 16));
    let free = before.recycled_frames + before.never_allocated_frames;
    assert!(frames.preflight_reserve(free - floor, false));
    assert!(!frames.preflight_reserve(free - floor + 1, false));
    assert!(frames.preflight_reserve(free - floor + 1, true));
    let kernel_frame = frames.allocate().expect("kernel frame beside the floor");
    // SAFETY: the test owns this frame outright, with no alias or DMA.
    unsafe {
        frames
            .deallocate(kernel_frame)
            .expect("release kernel frame")
    };
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

    // 0054 acceptance 2: a shared frame recycles only when its count reaches
    // zero, with the two references freed in either order.
    let shared_a = frames.allocate().expect("shared frame A");
    let shared_b = frames.allocate().expect("shared frame B");
    frames.share(shared_a).expect("share A");
    frames.share(shared_b).expect("share B");
    let before_shares = frames.stats().unwrap();
    // SAFETY: each deallocate drops one of the test's two references; the other
    // keeps the frame live, so the second call is the sole remaining owner.
    unsafe {
        frames
            .deallocate(shared_a)
            .expect("drop A's first reference");
        frames
            .deallocate(shared_b)
            .expect("drop B's first reference");
        assert_eq!(frames.stats(), Some(before_shares));
        frames
            .deallocate(shared_a)
            .expect("drop A's last reference");
        frames
            .deallocate(shared_b)
            .expect("drop B's last reference");
    }
    let after_shares = frames.stats().unwrap();
    assert_eq!(
        after_shares.allocated_frames,
        before_shares.allocated_frames - 2
    );
    // The ShareLimit boundary: a count at u16::MAX rejects the next share and
    // stays unchanged. The test sets the boundary directly; 65534 increments
    // would prove nothing more.
    let limit = frames.allocate().expect("share-limit frame");
    let limit_index = frames.index_of(limit).expect("test frame indexed");
    frames.share(limit).expect("share before the boundary");
    frames
        .recycling
        .as_mut()
        .expect("bitmap initialized")
        .set_count(limit_index, u16::MAX);
    assert_eq!(frames.share(limit), Err(FrameError::ShareLimit));
    assert_eq!(
        frames.recycling.as_ref().unwrap().count(limit_index),
        u16::MAX,
        "a rejected share changed the count"
    );
    // SAFETY: restore the limit frame to one reference, then free it; the test
    // owns it outright and no alias exists.
    unsafe {
        frames.recycling.as_mut().unwrap().set_count(limit_index, 1);
        frames.deallocate(limit).expect("release share-limit frame");
    }
    // A contiguous run with any count above 1 is rejected before any change.
    let run = frames
        .allocate_contiguous_dma32(2)
        .expect("two-page share run");
    frames.share(run).expect("share the run's first page");
    let before_run = frames.stats().unwrap();
    // SAFETY: an invalid transition probe; the run stays fully allocated.
    unsafe {
        assert_eq!(
            frames.deallocate_contiguous(run, 2),
            Err(FrameError::Shared)
        );
    }
    assert_eq!(frames.stats(), Some(before_run));
    // SAFETY: drop the extra reference, then the whole run the test owns.
    unsafe {
        frames
            .deallocate(run)
            .expect("drop the run's extra reference");
        frames
            .deallocate_contiguous(run, 2)
            .expect("release the run");
    }
    // Rejected non-allocated shares, now that every test frame is released:
    // the recycled, metadata and never-allocated states.
    assert_eq!(frames.share(shared_a), Err(FrameError::AlreadyFree));
    assert_eq!(frames.share(metadata), Err(FrameError::MetadataReserved));
    assert_eq!(frames.share(virgin), Err(FrameError::NeverAllocated));
    assert_eq!(frames.share(virgin + 1), Err(FrameError::UnalignedAddress));
    assert_eq!(
        frames.stats().unwrap().allocated_frames,
        before_shares.allocated_frames - 2,
        "share tests must restore the frame baseline"
    );
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

//! Exclusive DMA32 buffers backed by Mirror's real physical-frame allocator.
//!
//! These are ordinary, contiguous USABLE RAM pages through the existing HHDM,
//! not MMIO mappings. Device access, volatile descriptor fields, and ordering
//! belong to the driver. There is deliberately no Drop: a buffer whose device
//! cannot be reset must remain allocated (quarantined) for the rest of the boot.

use core::{marker::PhantomData, ptr::NonNull};

use crate::memory::{
    ContiguousError, DMA32_LIMIT, FrameAllocator, FrameError, MAX_DMA_PAGES, PAGE_SIZE,
};

#[derive(Debug, PartialEq, Eq)]
pub enum DmaError {
    InvalidPageCount,
    RecyclingRequired,
    OutOfFrames,
    UnalignedDirectMap,
    DirectMapOverflow,
    NonCanonicalAddress,
    ReleaseFailed(FrameError),
}

/// Sole ownership of physical pages and their dormant direct-map alias.
/// Neither Clone nor Send/Sync; only explicit, quiesced release frees pages.
#[must_use = "DMA memory must be explicitly released or intentionally quarantined"]
pub struct DmaBuffer {
    physical: u64,
    address: NonNull<u8>,
    pages: usize,
    _boot_cpu_only: PhantomData<*mut ()>,
}

impl DmaBuffer {
    /// Allocate and zero a page-aligned run of 1..=4 pages below 4 GiB.
    ///
    /// # Safety
    /// Run on the sole boot CPU with interrupts disabled, using the unique real
    /// frame allocator. Every USABLE frame must remain mapped writable at
    /// direct_map + physical for this buffer's lifetime. No aliases, references,
    /// or device may access the newly allocated pages during initialization.
    /// Before publishing the address to hardware, the driver must establish
    /// device ownership and ordering; the allocator does not configure an IOMMU.
    pub unsafe fn allocate(
        frames: &mut FrameAllocator<'_>,
        direct_map: u64,
        pages: usize,
    ) -> Result<Self, DmaError> {
        if !(1..=MAX_DMA_PAGES).contains(&pages) {
            return Err(DmaError::InvalidPageCount);
        }
        if !direct_map.is_multiple_of(PAGE_SIZE) {
            return Err(DmaError::UnalignedDirectMap);
        }
        let physical = frames
            .allocate_contiguous_dma32(pages)
            .map_err(|error| match error {
                ContiguousError::InvalidPageCount => DmaError::InvalidPageCount,
                ContiguousError::RecyclingDisabled => DmaError::RecyclingRequired,
                ContiguousError::OutOfFrames => DmaError::OutOfFrames,
            })?;
        let length = pages * PAGE_SIZE as usize;
        debug_assert!(physical + length as u64 <= DMA32_LIMIT);
        let address = match direct_pointer(direct_map, physical, length) {
            Ok(address) => address,
            Err(error) => {
                // SAFETY: This fresh allocation was never mapped elsewhere,
                // referenced, initialized, or published to a device.
                unsafe { frames.deallocate_contiguous(physical, pages) }
                    .map_err(DmaError::ReleaseFailed)?;
                return Err(error);
            }
        };
        // SAFETY: The caller guarantees the full run's writable direct map and
        // exclusive CPU ownership. The checked, page-aligned run is nonempty
        // and at most 16 KiB; the device has not been given its address yet.
        unsafe { address.as_ptr().write_bytes(0, length) };
        Ok(Self {
            physical,
            address,
            pages,
            _boot_cpu_only: PhantomData,
        })
    }

    pub fn physical_address(&self) -> u64 {
        self.physical
    }

    pub fn len(&self) -> usize {
        self.pages * PAGE_SIZE as usize
    }

    /// Raw access only: the caller must coordinate every dereference with the
    /// device, enforce bounds/alignment, and avoid Rust references to DMA fields.
    pub fn as_ptr(&self) -> *mut u8 {
        self.address.as_ptr()
    }

    /// Return the entire run after device ownership has ended. Failure retains
    /// every frame in the allocator; no Drop path can free live DMA memory.
    ///
    /// # Safety
    /// The driver must fully reset/quiesce the device and prove it cannot access
    /// this run again. All raw pointers, references, and non-direct-map aliases
    /// must be retired. Use the original allocator on the sole boot CPU with
    /// interrupts disabled. If reset fails, retain/quarantine this buffer instead.
    pub unsafe fn release(self, frames: &mut FrameAllocator<'_>) -> Result<(), DmaError> {
        // SAFETY: The caller has retired every device/CPU use and provides the
        // original exclusive allocator. The bounded release preflights all pages.
        unsafe { frames.deallocate_contiguous(self.physical, self.pages) }
            .map_err(DmaError::ReleaseFailed)
    }
}

fn direct_pointer(offset: u64, physical: u64, length: usize) -> Result<NonNull<u8>, DmaError> {
    let address = offset
        .checked_add(physical)
        .ok_or(DmaError::DirectMapOverflow)?;
    let end = address
        .checked_add(length as u64 - 1)
        .ok_or(DmaError::DirectMapOverflow)?;
    let canonical = |value: u64| value >> 47 == 0 || value >> 47 == 0x1ffff;
    if !canonical(address) || !canonical(end) {
        return Err(DmaError::NonCanonicalAddress);
    }
    NonNull::new(address as *mut u8).ok_or(DmaError::DirectMapOverflow)
}

#[cfg(feature = "boot-smoke")]
/// Exercise actual RAM allocations without publishing any address to hardware.
///
/// # Safety
/// The sole allocator and direct map must satisfy DmaBuffer::allocate's contract;
/// run on the boot CPU with interrupts disabled and recycling already enabled.
pub unsafe fn self_test(frames: &mut FrameAllocator<'_>, direct_map: u64) {
    let before = frames.stats().expect("DMA requires recycling");
    // SAFETY: Invalid page counts/alignment fail before physical allocation or
    // memory access. These probes do not rely on the supplied mapping.
    unsafe {
        assert!(matches!(
            DmaBuffer::allocate(frames, direct_map, 0),
            Err(DmaError::InvalidPageCount)
        ));
        assert!(matches!(
            DmaBuffer::allocate(frames, direct_map, MAX_DMA_PAGES + 1),
            Err(DmaError::InvalidPageCount)
        ));
        assert!(matches!(
            DmaBuffer::allocate(frames, 1, 1),
            Err(DmaError::UnalignedDirectMap)
        ));
    }
    assert_eq!(frames.stats(), Some(before));

    // Invalid computed mappings fail before zeroing and roll the run back.
    // SAFETY: These offsets deterministically produce overflow/noncanonical
    // addresses for any DMA32 frame; neither probe accesses that address.
    unsafe {
        assert!(matches!(
            DmaBuffer::allocate(frames, !(PAGE_SIZE - 1), 1),
            Err(DmaError::DirectMapOverflow)
        ));
        assert!(matches!(
            DmaBuffer::allocate(frames, 0x0000_8000_0000_0000, 1),
            Err(DmaError::NonCanonicalAddress)
        ));
    }
    assert_eq!(
        frames.stats().unwrap().allocated_frames,
        before.allocated_frames
    );

    // SAFETY: Boot retains the real writable HHDM. The test owns the sole frame
    // allocator with interrupts disabled; no device receives these addresses.
    let ring = unsafe { DmaBuffer::allocate(frames, direct_map, 3) }.expect("DMA ring test");
    // SAFETY: Same exclusive allocator/direct-map contract as the ring allocation.
    let bounce = unsafe { DmaBuffer::allocate(frames, direct_map, 1) }.expect("DMA bounce test");
    assert!(
        ring.physical_address() + ring.len() as u64 <= bounce.physical_address()
            || bounce.physical_address() + bounce.len() as u64 <= ring.physical_address()
    );
    for buffer in [&ring, &bounce] {
        assert!(buffer.physical_address().is_multiple_of(PAGE_SIZE));
        assert!(buffer.physical_address() + buffer.len() as u64 <= DMA32_LIMIT);
        assert!((buffer.as_ptr() as usize).is_multiple_of(PAGE_SIZE as usize));
        for offset in 0..buffer.len() {
            // SAFETY: Freshly zeroed test-owned RAM; no device or alias accesses
            // it. The loop bounds every byte within this allocation.
            unsafe {
                assert_eq!(buffer.as_ptr().add(offset).read_volatile(), 0);
                buffer
                    .as_ptr()
                    .add(offset)
                    .write_volatile((offset as u8) ^ 0xa5);
            }
        }
        for offset in 0..buffer.len() {
            // SAFETY: Same live allocation and checked byte offsets as above.
            unsafe {
                assert_eq!(
                    buffer.as_ptr().add(offset).read_volatile(),
                    (offset as u8) ^ 0xa5
                );
            }
        }
    }
    assert_eq!(
        frames.stats().unwrap().allocated_frames,
        before.allocated_frames + 4
    );
    // SAFETY: Both test buffers were never published to hardware; all raw
    // accesses are finished and no non-direct-map alias or reference remains.
    unsafe {
        ring.release(frames).expect("release DMA ring test");
        bounce.release(frames).expect("release DMA bounce test");
    }
    // SAFETY: Same real mapping/ownership contract, exercising the maximum run.
    let maximum =
        unsafe { DmaBuffer::allocate(frames, direct_map, MAX_DMA_PAGES) }.expect("maximum DMA run");
    for offset in 0..maximum.len() {
        // SAFETY: This newly acquired, zeroed run has no device or other owner.
        unsafe { assert_eq!(maximum.as_ptr().add(offset).read_volatile(), 0) };
    }
    // SAFETY: All CPU accesses are complete and hardware never owned this run.
    unsafe { maximum.release(frames) }.expect("release maximum DMA run");
    let after = frames.stats().unwrap();
    assert_eq!(after.allocated_frames, before.allocated_frames);
    assert_eq!(after.metadata_frames, before.metadata_frames);
    assert_eq!(after.total_frames, before.total_frames);
    assert_eq!(
        after.never_allocated_frames + after.recycled_frames,
        before.never_allocated_frames + before.recycled_frames
    );
    crate::serial::println!("MIRROR_DMA_TESTS_OK");
}

//! Bounded, read-only VirtIO block I/O through the legacy PCI port transport.
//!
//! One boot CPU, queue 0, one in-flight 512-byte read, and owned coherent DMA
//! buffers. No interrupts, writes, indirect descriptors, event index, packed
//! rings, modern transport, hotplug, filesystem, or IOMMU is implemented here.
//!
//! Virtio 1.2: sections 2.4, 2.5.4, 2.7.2, 2.7.13/14, 3.1.2, 3.3,
//! 4.1.4.10, 4.1.5.1.3.1, and 5.2.6 (including legacy used-length caveats):
//! https://docs.oasis-open.org/virtio/virtio/v1.2/virtio-v1.2.html

use core::{
    fmt,
    marker::PhantomData,
    mem,
    sync::atomic::{Ordering, fence},
};

use crate::{
    arch,
    dma::{DmaBuffer, DmaError},
    memory::FrameAllocator,
    pci::{Inventory, LegacyBlock, LegacyError},
    serial,
};

const PAGE_SIZE: usize = 4096;
pub const SECTOR_SIZE: usize = 512;
const MAX_QUEUE_SIZE: u16 = 256;
const RESET_POLLS: usize = 100_000;
const REQUEST_POLLS: usize = 5_000_000;
const CONFIG_ATTEMPTS: usize = 8;
const DEVICE_FEATURES: u16 = 0;
const DRIVER_FEATURES: u16 = 4;
const QUEUE_ADDRESS: u16 = 8;
const QUEUE_SIZE: u16 = 12;
const QUEUE_SELECT: u16 = 14;
const QUEUE_NOTIFY: u16 = 16;
const DEVICE_STATUS: u16 = 18;
const ISR_STATUS: u16 = 19;
const CAPACITY_LOW: u16 = 20;
const CAPACITY_HIGH: u16 = 24;
const ACKNOWLEDGE: u8 = 1;
const DRIVER: u8 = 2;
const DRIVER_OK: u8 = 4;
const DEVICE_NEEDS_RESET: u8 = 64;
const FAILED: u8 = 128;
const READ_ONLY: u32 = 1 << 5;
const DESCRIPTOR_NEXT: u16 = 1;
const DESCRIPTOR_WRITE: u16 = 2;
const HEADER_SIZE: usize = 16;
const DATA_OFFSET: usize = HEADER_SIZE;
const STATUS_OFFSET: usize = DATA_OFFSET + SECTOR_SIZE;
const STATUS_SENTINEL: u8 = 0xff;

#[derive(Debug)]
pub enum BlockError {
    Pci(LegacyError),
    Dma(DmaError),
    InvalidQueueSize,
    InvalidDmaAddress,
    InvalidCapacity,
    UnstableConfiguration,
    ConfigurationChanged,
    FeatureReadback,
    QueueAlreadyConfigured,
    QueueAddressReadback,
    DeviceNotReady,
    SectorOutOfRange,
    UnexpectedUsedIndex,
    UnexpectedUsedHead,
    InvalidStatus,
    IoError,
    UnsupportedRequest,
    Timeout,
    ResetTimeout,
    DmaQuarantined,
}

impl fmt::Display for BlockError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Pci(error) => write!(formatter, "PCI transport: {error:?}"),
            Self::Dma(error) => write!(formatter, "DMA allocation: {error:?}"),
            other => write!(formatter, "{other:?}"),
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct QueueLayout {
    size: u16,
    available: usize,
    used: usize,
    pages: usize,
}

impl QueueLayout {
    fn new(size: u16) -> Result<Self, BlockError> {
        if !(4..=MAX_QUEUE_SIZE).contains(&size) || !size.is_power_of_two() {
            return Err(BlockError::InvalidQueueSize);
        }
        let size_bytes = usize::from(size);
        let available = size_bytes * 16;
        // Legacy layout reserves the extra event words even without EVENT_IDX.
        let used = align_page(available + 6 + size_bytes * 2);
        let length = used + align_page(6 + size_bytes * 8);
        Ok(Self {
            size,
            available,
            used,
            pages: length / PAGE_SIZE,
        })
    }

    fn slot(self, index: u16) -> usize {
        usize::from(index & (self.size - 1))
    }
}

fn align_page(value: usize) -> usize {
    (value + PAGE_SIZE - 1) & !(PAGE_SIZE - 1)
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Descriptor {
    address: u64,
    length: u32,
    flags: u16,
    next: u16,
}

const _: [(); 16] = [(); mem::size_of::<Descriptor>()];

fn read_descriptors(bounce: u64) -> Result<[Descriptor; 3], BlockError> {
    if bounce == 0
        || bounce & (PAGE_SIZE as u64 - 1) != 0
        || bounce
            .checked_add(PAGE_SIZE as u64)
            .is_none_or(|end| end > 1 << 32)
    {
        return Err(BlockError::InvalidDmaAddress);
    }
    Ok([
        Descriptor {
            address: bounce,
            length: HEADER_SIZE as u32,
            flags: DESCRIPTOR_NEXT,
            next: 1,
        },
        Descriptor {
            address: bounce + DATA_OFFSET as u64,
            length: SECTOR_SIZE as u32,
            flags: DESCRIPTOR_NEXT | DESCRIPTOR_WRITE,
            next: 2,
        },
        Descriptor {
            address: bounce + STATUS_OFFSET as u64,
            length: 1,
            flags: DESCRIPTOR_WRITE,
            next: 0,
        },
    ])
}

fn valid_capacity(capacity: u64) -> bool {
    capacity != 0 && capacity.checked_mul(SECTOR_SIZE as u64).is_some()
}

fn valid_sector(sector: u64, capacity: u64) -> bool {
    valid_capacity(capacity) && sector.checked_add(1).is_some_and(|end| end <= capacity)
}

fn validate_completion(
    previous: u16,
    current: u16,
    head: u32,
    _legacy_length: u32,
    status: u8,
) -> Result<(), BlockError> {
    if current.wrapping_sub(previous) != 1 {
        return Err(BlockError::UnexpectedUsedIndex);
    }
    if head != 0 {
        return Err(BlockError::UnexpectedUsedHead);
    }
    // Legacy devices historically report this length inconsistently. Never use
    // it for pointer arithmetic, copying, or deciding which bytes are valid.
    // The bounce page is zeroed each time; status determines request success.
    match status {
        0 => Ok(()),
        1 => Err(BlockError::IoError),
        2 => Err(BlockError::UnsupportedRequest),
        _ => Err(BlockError::InvalidStatus),
    }
}

// These helpers materialize no shared Rust references to DMA-visible data.
// Every internal offset is additionally checked before the volatile access.
fn dma_write<T: Copy>(buffer: &DmaBuffer, offset: usize, value: T) {
    assert!(
        offset
            .checked_add(mem::size_of::<T>())
            .is_some_and(|end| end <= buffer.len())
    );
    let pointer = buffer.as_ptr().wrapping_add(offset).cast::<T>();
    assert!(pointer.is_aligned());
    // SAFETY: The buffer owns mapped, initialized DMA memory and the checked
    // offset/alignment fit T. The caller writes only unexposed driver-owned data.
    unsafe { pointer.write_volatile(value) };
}

fn dma_read<T: Copy>(buffer: &DmaBuffer, offset: usize) -> T {
    assert!(
        offset
            .checked_add(mem::size_of::<T>())
            .is_some_and(|end| end <= buffer.len())
    );
    let pointer = buffer.as_ptr().wrapping_add(offset).cast::<T>();
    assert!(pointer.is_aligned());
    // SAFETY: All buffers are initialized by allocation or before submission;
    // checked scalar reads stay within the owned DMA mapping. Device writes are
    // observed through volatile access and explicit DMA ordering barriers.
    unsafe { pointer.read_volatile() }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum State {
    NeedsReset,
    Stopped,
    Live,
    Quarantined,
}

/// Owns PCI transport and DMA until explicit shutdown. There is intentionally
/// no Drop that could free memory while the device might still reference it.
#[must_use = "shut down the block driver to reset the device and release its DMA"]
pub struct Driver {
    transport: LegacyBlock,
    ring: Option<DmaBuffer>,
    bounce: Option<DmaBuffer>,
    layout: QueueLayout,
    capacity: u64,
    read_only: bool,
    next_available: u16,
    next_used: u16,
    state: State,
    _boot_cpu_only: PhantomData<*mut ()>,
}

impl Driver {
    /// # Safety
    /// Interrupts must be disabled, and the boot CPU must exclusively own the
    /// selected PCI device and allocator throughout initialization.
    /// `direct_map` must map its usable RAM coherently and writable at physical
    /// address + offset, with mappings retained until shutdown. No IOMMU address
    /// translation may differ from these physical DMA addresses. Device BARs,
    /// PCI capabilities, and mappings must remain fixed while this handle lives.
    pub unsafe fn open(
        inventory: &Inventory,
        frames: &mut FrameAllocator<'_>,
        direct_map: u64,
    ) -> Result<Option<Self>, BlockError> {
        let Some(transport) = LegacyBlock::discover(inventory).map_err(BlockError::Pci)? else {
            return Ok(None);
        };
        serial::println!(
            "VirtIO block transport: PCI {:02x}:{:02x}.{} io={:#x}",
            transport.bdf.bus,
            transport.bdf.slot,
            transport.bdf.function,
            transport.io_base
        );
        let mut driver = Self {
            transport,
            ring: None,
            bounce: None,
            layout: QueueLayout::new(4).expect("minimum supported queue"),
            capacity: 0,
            read_only: false,
            next_available: 0,
            next_used: 0,
            state: State::NeedsReset,
            _boot_cpu_only: PhantomData,
        };
        // SAFETY: The caller supplies exclusive PCI, allocator, and DMA mapping
        // ownership; initialize enables no DMA until after acknowledged reset.
        if let Err(error) = unsafe { driver.initialize(frames, direct_map) } {
            serial::println!("VirtIO block initialization failed: {:?}", error);
            // SAFETY: The same owning allocator and mapping are still live.
            // Cleanup resets before releasing even partially prepared buffers.
            unsafe { driver.shutdown(frames) }?;
            return Err(error);
        }
        Ok(Some(driver))
    }

    unsafe fn initialize(
        &mut self,
        frames: &mut FrameAllocator<'_>,
        direct_map: u64,
    ) -> Result<(), BlockError> {
        // SAFETY: Exclusive PCI ownership from open; only decode is enabled,
        // bus mastering and all interrupt delivery remain disabled for reset.
        unsafe { self.transport.enable_io_only() }.map_err(BlockError::Pci)?;
        self.reset()?;
        self.state = State::NeedsReset;
        self.set_status(ACKNOWLEDGE)?;
        self.set_status(ACKNOWLEDGE | DRIVER)?;
        let offered = self.read32(DEVICE_FEATURES);
        let accepted = offered & READ_ONLY;
        self.read_only = accepted != 0;
        self.write32(DRIVER_FEATURES, accepted);
        if self.read32(DRIVER_FEATURES) != accepted {
            return Err(BlockError::FeatureReadback);
        }
        // The legacy transport omits FEATURES_OK and its readback (Virtio3.1.2).
        self.write16(QUEUE_SELECT, 0);
        self.layout = QueueLayout::new(self.read16(QUEUE_SIZE))?;
        if self.read32(QUEUE_ADDRESS) != 0 {
            return Err(BlockError::QueueAlreadyConfigured);
        }
        self.capacity = self.read_capacity()?;
        // SAFETY: open's contract supplies the sole allocator and retained
        // coherent direct map; no address is published before both allocations.
        self.ring = Some(
            unsafe { DmaBuffer::allocate(frames, direct_map, self.layout.pages) }
                .map_err(BlockError::Dma)?,
        );
        // SAFETY: Same exclusive allocator and direct-map ownership.
        self.bounce =
            Some(unsafe { DmaBuffer::allocate(frames, direct_map, 1) }.map_err(BlockError::Dma)?);
        let ring = self.ring.as_ref().expect("allocated ring");
        let physical = ring.physical_address();
        if physical == 0
            || physical & (PAGE_SIZE as u64 - 1) != 0
            || physical
                .checked_add(ring.len() as u64)
                .is_none_or(|end| end > 1 << 32)
        {
            return Err(BlockError::InvalidDmaAddress);
        }
        read_descriptors(
            self.bounce
                .as_ref()
                .expect("allocated bounce")
                .physical_address(),
        )?;
        dma_write(ring, self.layout.available, 1u16); // VIRTQ_AVAIL_F_NO_INTERRUPT
        dma_write(ring, self.layout.available + 2, 0u16);
        dma_write(ring, self.layout.used, 0u16);
        dma_write(ring, self.layout.used + 2, 0u16);
        fence(Ordering::SeqCst);
        // SAFETY: Reset was acknowledged and DMA storage is owned, zeroed, and
        // ready. PCI MSI/MSI-X are rejected and legacy INTx remains disabled.
        unsafe { self.transport.enable() }.map_err(BlockError::Pci)?;
        let pfn = (physical / PAGE_SIZE as u64) as u32;
        self.write32(QUEUE_ADDRESS, pfn);
        if self.read32(QUEUE_ADDRESS) != pfn {
            return Err(BlockError::QueueAddressReadback);
        }
        self.set_status(ACKNOWLEDGE | DRIVER | DRIVER_OK)?;
        self.state = State::Live;
        Ok(())
    }

    pub fn capacity_sectors(&self) -> u64 {
        self.capacity
    }
    pub fn queue_size(&self) -> u16 {
        self.layout.size
    }
    pub fn is_read_only(&self) -> bool {
        self.read_only
    }

    /// Read one sector. No write request is ever generated. Caller output is
    /// changed only after a complete, validated successful request; completed
    /// IOERR/UNSUPP results remain retryable on the same queue.
    pub fn read_sector(
        &mut self,
        sector: u64,
        output: &mut [u8; SECTOR_SIZE],
    ) -> Result<(), BlockError> {
        if self.state != State::Live {
            return Err(BlockError::DeviceNotReady);
        }
        if !valid_sector(sector, self.capacity) {
            return Err(BlockError::SectorOutOfRange);
        }
        match self.perform_read(sector, output) {
            Ok(()) => Ok(()),
            Err(error @ (BlockError::IoError | BlockError::UnsupportedRequest)) => Err(error),
            Err(error) => {
                self.write8(DEVICE_STATUS, self.read8(DEVICE_STATUS) | FAILED);
                if self.reset().is_err() {
                    self.quarantine();
                    return Err(BlockError::DmaQuarantined);
                }
                Err(error)
            }
        }
    }

    fn perform_read(
        &mut self,
        sector: u64,
        output: &mut [u8; SECTOR_SIZE],
    ) -> Result<(), BlockError> {
        if !ready_status(self.read8(DEVICE_STATUS)) {
            return Err(BlockError::DeviceNotReady);
        }
        if self.read_capacity()? != self.capacity {
            return Err(BlockError::ConfigurationChanged);
        }
        let ring = self.ring.as_ref().expect("live queue owns ring");
        let bounce = self.bounce.as_ref().expect("live queue owns bounce");
        if dma_read::<u16>(ring, self.layout.used + 2) != self.next_used {
            return Err(BlockError::UnexpectedUsedIndex);
        }
        // Device returned the sole prior request (or no request existed). It
        // cannot touch these buffers until the new available index is published.
        for offset in 0..=STATUS_OFFSET {
            dma_write(bounce, offset, 0u8);
        }
        dma_write(bounce, 0, 0u32); // VIRTIO_BLK_T_IN only.
        dma_write(bounce, 4, 0u32); // Reserved field.
        dma_write(bounce, 8, sector);
        dma_write(bounce, STATUS_OFFSET, STATUS_SENTINEL);
        for (index, descriptor) in read_descriptors(bounce.physical_address())?
            .into_iter()
            .enumerate()
        {
            dma_write(ring, index * 16, descriptor);
        }
        dma_write(
            ring,
            self.layout.available + 4 + self.layout.slot(self.next_available) * 2,
            0u16,
        );
        // The device must see the complete header, buffers, descriptor chain,
        // and available slot before it can observe the incremented index.
        fence(Ordering::SeqCst);
        self.next_available = self.next_available.wrapping_add(1);
        dma_write(ring, self.layout.available + 2, self.next_available);
        fence(Ordering::SeqCst);
        // Event-index was not negotiated. Ignore notification suppression and
        // always kick this single request; redundant notifications are harmless.
        self.write16(QUEUE_NOTIFY, 0);

        for attempt in 0..REQUEST_POLLS {
            let used = dma_read::<u16>(ring, self.layout.used + 2);
            if used != self.next_used {
                // Acquire device-written used entries, status, and data only
                // after observing its published used index. x86 DMA is coherent.
                fence(Ordering::SeqCst);
                let offset = self.layout.used + 4 + self.layout.slot(self.next_used) * 8;
                let head = dma_read::<u32>(ring, offset);
                let length = dma_read::<u32>(ring, offset + 4);
                let status = dma_read::<u8>(bounce, STATUS_OFFSET);
                let result = validate_completion(self.next_used, used, head, length, status);
                if !matches!(
                    result,
                    Ok(()) | Err(BlockError::IoError | BlockError::UnsupportedRequest)
                ) {
                    return result;
                }
                // A valid used head returns ownership even when I/O failed.
                self.next_used = used;
                let interrupt_status = self.read8(ISR_STATUS);
                if interrupt_status & 2 != 0 || !ready_status(self.read8(DEVICE_STATUS)) {
                    return Err(BlockError::ConfigurationChanged);
                }
                result?;
                for (index, byte) in output.iter_mut().enumerate() {
                    *byte = dma_read(bounce, DATA_OFFSET + index);
                }
                return Ok(());
            }
            // Periodic bounded port reads also observe a device reset/error,
            // and allow emulators to service asynchronous backend completion.
            if attempt & 1023 == 0 && !ready_status(self.read8(DEVICE_STATUS)) {
                return Err(BlockError::DeviceNotReady);
            }
            core::hint::spin_loop();
        }
        Err(BlockError::Timeout)
    }

    fn read_capacity(&self) -> Result<u64, BlockError> {
        let sample =
            || u64::from(self.read32(CAPACITY_LOW)) | (u64::from(self.read32(CAPACITY_HIGH)) << 32);
        let mut previous = sample();
        for _ in 0..CONFIG_ATTEMPTS {
            let current = sample();
            if current == previous {
                return if valid_capacity(current) {
                    Ok(current)
                } else {
                    Err(BlockError::InvalidCapacity)
                };
            }
            previous = current;
        }
        Err(BlockError::UnstableConfiguration)
    }

    fn set_status(&self, status: u8) -> Result<(), BlockError> {
        self.write8(DEVICE_STATUS, status);
        let actual = self.read8(DEVICE_STATUS);
        if actual & status != status || actual & (FAILED | DEVICE_NEEDS_RESET) != 0 {
            Err(BlockError::DeviceNotReady)
        } else {
            Ok(())
        }
    }

    fn reset(&mut self) -> Result<(), BlockError> {
        fence(Ordering::SeqCst);
        self.write8(DEVICE_STATUS, 0);
        for _ in 0..RESET_POLLS {
            if self.read8(DEVICE_STATUS) == 0 {
                fence(Ordering::SeqCst);
                self.state = State::Stopped;
                return Ok(());
            }
            core::hint::spin_loop();
        }
        Err(BlockError::ResetTimeout)
    }

    fn quarantine(&mut self) {
        self.state = State::Quarantined;
        // SAFETY: The transport remains exclusively claimed. Disabling bus
        // mastering is additional containment, never proof that DMA has ended.
        let command_result = unsafe { self.transport.disable_bus_master() };
        serial::println!(
            "MIRROR_BLOCK_DMA_QUARANTINED bus_master_disable={:?}",
            command_result
        );
    }

    /// # Safety
    /// Interrupts must be disabled, and `frames` must be the same owning
    /// allocator passed to open. The original DMA mapping and exclusive PCI
    /// ownership must remain valid through reset and frame release.
    /// If reset cannot be acknowledged, buffers and the PCI claim stay reserved;
    /// they are never returned to the allocator merely because polling stopped.
    pub unsafe fn shutdown(mut self, frames: &mut FrameAllocator<'_>) -> Result<(), BlockError> {
        if self.state != State::Stopped && self.reset().is_err() {
            self.quarantine();
            return Err(BlockError::DmaQuarantined);
        }
        fence(Ordering::SeqCst);
        // SAFETY: A zero device-status read acknowledged reset and stopped all
        // queue access. Restoring the original command cannot revive reset queues.
        let restored = unsafe { self.transport.restore() }.map_err(BlockError::Pci);
        // Attempt both releases even if command restoration or one release fails.
        let ring_result = if let Some(ring) = self.ring.take() {
            // SAFETY: Reset acknowledged no future DMA and the allocator owns
            // this exact allocation. No virtual references escape the driver.
            unsafe { ring.release(frames) }.map_err(BlockError::Dma)
        } else {
            Ok(())
        };
        let bounce_result = if let Some(bounce) = self.bounce.take() {
            // SAFETY: Same acknowledged quiescence and allocator ownership.
            unsafe { bounce.release(frames) }.map_err(BlockError::Dma)
        } else {
            Ok(())
        };
        restored?;
        ring_result?;
        bounce_result
    }

    fn read8(&self, offset: u16) -> u8 {
        // SAFETY: PCI discovery validated BAR0 through offset0x1b and this handle
        // exclusively owns that fixed port window; all offsets are constants.
        unsafe { arch::in8(self.transport.io_base + offset) }
    }
    fn write8(&self, offset: u16, value: u8) {
        // SAFETY: Same claimed, validated legacy I/O window and byte field width.
        unsafe { arch::out8(self.transport.io_base + offset, value) };
    }
    fn read32(&self, offset: u16) -> u32 {
        // SAFETY: Same claimed port window; used dword fields are aligned and
        // entirely inside BAR0's validated first28 bytes.
        unsafe { arch::in32(self.transport.io_base + offset) }
    }
    fn write32(&self, offset: u16, value: u32) {
        // SAFETY: Same claimed window and aligned 32-bit writable field width.
        unsafe { arch::out32(self.transport.io_base + offset, value) };
    }
    fn read16(&self, offset: u16) -> u16 {
        // SAFETY: Queue size is a 16-bit field in the claimed port window.
        unsafe { arch::in16(self.transport.io_base + offset) }
    }
    fn write16(&self, offset: u16, value: u16) {
        // SAFETY: Queue select/notify are aligned 16-bit fields in the claimed
        // port window. DMA publication uses explicit barriers before notification.
        unsafe { arch::out16(self.transport.io_base + offset, value) };
    }
}

fn ready_status(status: u8) -> bool {
    status & (ACKNOWLEDGE | DRIVER | DRIVER_OK) == ACKNOWLEDGE | DRIVER | DRIVER_OK
        && status & (FAILED | DEVICE_NEEDS_RESET) == 0
}

#[cfg(feature = "boot-smoke")]
pub fn self_test() {
    for size in [0, 1, 2, 3, 7, 255, 257, 512, u16::MAX] {
        assert!(matches!(
            QueueLayout::new(size),
            Err(BlockError::InvalidQueueSize)
        ));
    }
    for size in [4, 8, 16, 32, 64, 128, 256] {
        let layout = QueueLayout::new(size).expect("supported queue geometry");
        assert_eq!(layout.available, usize::from(size) * 16);
        assert!(layout.used.is_multiple_of(PAGE_SIZE));
        assert!(layout.available + 6 + usize::from(size) * 2 <= layout.used);
        assert!(layout.used + 6 + usize::from(size) * 8 <= layout.pages * PAGE_SIZE);
        assert!((2..=3).contains(&layout.pages));
        assert_eq!(layout.slot(size), 0);
        assert_eq!(layout.slot(u16::MAX), usize::from(size - 1));
        assert_eq!(layout.slot(u16::MAX.wrapping_add(1)), 0);
    }
    assert_eq!(QueueLayout::new(256).unwrap().used, 8192);
    assert_eq!(QueueLayout::new(256).unwrap().pages, 3);
    assert_eq!(QueueLayout::new(128).unwrap().pages, 2);
    let descriptors = read_descriptors(0x2000).expect("aligned bounce");
    assert_eq!(
        descriptors[0],
        Descriptor {
            address: 0x2000,
            length: 16,
            flags: 1,
            next: 1
        }
    );
    assert_eq!(
        descriptors[1],
        Descriptor {
            address: 0x2010,
            length: 512,
            flags: 3,
            next: 2
        }
    );
    assert_eq!(
        descriptors[2],
        Descriptor {
            address: 0x2210,
            length: 1,
            flags: 2,
            next: 0
        }
    );
    for address in [0, 1, 0x2001, 1 << 32, u64::MAX] {
        assert!(matches!(
            read_descriptors(address),
            Err(BlockError::InvalidDmaAddress)
        ));
    }
    assert!(read_descriptors((1 << 32) - PAGE_SIZE as u64).is_ok());
    assert!(valid_sector(0, 2048));
    assert!(valid_sector(2047, 2048));
    assert!(!valid_sector(2048, 2048));
    assert!(!valid_sector(u64::MAX, 2048));
    assert!(!valid_sector(0, 0));
    assert!(!valid_sector(0, u64::MAX));
    for legacy_length in [0, 1, 512, 513, 529, u32::MAX] {
        assert!(validate_completion(9, 10, 0, legacy_length, 0).is_ok());
    }
    assert!(validate_completion(u16::MAX, 0, 0, 513, 0).is_ok());
    assert!(matches!(
        validate_completion(1, 1, 0, 513, 0),
        Err(BlockError::UnexpectedUsedIndex)
    ));
    assert!(matches!(
        validate_completion(1, 3, 0, 513, 0),
        Err(BlockError::UnexpectedUsedIndex)
    ));
    assert!(matches!(
        validate_completion(1, 2, 1, 513, 0),
        Err(BlockError::UnexpectedUsedHead)
    ));
    assert!(matches!(
        validate_completion(1, 2, u32::MAX, 513, 0),
        Err(BlockError::UnexpectedUsedHead)
    ));
    assert!(matches!(
        validate_completion(1, 2, 0, 1, 1),
        Err(BlockError::IoError)
    ));
    assert!(matches!(
        validate_completion(1, 2, 0, 1, 2),
        Err(BlockError::UnsupportedRequest)
    ));
    assert!(matches!(
        validate_completion(1, 2, 0, 513, STATUS_SENTINEL),
        Err(BlockError::InvalidStatus)
    ));
    assert!(ready_status(ACKNOWLEDGE | DRIVER | DRIVER_OK));
    assert!(!ready_status(DRIVER_OK));
    assert!(!ready_status(ACKNOWLEDGE | DRIVER | DRIVER_OK | FAILED));
    assert!(!ready_status(
        ACKNOWLEDGE | DRIVER | DRIVER_OK | DEVICE_NEEDS_RESET
    ));
    serial::println!("MIRROR_BLOCK_TESTS_OK");
}

//! Bootstrap ownership of the four-level x86-64 page-table hierarchy.
//!
//! We copy Limine's tables, preserving every present leaf and its permissions.
//! This keeps the kernel, bootstrap stack, and direct map available across CR3.
//! Private user roots borrow supervisor-only upper mappings and own their lower
//! tables and leaves. Page-table handles and the allocator stay on the BSP;
//! software mutation requires interrupts disabled there. Before AP release,
//! arbitrary kernel mappings are sealed. Prepared heap and 2 MiB scratch windows
//! permit changes to 4 KiB leaves through synchronous TLB shootdowns; their table
//! hierarchy remains pinned. APs retain the kernel root and never access
//! private user mappings or their physical pages. Huge-page splitting and
//! arbitrary shared hierarchy changes remain unsupported.
//! Bootloader-reclaimable memory stays reserved for the stack and
//! boot responses even after the page-table hierarchy has been copied.

use core::{
    arch::{
        asm,
        x86_64::{__cpuid, CpuidResult},
    },
    marker::PhantomData,
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
};

use crate::memory::{FrameAllocator, PAGE_SIZE};

const PRESENT: u64 = 1;
const WRITABLE: u64 = 1 << 1;
const USER: u64 = 1 << 2;
const WRITE_THROUGH: u64 = 1 << 3;
const CACHE_DISABLE: u64 = 1 << 4;
const HUGE: u64 = 1 << 7;
const NO_EXECUTE: u64 = 1 << 63;
const ADDRESS_MASK: u64 = 0x000f_ffff_ffff_f000;
const ENTRIES: usize = 512;
const USER_END: u64 = 1 << 47;
const CR4_PGE: u64 = 1 << 7;
const CR4_LA57: u64 = 1 << 12;
const CR4_PCIDE: u64 = 1 << 17;
const EFER: u32 = 0xc000_0080;
const EFER_NXE: u64 = 1 << 11;

pub const SHARED_BASE: u64 = 0xffff_d200_0000_0000;
pub const SHARED_PAGES: usize = ENTRIES;
const SHARED_END: u64 = SHARED_BASE + SHARED_PAGES as u64 * PAGE_SIZE;
const HEAP_TABLES: usize = crate::heap::HEAP_MAX_SIZE / (ENTRIES * PAGE_SIZE as usize);
const _: () = {
    assert!(HEAP_TABLES > 0);
    assert!(crate::heap::HEAP_MAX_SIZE.is_multiple_of(ENTRIES * PAGE_SIZE as usize));
    assert!(crate::heap::HEAP_START.is_multiple_of(ENTRIES as u64 * PAGE_SIZE));
};

// Released APs require permanently retained kernel table paths. Only the
// prepared window's leaves may change, using the acknowledged IPI protocol.
// Private userspace page tables remain exclusively owned by the BSP.
static KERNEL_SHARED: AtomicBool = AtomicBool::new(false);

#[cfg(feature = "boot-smoke")]
pub const TEST_ADDRESS: u64 = 0xffff_c000_0000_0000;

#[derive(Debug, PartialEq, Eq)]
pub enum PagingError {
    FiveLevelPaging,
    NoExecuteUnsupported,
    InvalidPhysicalAddressWidth,
    InvalidPhysicalAddress,
    NonCanonicalAddress,
    UnalignedAddress,
    DirectMapOverflow,
    OutOfFrames,
    InvalidTable,
    NotActive,
    AlreadyMapped,
    NotMapped,
    HugePageConflict,
    ReadOnlyParent,
    ChangedBootstrapMapping,
    InvalidUserAddress,
    InvalidUserPermissions,
    UserAccessDenied,
    AddressRangeOverflow,
    StillActive,
    RecyclingRequired,
    FrameReleaseFailed,
    UnsupportedCacheLayout,
    KernelMappingsShared,
    SharedWindowAlreadyPrepared,
    SharedWindowNotPrepared,
    SharedWindowReserved,
    InvalidSharedAddress,
    SharedUpdateUnavailable,
    HeapWindowAlreadyPrepared,
    HeapWindowNotPrepared,
    HeapWindowReserved,
    InvalidHeapRange,
}

pub struct PageTables {
    root: u64,
    offset: u64,
    physical_mask: u64,
    owned_tables: usize,
    shared_table: Option<u64>,
    heap_tables: Option<[u64; HEAP_TABLES]>,
    // A page-table handle cannot be sent to another CPU or concurrently shared.
    _boot_cpu_only: PhantomData<*mut ()>,
}

impl PageTables {
    pub fn root_physical_address(&self) -> u64 {
        self.root
    }

    pub fn owned_table_count(&self) -> usize {
        self.owned_tables
    }

    /// Seal arbitrary mappings before publishing this CR3 to secondary CPUs.
    /// The root and all table paths remain permanent; only prepared-window
    /// leaves may change through the synchronous shootdown methods.
    /// This BSP-owned handle and the frame allocator are never shared with APs.
    pub fn share_with_secondaries(&self) -> Result<u64, PagingError> {
        self.check_active()?;
        KERNEL_SHARED.store(true, Ordering::Release);
        Ok(self.root)
    }

    /// Reserve the complete table path for a 2 MiB shared kernel mapping window.
    /// Every leaf starts absent. Existing mappings are never changed; allocation
    /// failures retain any new empty parent tables, as ordinary kernel mapping
    /// does. Successful preparation pins the path for the kernel's lifetime.
    ///
    /// # Safety
    /// Call once on the BSP with IF=0 and this root active, before AP release
    /// and before constructing any userspace root that must retain this window.
    /// The unique allocator and HHDM must satisfy take_ownership's contract.
    /// No other code may access or independently modify this reserved range.
    pub unsafe fn prepare_shared_window(
        &mut self,
        frames: &mut FrameAllocator<'_>,
    ) -> Result<(), PagingError> {
        if self.shared_table.is_some() {
            return Err(PagingError::SharedWindowAlreadyPrepared);
        }
        if KERNEL_SHARED.load(Ordering::Acquire) {
            return Err(PagingError::KernelMappingsShared);
        }
        self.check_active()?;
        self.check_write_back_cache()?;
        // Check the whole range before installing even an empty table. A huge
        // leaf covering it is occupied too and must never be split or replaced.
        for page in 0..SHARED_PAGES {
            if self
                .translate(SHARED_BASE + page as u64 * PAGE_SIZE)?
                .is_some()
            {
                return Err(PagingError::AlreadyMapped);
            }
        }
        self.shared_table = Some(self.prepare_empty_leaf_table(SHARED_BASE, frames)?);
        Ok(())
    }

    fn check_write_back_cache(&self) -> Result<(), PagingError> {
        if cpuid(1).edx & (1 << 16) == 0 {
            return Err(PagingError::UnsupportedCacheLayout);
        }
        let pat_low: u32;
        // SAFETY: CPL0 and CPUID reports PAT. Leaf cache bits select PAT0;
        // require WB before any table change, without changing cache policy.
        unsafe {
            asm!("rdmsr", in("ecx") 0x277_u32, out("eax") pat_low,
                out("edx") _, options(nomem, nostack, preserves_flags));
        }
        if pat_low & 0xff != 6 {
            return Err(PagingError::UnsupportedCacheLayout);
        }
        Ok(())
    }

    // Caller has preflighted an exclusively owned, empty, 2 MiB-aligned range
    // before AP release and creation of any retained userspace root.
    fn prepare_empty_leaf_table(
        &mut self,
        address: u64,
        frames: &mut FrameAllocator<'_>,
    ) -> Result<u64, PagingError> {
        let mut physical = self.root;
        for level in (2..=4).rev() {
            let table = self.table_pointer(physical)?;
            // SAFETY: A validated, exclusively managed table before AP release;
            // The window base is canonical and every index is within its page.
            let slot = unsafe { table.add(index(address, level)) };
            // SAFETY: Aligned initialized entry in the current live table.
            let entry = unsafe { slot.read_volatile() };
            if entry & PRESENT != 0 {
                if entry & HUGE != 0 {
                    return Err(PagingError::HugePageConflict);
                }
                if entry & USER != 0 {
                    return Err(PagingError::UserAccessDenied);
                }
                if entry & WRITABLE == 0 {
                    return Err(PagingError::ReadOnlyParent);
                }
                physical = entry & self.physical_mask;
            } else {
                if entry != 0 {
                    return Err(PagingError::InvalidTable);
                }
                let child = self.allocate_table(frames)?;
                // SAFETY: The zeroed child is fully initialized before becoming
                // reachable. No AP or borrowed userspace root uses this path.
                unsafe { slot.write_volatile(child | PRESENT | WRITABLE) };
                physical = child;
            }
        }
        let table = self.table_pointer(physical)?;
        for leaf in 0..ENTRIES {
            // SAFETY: The validated full PT is initialized; no software shares
            // it yet. Non-present metadata is not ours to overwrite either.
            if unsafe { table.add(leaf).read_volatile() } != 0 {
                return Err(PagingError::InvalidTable);
            }
        }
        Ok(physical)
    }

    /// Install a WB, supervisor-only, writable, NX 4 KiB page in the prepared
    /// window. Success includes local and remote invalidation completion.
    /// Every returned error precedes publication; a failed post-publication
    /// shootdown halts the kernel and never returns ownership to the caller.
    ///
    /// # Safety
    /// Run on the BSP with IF=0 and this kernel root active. Own a full, aligned,
    /// initialized RAM frame with compatible WB aliases. Exclude live references,
    /// DMA and accesses to the window page until completion; IRQ/NMI paths must
    /// not use it or reenter mapping operations while the barrier waits. Retain
    /// the frame until a successful unmap and any other owned aliases are retired.
    /// Hold no lock needed by an IRQ handler while the barrier enables interrupts.
    pub unsafe fn map_shared_page(&self, address: u64, physical: u64) -> Result<(), PagingError> {
        self.check_physical(physical)?;
        let slot = self.shared_leaf(address)?;
        let previous = slot.load(Ordering::Acquire);
        if previous & PRESENT != 0 {
            return Err(PagingError::AlreadyMapped);
        }
        if previous != 0 {
            return Err(PagingError::InvalidTable);
        }
        let update = crate::smp::prepare_kernel_update()
            .map_err(|_| PagingError::SharedUpdateUnavailable)?;
        // This compare-exchange cannot race another permitted software writer.
        // Unlike a split store, it publishes one complete PTE to page walkers.
        if slot
            .compare_exchange(
                0,
                physical | PRESENT | WRITABLE | NO_EXECUTE,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_err()
        {
            return Err(PagingError::AlreadyMapped);
        }
        // SAFETY: The ticket predates this sole PTE change. The page is quiescent
        // and all parents remain pinned. No frame ownership escapes before ACKs.
        unsafe { update.invalidate_page(address) };
        Ok(())
    }

    /// Remove a shared-window leaf and return its frame only after every required
    /// CPU has invalidated the translation. All table frames remain pinned.
    /// Errors leave the mapping unchanged; a post-publication failure never
    /// returns, so a caller cannot recycle a frame with a stale remote mapping.
    ///
    /// # Safety
    /// Run on the BSP with IF=0 and this kernel root active. End every reference,
    /// DMA operation and access to the window page on every CPU before calling.
    /// IRQ/NMI paths must not access it or reenter mapping operations. Other
    /// aliases remain the caller's responsibility before the frame is recycled.
    /// Hold no lock needed by an IRQ handler while the barrier enables interrupts.
    pub unsafe fn unmap_shared_page(&self, address: u64) -> Result<u64, PagingError> {
        let slot = self.shared_leaf(address)?;
        let previous = slot.load(Ordering::Acquire);
        if previous & PRESENT == 0 {
            return Err(PagingError::NotMapped);
        }
        let expected = PRESENT | WRITABLE | NO_EXECUTE;
        let accessed_dirty = (1 << 5) | (1 << 6);
        if previous & !(self.physical_mask | accessed_dirty) != expected {
            return Err(PagingError::InvalidTable);
        }
        let update = crate::smp::prepare_kernel_update()
            .map_err(|_| PagingError::SharedUpdateUnavailable)?;
        // Atomic exchange tolerates hardware setting A/D after the preflight
        // load without losing or tearing the frame identity being withdrawn.
        let removed = slot.swap(0, Ordering::AcqRel);
        // SAFETY: The ticket precedes removal. No caller can obtain or free the
        // withdrawn frame until all required invalidations have completed.
        unsafe { update.invalidate_page(address) };
        Ok(removed & self.physical_mask)
    }

    fn shared_leaf(&self, address: u64) -> Result<&AtomicU64, PagingError> {
        check_page_address(address)?;
        if !(SHARED_BASE..SHARED_END).contains(&address) {
            return Err(PagingError::InvalidSharedAddress);
        }
        self.check_active()?;
        let physical = self
            .shared_table
            .ok_or(PagingError::SharedWindowNotPrepared)?;
        let table = self.table_pointer(physical)?;
        // SAFETY: Preparation retains the full page-aligned PT permanently;
        // each u64 entry satisfies AtomicU64's alignment. Only this BSP writes
        // these leaves in software and uses atomic operations while published.
        // Hardware may atomically set accessed/dirty bits. No reference to an
        // ordinary u64 is created, and the resulting reference stays bounded by
        // this retained page-table handle's lifetime.
        Ok(unsafe { AtomicU64::from_ptr(table.add(index(address, 1))) })
    }

    /// Pin the heap's complete reserved hierarchy while every leaf is absent.
    /// The adjacent permanent guard pages remain absent and reserved. Empty
    /// parent tables allocated on failure remain owned by this hierarchy.
    ///
    /// # Safety
    /// Run once on the BSP with IF=0 before AP release, heap initialization,
    /// and creation of retained user roots. Own the allocator and entire heap
    /// reservation, including both guards; preserve the table paths forever.
    pub unsafe fn prepare_heap_window(
        &mut self,
        frames: &mut FrameAllocator<'_>,
    ) -> Result<(), PagingError> {
        if self.heap_tables.is_some() {
            return Err(PagingError::HeapWindowAlreadyPrepared);
        }
        if KERNEL_SHARED.load(Ordering::Acquire) {
            return Err(PagingError::KernelMappingsShared);
        }
        self.check_active()?;
        self.check_write_back_cache()?;
        let pages = crate::heap::HEAP_MAX_SIZE / PAGE_SIZE as usize;
        for page in 0..pages + 2 {
            let address = crate::heap::HEAP_START - PAGE_SIZE + page as u64 * PAGE_SIZE;
            if self.translate(address)?.is_some() {
                return Err(PagingError::AlreadyMapped);
            }
        }
        let mut tables = [0; HEAP_TABLES];
        for (index, table) in tables.iter_mut().enumerate() {
            let address = crate::heap::HEAP_START + (index * ENTRIES) as u64 * PAGE_SIZE;
            *table = self.prepare_empty_leaf_table(address, frames)?;
        }
        self.heap_tables = Some(tables);
        Ok(())
    }

    /// Publish a contiguous virtual run of owned heap frames with one remote
    /// invalidation command per CPU. Every recoverable error precedes all PTE
    /// writes. After publication any failure halts before the caller can reuse
    /// frames or expose the new capacity to the allocator.
    ///
    /// # Safety
    /// Run on the BSP with IF=0 and this root active. Exclusively own distinct,
    /// initialized WB RAM frames and the absent heap tail. No access, reference
    /// or DMA may use the tail until completion. Existing heap allocations may
    /// remain live. Do not hold the allocator lock or any IRQ-needed lock while
    /// waiting. Keep successful mappings and frames for the allocator's lifetime.
    pub unsafe fn map_heap_pages(&self, address: u64, frames: &[u64]) -> Result<(), PagingError> {
        self.check_heap_range(address, frames.len())?;
        for (page, &physical) in frames.iter().enumerate() {
            self.check_physical(physical)?;
            let slot = self.heap_leaf(address + page as u64 * PAGE_SIZE)?;
            let previous = slot.load(Ordering::Acquire);
            if previous & PRESENT != 0 {
                return Err(PagingError::AlreadyMapped);
            }
            if previous != 0 {
                return Err(PagingError::InvalidTable);
            }
        }
        let update = crate::smp::prepare_kernel_update()
            .map_err(|_| PagingError::SharedUpdateUnavailable)?;
        for (page, &physical) in frames.iter().enumerate() {
            let slot = self
                .heap_leaf(address + page as u64 * PAGE_SIZE)
                .expect("preflighted heap leaf");
            // The exclusive BSP writer preflighted every absent leaf. Atomic
            // stores publish complete entries; no fallible path follows them.
            slot.store(
                physical | PRESENT | WRITABLE | NO_EXECUTE,
                Ordering::Release,
            );
        }
        // SAFETY: The ticket and full preflight precede publication. The tail
        // stays inaccessible to clients until all CPUs complete invalidation.
        unsafe { update.invalidate_range(address, frames.len()) };
        Ok(())
    }

    /// Withdraw a smoke-test heap tail that has not been given to the allocator.
    /// The supplied frame identities must match every leaf. Return only after
    /// all CPUs acknowledge invalidation; then the caller can recycle the run.
    ///
    /// # Safety
    /// As for map_heap_pages, with all accesses finished on every CPU. Never
    /// remove committed allocator capacity, including currently free blocks.
    #[cfg(feature = "boot-smoke")]
    pub unsafe fn unmap_heap_pages(&self, address: u64, frames: &[u64]) -> Result<(), PagingError> {
        self.check_heap_range(address, frames.len())?;
        let accessed_dirty = (1 << 5) | (1 << 6);
        for (page, &physical) in frames.iter().enumerate() {
            self.check_physical(physical)?;
            let entry = self
                .heap_leaf(address + page as u64 * PAGE_SIZE)?
                .load(Ordering::Acquire);
            if entry & PRESENT == 0 {
                return Err(PagingError::NotMapped);
            }
            if entry & !accessed_dirty != physical | PRESENT | WRITABLE | NO_EXECUTE {
                return Err(PagingError::InvalidTable);
            }
        }
        let update = crate::smp::prepare_kernel_update()
            .map_err(|_| PagingError::SharedUpdateUnavailable)?;
        for page in 0..frames.len() {
            self.heap_leaf(address + page as u64 * PAGE_SIZE)
                .expect("preflighted heap removal")
                .swap(0, Ordering::AcqRel);
        }
        // SAFETY: Every alias is quiescent and the matching frames stay owned
        // until the exact acknowledgement barrier completes for the whole run.
        unsafe { update.invalidate_range(address, frames.len()) };
        Ok(())
    }

    fn check_heap_range(&self, address: u64, pages: usize) -> Result<(), PagingError> {
        check_page_address(address)?;
        if !(1..=crate::heap::MAX_GROW_PAGES).contains(&pages)
            || address < crate::heap::HEAP_START
            || address
                .checked_add(pages as u64 * PAGE_SIZE)
                .is_none_or(|end| end > crate::heap::HEAP_START + crate::heap::HEAP_MAX_SIZE as u64)
        {
            return Err(PagingError::InvalidHeapRange);
        }
        self.check_active()?;
        if self.heap_tables.is_none() {
            return Err(PagingError::HeapWindowNotPrepared);
        }
        Ok(())
    }

    // Range validation precedes calls; pinned PTs also keep these references
    // valid when the BSP temporarily enables interrupts during a shootdown.
    fn heap_leaf(&self, address: u64) -> Result<&AtomicU64, PagingError> {
        let tables = self
            .heap_tables
            .as_ref()
            .ok_or(PagingError::HeapWindowNotPrepared)?;
        let page = (address - crate::heap::HEAP_START) as usize / PAGE_SIZE as usize;
        let physical = *tables
            .get(page / ENTRIES)
            .ok_or(PagingError::InvalidHeapRange)?;
        let table = self.table_pointer(physical)?;
        // SAFETY: Retained full PT, aligned atomic entry, no ordinary reference
        // aliases it. Software writes are exclusive to this BSP handle.
        Ok(unsafe { AtomicU64::from_ptr(table.add(page % ENTRIES)) })
    }

    fn heap_reserved(&self, address: u64) -> bool {
        self.heap_tables.is_some()
            && (crate::heap::HEAP_START - PAGE_SIZE
                ..crate::heap::HEAP_START + crate::heap::HEAP_MAX_SIZE as u64 + PAGE_SIZE)
                .contains(&address)
    }

    /// Restore the kernel root after executing an isolated user address space.
    ///
    /// # Safety
    /// Run at CPL0 on the BSP with interrupts disabled. The current code, stack,
    /// and all live kernel data must belong to the shared upper half. APs may
    /// retain the kernel root with pinned table paths; this changes only the
    /// BSP's translations.
    pub unsafe fn activate(&self) -> Result<(), PagingError> {
        self.check_physical(self.root)?;
        // SAFETY: The caller preserves the common supervisor mappings that
        // cover this code and stack across the root replacement.
        unsafe { switch_root(self.root) };
        self.check_active()
    }

    /// Resolve a byte address, including offsets within 4 KiB, 2 MiB, or 1 GiB
    /// leaves. This reports the mapped physical address, not access permissions.
    pub fn translate(&self, address: u64) -> Result<Option<u64>, PagingError> {
        check_canonical(address)?;
        let mut physical = self.root;
        for level in (1..=4).rev() {
            let table = self.table_pointer(physical)?;
            // SAFETY: Construction and mutation maintain valid, direct-mapped
            // table frames. The index is always in the 512-entry table.
            let entry = unsafe { table.add(index(address, level)).read_volatile() };
            if entry & PRESENT == 0 {
                return Ok(None);
            }
            if level == 1 || (level < 4 && entry & HUGE != 0) {
                let offset_mask = (1_u64 << (12 + 9 * (level - 1))) - 1;
                return Ok(Some(
                    (entry & self.physical_mask & !offset_mask) | (address & offset_mask),
                ));
            }
            if entry & HUGE != 0 {
                return Err(PagingError::InvalidTable);
            }
            physical = entry & self.physical_mask;
        }
        Err(PagingError::InvalidTable)
    }

    /// Add a supervisor-only, writable, non-executable 4 KiB mapping.
    /// Existing mappings and huge leaves are never replaced. Allocated table
    /// frames remain owned, including empty tables retained after an error.
    ///
    /// # Safety
    /// Run only on the boot CPU with interrupts disabled and this root active.
    /// Complete kernel mappings before publishing this root to APs; insertion
    /// is rejected once share_with_secondaries has sealed the hierarchy.
    /// The prepared shared window accepts only map_shared_page updates.
    /// `physical` must be a full, valid RAM page the caller may map writable.
    /// The caller must account for aliases and Rust reference lifetimes. The
    /// allocator must remain the unique allocator used to create these tables.
    pub unsafe fn map_page(
        &mut self,
        address: u64,
        physical: u64,
        frames: &mut FrameAllocator<'_>,
    ) -> Result<(), PagingError> {
        // SAFETY: The caller owns this RAM page and its aliases.
        unsafe { self.map_kernel_page(address, physical, WRITABLE, frames) }
    }

    /// Temporarily map reserved firmware RAM read-only, supervisor-only and NX.
    ///
    /// # Safety
    /// The caller verifies the whole physical page belongs to firmware RAM,
    /// with no incompatible cache aliases, and excludes mutation/interrupts on
    /// the BSP. This root is active and the allocator is exclusively owned.
    /// Temporary aliases must be removed before sharing this root with APs.
    pub unsafe fn map_firmware_page(
        &mut self,
        address: u64,
        physical: u64,
        frames: &mut FrameAllocator<'_>,
    ) -> Result<(), PagingError> {
        // SAFETY: The caller supplies reserved RAM; no writable alias is added.
        unsafe { self.map_kernel_page(address, physical, 0, frames) }
    }

    /// Map one device page supervisor-only, writable, NX and strongly uncached.
    /// Uses Limine's PAT3=UC (PCD|PWT), without changing global cache policy.
    ///
    /// # Safety
    /// As for map_page, but physical is an exclusively owned MMIO page, never
    /// allocator RAM. The caller must exclude every conflicting cached alias.
    /// Create shared kernel mappings before creating private userspace roots
    /// or sealing the root for secondary CPUs.
    pub unsafe fn map_device_page(
        &mut self,
        address: u64,
        physical: u64,
        frames: &mut FrameAllocator<'_>,
    ) -> Result<(), PagingError> {
        if cpuid(1).edx & (1 << 16) == 0 {
            return Err(PagingError::UnsupportedCacheLayout);
        }
        let low: u32;
        // SAFETY: CPUID reports PAT. At CPL0 IA32_PAT is readable and Limine
        // initialized it. Verify entry 3 rather than assuming a reset value.
        unsafe {
            asm!("rdmsr", in("ecx") 0x277_u32, out("eax") low,
                out("edx") _, options(nomem, nostack, preserves_flags));
        }
        if low >> 24 != 0 {
            return Err(PagingError::UnsupportedCacheLayout);
        }
        // SAFETY: Ownership and alias constraints are inherited from the caller.
        unsafe {
            self.map_kernel_page(
                address,
                physical,
                WRITABLE | WRITE_THROUGH | CACHE_DISABLE,
                frames,
            )
        }
    }

    unsafe fn map_kernel_page(
        &mut self,
        address: u64,
        physical: u64,
        attributes: u64,
        frames: &mut FrameAllocator<'_>,
    ) -> Result<(), PagingError> {
        if KERNEL_SHARED.load(Ordering::Acquire) {
            return Err(PagingError::KernelMappingsShared);
        }
        if self.shared_table.is_some() && (SHARED_BASE..SHARED_END).contains(&address) {
            return Err(PagingError::SharedWindowReserved);
        }
        if self.heap_reserved(address) {
            return Err(PagingError::HeapWindowReserved);
        }
        check_page_address(address)?;
        self.check_physical(physical)?;
        self.check_active()?;
        let mut table_physical = self.root;
        for level in (2..=4).rev() {
            let table = self.table_pointer(table_physical)?;
            // SAFETY: The table belongs to this hierarchy and the index is in
            // range. No concurrent software modifies it on this boot CPU.
            let slot = unsafe { table.add(index(address, level)) };
            // SAFETY: `slot` points to an aligned entry in that live table.
            let entry = unsafe { slot.read_volatile() };
            if entry & PRESENT != 0 {
                if entry & HUGE != 0 {
                    return Err(if level == 4 {
                        PagingError::InvalidTable
                    } else {
                        PagingError::HugePageConflict
                    });
                }
                if entry & WRITABLE == 0 {
                    return Err(PagingError::ReadOnlyParent);
                }
                table_physical = entry & self.physical_mask;
            } else {
                let child = self.allocate_table(frames)?;
                // SAFETY: The new child is zeroed before it becomes reachable.
                // The caller has excluded concurrent software page-table access.
                unsafe { slot.write_volatile(child | PRESENT | WRITABLE) };
                table_physical = child;
            }
        }
        let table = self.table_pointer(table_physical)?;
        // SAFETY: This is the in-range PTE in a live, exclusively managed table.
        let slot = unsafe { table.add(index(address, 1)) };
        // SAFETY: The PTE is aligned, initialized, and mapped through the HHDM.
        if unsafe { slot.read_volatile() } & PRESENT != 0 {
            return Err(PagingError::AlreadyMapped);
        }
        // SAFETY: The caller owns the mapped memory and has excluded other
        // software writers. NXE is enabled before PageTables is returned.
        unsafe { slot.write_volatile(physical | PRESENT | attributes | NO_EXECUTE) };
        invalidate_page(address);
        Ok(())
    }

    /// Remove a 4 KiB leaf and return its physical frame without freeing it.
    /// Empty parent tables remain owned and available for later mappings.
    ///
    /// # Safety
    /// Run only on the boot CPU with interrupts disabled and this root active.
    /// Remove mappings before publishing this root to APs; removal is rejected
    /// once share_with_secondaries has sealed the hierarchy.
    /// Prepared windows and heap guards reject this general mapping API.
    /// No live reference or subsequent access may depend on the removed alias.
    /// In particular, keep the executing code, stack, and HHDM of all table
    /// frames mapped. The caller must handle any remaining physical aliases.
    pub unsafe fn unmap_page(&mut self, address: u64) -> Result<u64, PagingError> {
        if KERNEL_SHARED.load(Ordering::Acquire) {
            return Err(PagingError::KernelMappingsShared);
        }
        if self.shared_table.is_some() && (SHARED_BASE..SHARED_END).contains(&address) {
            return Err(PagingError::SharedWindowReserved);
        }
        if self.heap_reserved(address) {
            return Err(PagingError::HeapWindowReserved);
        }
        check_page_address(address)?;
        self.check_active()?;
        let mut physical = self.root;
        for level in (1..=4).rev() {
            let table = self.table_pointer(physical)?;
            // SAFETY: Valid table frame and in-range index; no concurrent writer.
            let slot = unsafe { table.add(index(address, level)) };
            // SAFETY: The slot is an aligned, initialized page-table entry.
            let entry = unsafe { slot.read_volatile() };
            if entry & PRESENT == 0 {
                return Err(PagingError::NotMapped);
            }
            if level == 1 {
                // SAFETY: The caller guarantees no live use of this mapping.
                unsafe { slot.write_volatile(0) };
                invalidate_page(address);
                return Ok(entry & self.physical_mask);
            }
            if entry & HUGE != 0 {
                return Err(if level == 4 {
                    PagingError::InvalidTable
                } else {
                    PagingError::HugePageConflict
                });
            }
            physical = entry & self.physical_mask;
        }
        Err(PagingError::InvalidTable)
    }

    fn check_active(&self) -> Result<(), PagingError> {
        if read_cr3() & ADDRESS_MASK == self.root {
            Ok(())
        } else {
            Err(PagingError::NotActive)
        }
    }

    fn check_physical(&self, physical: u64) -> Result<(), PagingError> {
        if physical & (PAGE_SIZE - 1) != 0 {
            return Err(PagingError::UnalignedAddress);
        }
        if physical & !self.physical_mask != 0 {
            return Err(PagingError::InvalidPhysicalAddress);
        }
        Ok(())
    }

    fn table_pointer(&self, physical: u64) -> Result<*mut u64, PagingError> {
        self.check_physical(physical)?;
        let address = self
            .offset
            .checked_add(physical)
            .ok_or(PagingError::DirectMapOverflow)?;
        check_page_address(address)?;
        Ok(address as *mut u64)
    }

    fn allocate_table(&mut self, frames: &mut FrameAllocator<'_>) -> Result<u64, PagingError> {
        let physical = frames.allocate().ok_or(PagingError::OutOfFrames)?;
        let pointer = self.table_pointer(physical)?;
        // SAFETY: The unique allocator returned a fresh, full USABLE frame.
        // The constructor's HHDM contract covers the entire writable page.
        unsafe { pointer.write_bytes(0, ENTRIES) };
        self.owned_tables += 1;
        Ok(physical)
    }

    fn clone_table(
        &mut self,
        source_physical: u64,
        level: u32,
        frames: &mut FrameAllocator<'_>,
    ) -> Result<u64, PagingError> {
        let source = self.table_pointer(source_physical)?;
        let physical = self.allocate_table(frames)?;
        let destination = self.table_pointer(physical)?;
        for entry_index in 0..ENTRIES {
            // SAFETY: Both source and destination are full, direct-mapped table
            // frames and each index is within the 512 entries.
            let mut entry = unsafe { source.add(entry_index).read_volatile() };
            if entry & PRESENT != 0 && level > 1 {
                if entry & HUGE != 0 {
                    if level == 4 {
                        return Err(PagingError::InvalidTable);
                    }
                    // At levels 3/2, PS identifies a 1 GiB/2 MiB leaf. Its
                    // address, PAT, permissions, and cache attributes survive.
                } else {
                    let child = self.clone_table(entry & self.physical_mask, level - 1, frames)?;
                    entry = (entry & !ADDRESS_MASK) | child;
                }
            }
            // At level 1, bit 7 is PAT, not a child-table or huge-page marker.
            // SAFETY: This entry is in the newly allocated, unreachable table.
            unsafe { destination.add(entry_index).write_volatile(entry) };
        }
        Ok(physical)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UserPermissions {
    pub writable: bool,
    pub executable: bool,
}

impl UserPermissions {
    pub const READ_ONLY: Self = Self {
        writable: false,
        executable: false,
    };
    pub const READ_WRITE: Self = Self {
        writable: true,
        executable: false,
    };
    pub const READ_EXECUTE: Self = Self {
        writable: false,
        executable: true,
    };
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UserTranslation {
    pub physical: u64,
    pub writable: bool,
    pub executable: bool,
}

/// An isolated lower half with a borrowed kernel table hierarchy above it.
/// There is deliberately no Drop: teardown requires the unique frame allocator
/// and proof that this root is inactive. Call destroy explicitly after restore.
/// Only the BSP may activate or modify it. APs keep the kernel root and must
/// never access this lower hierarchy or its owned physical pages. Borrowed
/// table paths remain pinned; prepared shared leaves can change through the
/// acknowledged shootdown protocol while this private root is inactive.
pub struct UserAddressSpace<'kernel> {
    kernel: &'kernel PageTables,
    root: u64,
    owned_tables: usize,
    owned_pages: usize,
}

struct OwnedPath {
    frames: [u64; 4],
    entries: [u64; 4],
    slots: [*mut u64; 4],
    depth: usize,
}

impl<'kernel> UserAddressSpace<'kernel> {
    /// Allocate an empty lower half and borrow the kernel's upper hierarchy.
    /// The upper PML4 entries always have U/S clear, irrespective of permissions
    /// below them. Bootloader lower mappings are never copied into this root.
    ///
    /// # Safety
    /// Run at CPL0 on the BSP with interrupts disabled and `kernel` active.
    /// Preserve its HHDM and exclusively own its allocator, with recycling
    /// enabled. APs may use only the retained kernel hierarchy.
    /// Executing code, stack, and all kernel data used while this root is active
    /// must be in the upper half. The prepared shared window, if used, must
    /// already exist. Borrowed table paths remain fixed; only prepared leaves
    /// may change, through the shared update methods with this root inactive.
    pub unsafe fn new(
        kernel: &'kernel PageTables,
        frames: &mut FrameAllocator<'_>,
    ) -> Result<Self, PagingError> {
        kernel.check_active()?;
        if frames.stats().is_none() {
            return Err(PagingError::RecyclingRequired);
        }
        let source = kernel.table_pointer(kernel.root)?;
        let mut space = Self {
            kernel,
            root: 0,
            owned_tables: 0,
            owned_pages: 0,
        };
        let root = space.allocate_table(frames)?;
        let destination = kernel.table_pointer(root)?;
        for entry_index in ENTRIES / 2..ENTRIES {
            // SAFETY: Both tables are full direct-mapped pages. The source
            // root's upper entries stay fixed; the new root is still unreachable.
            unsafe {
                let entry = source.add(entry_index).read_volatile();
                destination.add(entry_index).write_volatile(entry & !USER);
            }
        }
        space.root = root;
        Ok(space)
    }

    pub fn root_physical_address(&self) -> u64 {
        self.root
    }

    pub fn owned_table_count(&self) -> usize {
        self.owned_tables
    }

    pub fn owned_page_count(&self) -> usize {
        self.owned_pages
    }

    /// Transfer one initialized frame into an inactive user's lower half.
    /// On error, ownership of `physical` stays with the caller. Any intermediate
    /// tables allocated before an error stay reachable and owned until destroy.
    ///
    /// # Safety
    /// Run on the BSP with interrupts disabled, exclusively using the same
    /// allocator as new. No other CPU may access this private lower hierarchy
    /// or the supplied page. The caller must own a full, initialized USABLE frame
    /// and end all references and aliases other than the dormant HHDM alias.
    /// Never pass a frame already mapped here, a kernel frame, or a table frame.
    pub unsafe fn map_page(
        &mut self,
        address: u64,
        physical: u64,
        permissions: UserPermissions,
        frames: &mut FrameAllocator<'_>,
    ) -> Result<(), PagingError> {
        check_user_address(address)?;
        check_page_address(address)?;
        self.kernel.check_physical(physical)?;
        self.check_inactive()?;
        if permissions.writable && permissions.executable {
            return Err(PagingError::InvalidUserPermissions);
        }
        let mut table_physical = self.root;
        for level in (2..=4).rev() {
            let table = self.kernel.table_pointer(table_physical)?;
            // SAFETY: Each table belongs to this root and the index is in range.
            let slot = unsafe { table.add(index(address, level)) };
            // SAFETY: This is an initialized, aligned table entry.
            let entry = unsafe { slot.read_volatile() };
            if entry & PRESENT == 0 {
                let child = self.allocate_table(frames)?;
                // SAFETY: The child is zeroed and owned before publication.
                unsafe { slot.write_volatile(child | PRESENT | WRITABLE | USER) };
                table_physical = child;
            } else {
                if entry & HUGE != 0 {
                    return Err(PagingError::InvalidTable);
                }
                if entry & USER == 0 {
                    return Err(PagingError::UserAccessDenied);
                }
                if (permissions.writable && entry & WRITABLE == 0)
                    || (permissions.executable && entry & NO_EXECUTE != 0)
                {
                    return Err(PagingError::InvalidUserPermissions);
                }
                table_physical = entry & self.kernel.physical_mask;
            }
        }
        let table = self.kernel.table_pointer(table_physical)?;
        // SAFETY: This is the in-range leaf slot in our inactive hierarchy.
        let slot = unsafe { table.add(index(address, 1)) };
        // SAFETY: The table was initialized and is exclusively owned.
        if unsafe { slot.read_volatile() } & PRESENT != 0 {
            return Err(PagingError::AlreadyMapped);
        }
        let mut flags = PRESENT | USER;
        if permissions.writable {
            flags |= WRITABLE;
        }
        if !permissions.executable {
            flags |= NO_EXECUTE;
        }
        // SAFETY: The caller transfers its exclusively owned initialized page.
        // User leaves are never global; activate flushes the reused PCID.
        unsafe { slot.write_volatile(physical | flags) };
        self.owned_pages += 1;
        Ok(())
    }

    /// Find the first owned present lower-half leaf in an aligned half-open
    /// range, including inaccessible leaves retained by protect_page(None).
    /// Missing entries skip their entire subtree; a sparse reservation does
    /// not require visiting every absent 4 KiB page. No mapping is modified.
    pub fn next_owned_page(&self, start: u64, end: u64) -> Result<Option<u64>, PagingError> {
        check_user_address(start)?;
        check_page_address(start)?;
        if end > USER_END {
            return Err(PagingError::InvalidUserAddress);
        }
        if end & (PAGE_SIZE - 1) != 0 {
            return Err(PagingError::UnalignedAddress);
        }
        if end < start {
            return Err(PagingError::AddressRangeOverflow);
        }
        self.check_live()?;
        let mut address = start;
        while address < end {
            let mut physical = self.root;
            for level in (1..=4).rev() {
                let table = self.kernel.table_pointer(physical)?;
                // SAFETY: The live owned hierarchy provides a full initialized
                // table, and each index is within its 512 aligned entries.
                let entry = unsafe { table.add(index(address, level)).read_volatile() };
                if entry & PRESENT == 0 {
                    let span = 1_u64 << (12 + 9 * (level - 1));
                    // Lower-half addresses and level<=4 keep this boundary
                    // within USER_END, including the final PML4 subtree.
                    address = ((address & !(span - 1)) + span).min(end);
                    break;
                }
                if level == 1 {
                    // A supervisor-only leaf is still owned physical memory.
                    self.kernel
                        .table_pointer(entry & self.kernel.physical_mask)?;
                    return Ok(Some(address));
                }
                if entry & HUGE != 0 {
                    return Err(PagingError::InvalidTable);
                }
                if entry & USER == 0 {
                    return Err(PagingError::UserAccessDenied);
                }
                physical = entry & self.kernel.physical_mask;
            }
        }
        Ok(None)
    }

    /// Remove an owned leaf and reclaim each empty lower table on its path.
    /// A missing page returns false and prunes any empty tables left by a
    /// failed map_page. The user root and borrowed upper mappings are never
    /// freed. Supervisor-only leaves
    /// installed by protect_page(None) remain owned and can be removed here.
    /// Failed releases restore that entry; earlier successful releases remain
    /// reflected in the tree and ownership counts, so teardown can be retried.
    ///
    /// # Safety
    /// Run on the BSP with interrupts disabled and this private root inactive,
    /// using the original unique allocator. No AP may have used the user root
    /// or its owned pages. Restore the kernel root on the BSP before
    /// calling to flush stale user translations. No live reference or DMA may
    /// access this page or any table that becomes empty after its removal.
    pub unsafe fn unmap_page(
        &mut self,
        address: u64,
        frames: &mut FrameAllocator<'_>,
    ) -> Result<bool, PagingError> {
        check_user_address(address)?;
        check_page_address(address)?;
        self.check_inactive()?;
        let path = self.owned_path(address)?;
        let present = path.depth == 4 && path.entries[3] & PRESENT != 0;

        // Validate the complete path and determine which lower tables become
        // empty before withdrawing anything. Each table's child on this path
        // is ignored because that child is about to be removed.
        let mut first_reclaimed_table = path.depth;
        for depth in (1..path.depth).rev() {
            let table = self.kernel.table_pointer(path.frames[depth])?;
            let removed_index = index(address, 4 - depth as u32);
            let has_neighbor = (0..ENTRIES).any(|entry_index| {
                // SAFETY: This is a full, initialized, exclusively owned table.
                entry_index != removed_index
                    && unsafe { table.add(entry_index).read_volatile() } & PRESENT != 0
            });
            if has_neighbor {
                break;
            }
            first_reclaimed_table = depth;
        }

        if present {
            // SAFETY: The entire inactive path is validated. Withdraw its owned
            // leaf before releasing the frame; no stale translation remains live.
            unsafe { path.slots[3].write_volatile(0) };
            // SAFETY: This exclusively owned leaf has no remaining active alias.
            if unsafe { frames.deallocate(path.entries[3] & self.kernel.physical_mask) }.is_err() {
                // SAFETY: A rejected free retains the allocation and ownership.
                unsafe { path.slots[3].write_volatile(path.entries[3]) };
                return Err(PagingError::FrameReleaseFailed);
            }
            self.owned_pages -= 1;
        }
        for depth in (first_reclaimed_table..path.depth).rev() {
            // SAFETY: This child table is empty after successful descendant
            // removal, and the parent remains live until its own later turn.
            unsafe { path.slots[depth - 1].write_volatile(0) };
            // SAFETY: The table is inactive, empty and no longer reachable.
            if unsafe { frames.deallocate(path.frames[depth]) }.is_err() {
                // SAFETY: Preserve ownership of the still-allocated table.
                unsafe { path.slots[depth - 1].write_volatile(path.entries[depth - 1]) };
                return Err(PagingError::FrameReleaseFailed);
            }
            self.owned_tables -= 1;
        }
        Ok(present)
    }

    /// Change permissions without replacing or clearing an owned data frame.
    /// None keeps an owned present supervisor-only, non-executable leaf, so
    /// restoring user access preserves its contents. Physical address, access
    /// and dirty bits, and unrelated leaf attributes survive every transition.
    ///
    /// # Safety
    /// Run on the BSP with interrupts disabled and this private root inactive.
    /// No AP may use its lower hierarchy or owned pages. Restore the kernel
    /// root on the BSP before calling to flush stale user permissions.
    /// No live reference or DMA may depend on the old access permissions.
    pub unsafe fn protect_page(
        &mut self,
        address: u64,
        permissions: Option<UserPermissions>,
    ) -> Result<(), PagingError> {
        check_user_address(address)?;
        check_page_address(address)?;
        self.check_inactive()?;
        if permissions.is_some_and(|permissions| permissions.writable && permissions.executable) {
            return Err(PagingError::InvalidUserPermissions);
        }
        let path = self.owned_path(address)?;
        if path.depth != 4 || path.entries[3] & PRESENT == 0 {
            return Err(PagingError::NotMapped);
        }
        if let Some(permissions) = permissions {
            for &entry in &path.entries[..3] {
                if (permissions.writable && entry & WRITABLE == 0)
                    || (permissions.executable && entry & NO_EXECUTE != 0)
                {
                    return Err(PagingError::InvalidUserPermissions);
                }
            }
        }
        let mut entry = path.entries[3] & !(USER | WRITABLE | NO_EXECUTE);
        match permissions {
            Some(permissions) => {
                entry |= USER;
                if permissions.writable {
                    entry |= WRITABLE;
                }
                if !permissions.executable {
                    entry |= NO_EXECUTE;
                }
            }
            None => entry |= NO_EXECUTE,
        }
        // SAFETY: The full path belongs to the inactive root; only the desired
        // leaf permission bits change. Activation will flush prior permissions.
        unsafe { path.slots[3].write_volatile(entry) };
        Ok(())
    }

    /// Walk an owned lower-half path without requiring user access on its leaf.
    /// Parents always retain USER, including above inaccessible owned pages.
    fn owned_path(&self, address: u64) -> Result<OwnedPath, PagingError> {
        let mut path = OwnedPath {
            frames: [0; 4],
            entries: [0; 4],
            slots: [core::ptr::null_mut(); 4],
            depth: 0,
        };
        let mut physical = self.root;
        for depth in 0..4 {
            let level = 4 - depth as u32;
            let table = self.kernel.table_pointer(physical)?;
            // SAFETY: The live owned table has exactly ENTRIES entries.
            let slot = unsafe { table.add(index(address, level)) };
            // SAFETY: The slot is initialized and no concurrent writer exists.
            let entry = unsafe { slot.read_volatile() };
            path.frames[depth] = physical;
            path.entries[depth] = entry;
            path.slots[depth] = slot;
            path.depth = depth + 1;
            if entry & PRESENT == 0 {
                return Ok(path);
            }
            if level > 1 {
                if entry & HUGE != 0 {
                    return Err(PagingError::InvalidTable);
                }
                if entry & USER == 0 {
                    return Err(PagingError::UserAccessDenied);
                }
            }
            physical = entry & self.kernel.physical_mask;
        }
        // Preflight the leaf's HHDM representation too, before any mutation.
        self.kernel.table_pointer(physical)?;
        Ok(path)
    }

    /// Resolve a user byte and calculate permissions across every table level.
    /// A present supervisor entry fails even if the final PTE has U/S set.
    pub fn translate(&self, address: u64) -> Result<Option<UserTranslation>, PagingError> {
        check_user_address(address)?;
        self.check_live()?;
        let mut table_physical = self.root;
        let mut writable = true;
        let mut executable = true;
        for level in (1..=4).rev() {
            let table = self.kernel.table_pointer(table_physical)?;
            // SAFETY: Owned, live, initialized table and an in-range index.
            let entry = unsafe { table.add(index(address, level)).read_volatile() };
            if entry & PRESENT == 0 {
                return Ok(None);
            }
            if entry & USER == 0 {
                return Err(PagingError::UserAccessDenied);
            }
            writable &= entry & WRITABLE != 0;
            executable &= entry & NO_EXECUTE == 0;
            if level == 1 {
                return Ok(Some(UserTranslation {
                    physical: (entry & self.kernel.physical_mask) | (address & (PAGE_SIZE - 1)),
                    writable,
                    executable,
                }));
            }
            if entry & HUGE != 0 {
                return Err(PagingError::InvalidTable);
            }
            table_physical = entry & self.kernel.physical_mask;
        }
        Err(PagingError::InvalidTable)
    }

    /// Validate the whole range before producing any syscall side effects.
    /// Empty ranges still require a valid lower-half, non-null-page address.
    pub fn validate_readable(&self, address: u64, length: usize) -> Result<(), PagingError> {
        self.validate_access(address, length, false)
    }

    /// Validate that every byte is user writable, accumulating U/S and R/W
    /// permissions at every level. No destination bytes are changed on failure.
    /// Empty ranges still require a valid lower-half, non-null-page address.
    pub fn validate_writable(&self, address: u64, length: usize) -> Result<(), PagingError> {
        self.validate_access(address, length, true)
    }

    fn validate_access(
        &self,
        address: u64,
        length: usize,
        writable: bool,
    ) -> Result<(), PagingError> {
        check_user_address(address)?;
        self.check_live()?;
        if length == 0 {
            return Ok(());
        }
        let last = address
            .checked_add(length as u64 - 1)
            .ok_or(PagingError::AddressRangeOverflow)?;
        check_user_address(last)?;
        let final_page = last & !(PAGE_SIZE - 1);
        let mut current = address & !(PAGE_SIZE - 1);
        loop {
            let translation = self.translate(current)?.ok_or(PagingError::NotMapped)?;
            if writable && !translation.writable {
                return Err(PagingError::UserAccessDenied);
            }
            // Preflight the whole HHDM page as well as its user permissions.
            // A later copy cannot encounter a direct-map representation error
            // after partially modifying its destination while tables are stable.
            self.kernel.table_pointer(translation.physical)?;
            if current == final_page {
                return Ok(());
            }
            current += PAGE_SIZE;
        }
    }

    /// Copy through the HHDM only after validating every page in the range.
    /// User-supplied integers are never cast directly into dereferenceable
    /// pointers. This also handles ranges crossing noncontiguous physical pages.
    ///
    /// # Safety
    /// The BSP must be in the kernel with this user suspended. APs must not
    /// access its private pages, and no DMA or concurrent writer may modify
    /// its mappings or page contents. The destination must not alias any source
    /// page, including through the HHDM.
    pub unsafe fn copy_from_user(
        &self,
        address: u64,
        destination: &mut [u8],
    ) -> Result<(), PagingError> {
        self.validate_readable(address, destination.len())?;
        let mut copied = 0;
        while copied < destination.len() {
            let current = address + copied as u64;
            let translation = self.translate(current)?.ok_or(PagingError::NotMapped)?;
            let page_offset = (current & (PAGE_SIZE - 1)) as usize;
            let count = (PAGE_SIZE as usize - page_offset).min(destination.len() - copied);
            let source = self
                .kernel
                .offset
                .checked_add(translation.physical)
                .ok_or(PagingError::DirectMapOverflow)?;
            // SAFETY: The checked user walk resolves initialized owned RAM,
            // mapped fully by HHDM. Count stays within this source page and the
            // destination bounds; the caller excludes aliases and writers.
            unsafe {
                core::ptr::copy_nonoverlapping(
                    source as *const u8,
                    destination.as_mut_ptr().add(copied),
                    count,
                );
            }
            copied += count;
        }
        Ok(())
    }

    /// Copy to owned user pages only after validating the entire writable range.
    /// This follows each page's physical mapping through the HHDM rather than
    /// dereferencing the supplied user address or assuming physical contiguity.
    ///
    /// # Safety
    /// The BSP must be in the kernel with this user suspended. APs must not
    /// access its private pages, and no DMA or concurrent access may modify
    /// its mappings or access destination bytes.
    /// No live kernel references may cover those bytes. The source must not
    /// alias any destination page, including through the HHDM or another mapping.
    pub unsafe fn copy_to_user(&self, address: u64, source: &[u8]) -> Result<(), PagingError> {
        self.validate_writable(address, source.len())?;
        let mut copied = 0;
        while copied < source.len() {
            let current = address + copied as u64;
            let translation = self.translate(current)?.ok_or(PagingError::NotMapped)?;
            let page_offset = (current & (PAGE_SIZE - 1)) as usize;
            let count = (PAGE_SIZE as usize - page_offset).min(source.len() - copied);
            let destination = self
                .kernel
                .offset
                .checked_add(translation.physical)
                .ok_or(PagingError::DirectMapOverflow)?;
            // SAFETY: The complete range was checked for effective user write
            // permission and valid HHDM addresses before any write. This chunk
            // stays inside its page and source bounds; the caller excludes
            // concurrent access, changes to mappings, and aliasing references.
            unsafe {
                core::ptr::copy_nonoverlapping(
                    source.as_ptr().add(copied),
                    destination as *mut u8,
                    count,
                );
            }
            copied += count;
        }
        Ok(())
    }

    /// Activate this root using PCID zero and flush stale global translations.
    ///
    /// # Safety
    /// Run at CPL0 on the BSP with interrupts disabled. No AP may activate this
    /// private root or access its lower mappings. Code, current and
    /// interrupt stacks, and all kernel data must reside in the shared upper
    /// half. Keep this root and its borrowed kernel hierarchy live until the
    /// kernel root is restored; userspace must not outlive its owned pages.
    pub unsafe fn activate(&self) -> Result<(), PagingError> {
        self.check_live()?;
        // SAFETY: The caller preserves the shared supervisor code and stack.
        unsafe { switch_root(self.root) };
        Ok(())
    }

    /// Release private leaves/tables after restoring the kernel root. Shared
    /// upper tables are never traversed or freed. Partial failures preserve
    /// remaining ownership and counts so teardown can be retried.
    ///
    /// # Safety
    /// Run on the BSP with interrupts disabled using the original unique
    /// allocator. No live references, DMA, or pending users may access these
    /// pages, and no AP may have used the private root or its owned pages.
    /// Restore through PageTables::activate to flush the BSP's stale user
    /// translations; no AP may retain a translation into this private hierarchy.
    pub unsafe fn destroy(&mut self, frames: &mut FrameAllocator<'_>) -> Result<(), PagingError> {
        self.check_inactive()?;
        self.release_entries(self.root, 4, ENTRIES / 2, frames)?;
        // SAFETY: The root is inactive, its lower entries have been withdrawn,
        // and the shared upper references belong to the live kernel hierarchy.
        unsafe { frames.deallocate(self.root) }.map_err(|_| PagingError::FrameReleaseFailed)?;
        self.root = 0;
        self.owned_tables -= 1;
        debug_assert_eq!(self.owned_tables, 0);
        debug_assert_eq!(self.owned_pages, 0);
        Ok(())
    }

    fn check_live(&self) -> Result<(), PagingError> {
        if self.root == 0 {
            Err(PagingError::InvalidTable)
        } else {
            Ok(())
        }
    }

    fn check_inactive(&self) -> Result<(), PagingError> {
        self.check_live()?;
        if read_cr3() & ADDRESS_MASK == self.root {
            Err(PagingError::StillActive)
        } else {
            Ok(())
        }
    }

    fn allocate_table(&mut self, frames: &mut FrameAllocator<'_>) -> Result<u64, PagingError> {
        let physical = frames.allocate().ok_or(PagingError::OutOfFrames)?;
        let pointer = match self.kernel.table_pointer(physical) {
            Ok(pointer) => pointer,
            Err(error) => {
                // SAFETY: This fresh frame has no mapping or live references.
                unsafe { frames.deallocate(physical) }
                    .map_err(|_| PagingError::FrameReleaseFailed)?;
                return Err(error);
            }
        };
        // SAFETY: A fresh, exclusively owned frame covered by the HHDM contract.
        unsafe { pointer.write_bytes(0, ENTRIES) };
        self.owned_tables += 1;
        Ok(physical)
    }

    fn release_entries(
        &mut self,
        physical: u64,
        level: u32,
        count: usize,
        frames: &mut FrameAllocator<'_>,
    ) -> Result<(), PagingError> {
        let table = self.kernel.table_pointer(physical)?;
        for entry_index in 0..count {
            // SAFETY: The count is 256 at PML4 or 512 at each lower level.
            let slot = unsafe { table.add(entry_index) };
            // SAFETY: The inactive, exclusively owned table is initialized.
            let entry = unsafe { slot.read_volatile() };
            if entry & PRESENT == 0 {
                continue;
            }
            if level > 1 && entry & HUGE != 0 {
                return Err(PagingError::InvalidTable);
            }
            let child = entry & self.kernel.physical_mask;
            if level > 1 {
                self.release_entries(child, level - 1, ENTRIES, frames)?;
            }
            // SAFETY: No live access uses this inactive root. Withdraw the last
            // mapping or parent reference before recycling the owned frame.
            unsafe { slot.write_volatile(0) };
            // SAFETY: The child is exclusively owned, inactive, and no longer
            // reachable from any table except its dormant direct-map alias.
            if unsafe { frames.deallocate(child) }.is_err() {
                // SAFETY: A rejected free leaves the child allocated; restore
                // ownership so the caller can diagnose or retry destruction.
                unsafe { slot.write_volatile(entry) };
                return Err(PagingError::FrameReleaseFailed);
            }
            if level == 1 {
                self.owned_pages -= 1;
            } else {
                self.owned_tables -= 1;
            }
        }
        Ok(())
    }
}

/// Deep-copy the active four-level hierarchy into USABLE frames and activate it.
/// NX support is required; NXE and supervisor write protection are enabled.
/// Allocation failures retain frames consumed by an incomplete copy. The old
/// root stays active if cloning fails; this transition has no rollback yet.
///
/// # Safety
/// Call at CPL0 on the BSP before any AP enters kernel code, with interrupts
/// disabled and a valid Limine four-level hierarchy. `offset` must directly map
/// every current table
/// and every allocatable USABLE frame as full, readable/writable pages. `frames`
/// must be the unique allocator and must never allocate bootloader-reclaimable
/// regions or any live frame. Preserve that HHDM for the lifetime of the result.
pub unsafe fn take_ownership(
    frames: &mut FrameAllocator<'_>,
    offset: u64,
) -> Result<PageTables, PagingError> {
    let cr4: u64;
    // SAFETY: The caller runs at CPL0 on the boot CPU.
    unsafe { asm!("mov {}, cr4", out(reg) cr4, options(nomem, nostack, preserves_flags)) };
    if cr4 & CR4_LA57 != 0 {
        return Err(PagingError::FiveLevelPaging);
    }
    let extended_limit = cpuid(0x8000_0000).eax;
    if extended_limit < 0x8000_0001 || cpuid(0x8000_0001).edx & (1 << 20) == 0 {
        return Err(PagingError::NoExecuteUnsupported);
    }
    // MAXPHYADDR defaults to 36 when the address-width CPUID leaf is absent.
    let physical_bits = if extended_limit >= 0x8000_0008 {
        cpuid(0x8000_0008).eax & 0xff
    } else {
        36
    };
    if !(32..=52).contains(&physical_bits) {
        return Err(PagingError::InvalidPhysicalAddressWidth);
    }
    let physical_mask = ((1_u64 << physical_bits) - 1) & ADDRESS_MASK;
    let old_cr3 = read_cr3();
    let mut tables = PageTables {
        root: old_cr3 & ADDRESS_MASK,
        offset,
        physical_mask,
        owned_tables: 0,
        shared_table: None,
        heap_tables: None,
        _boot_cpu_only: PhantomData,
    };
    let code_address = take_ownership as *const () as u64;
    let stack_address = &old_cr3 as *const u64 as u64;
    let code_before = tables.translate(code_address)?;
    let stack_before = tables.translate(stack_address)?;
    if code_before.is_none() || stack_before.is_none() {
        return Err(PagingError::ChangedBootstrapMapping);
    }
    let new_root = tables.clone_table(tables.root, 4, frames)?;
    tables.root = new_root;
    if tables.translate(code_address)? != code_before
        || tables.translate(stack_address)? != stack_before
    {
        return Err(PagingError::ChangedBootstrapMapping);
    }

    let low: u32;
    let high: u32;
    // SAFETY: NX was checked by CPUID; EFER is available in x86-64 at CPL0.
    unsafe {
        asm!("rdmsr", in("ecx") EFER, out("eax") low, out("edx") high,
            options(nomem, nostack, preserves_flags));
    }
    let efer = ((u64::from(high) << 32) | u64::from(low)) | EFER_NXE;
    // SAFETY: Preserve all EFER bits and only enable supported NX semantics.
    unsafe {
        asm!("wrmsr", in("ecx") EFER, in("eax") efer as u32,
            in("edx") (efer >> 32) as u32, options(nostack, preserves_flags));
    }
    let mut cr0: u64;
    // SAFETY: At CPL0; preserve all CR0 bits and enforce supervisor write
    // protection. The live stack and HHDM already have writable mappings.
    unsafe {
        asm!("mov {}, cr0", out(reg) cr0, options(nomem, nostack, preserves_flags));
        cr0 |= 1 << 16;
        asm!("mov cr0, {}", in(reg) cr0, options(nostack, preserves_flags));
    }

    // With PCIDE, preserve the current PCID; otherwise only PWT/PCD are flags.
    // Bit 63 (no-flush) stays clear. Global TLB leaves may survive MOV CR3, but
    // every cloned leaf has identical physical address and access/cache flags.
    let cr3_flags = if cr4 & CR4_PCIDE != 0 {
        old_cr3 & 0xfff
    } else {
        old_cr3 & ((1 << 3) | (1 << 4))
    };
    // SAFETY: The new root owns a complete copy of every live mapping. This
    // compiler memory barrier publishes all table writes before the CR3 switch.
    unsafe {
        asm!("mov cr3, {}", in(reg) (new_root | cr3_flags), options(nostack, preserves_flags));
    }
    tables.check_active()?;
    Ok(tables)
}

fn check_canonical(address: u64) -> Result<(), PagingError> {
    let upper = address >> 47;
    if upper == 0 || upper == 0x1_ffff {
        Ok(())
    } else {
        Err(PagingError::NonCanonicalAddress)
    }
}

/// Adopt the BSP's sealed root without sharing a mutable PageTables handle.
///
/// # Safety
/// This is a secondary CPU with IF=0 and private exception descriptors loaded.
/// `root` is the permanently retained root published by share_with_secondaries;
/// code, the current boot stack, GDT/TSS/IST and HHDM survive the transition.
/// The caller never mutates mappings, allocates frames or enters userspace.
pub(crate) unsafe fn activate_secondary(root: u64, expected_pat: u64) -> Result<(), PagingError> {
    if !KERNEL_SHARED.load(Ordering::Acquire) {
        return Err(PagingError::NotActive);
    }
    let cr4: u64;
    // SAFETY: CPL0, IF=0; this only inspects this CPU's controls.
    unsafe {
        asm!("mov {}, cr4", out(reg) cr4, options(nomem, nostack, preserves_flags));
    }
    if cr4 & CR4_LA57 != 0 {
        return Err(PagingError::FiveLevelPaging);
    }
    let maximum = cpuid(0x8000_0000).eax;
    if maximum < 0x8000_0001 || cpuid(0x8000_0001).edx & (1 << 20) == 0 {
        return Err(PagingError::NoExecuteUnsupported);
    }
    let bits = if maximum >= 0x8000_0008 {
        cpuid(0x8000_0008).eax & 0xff
    } else {
        36
    };
    if !(32..=52).contains(&bits) || root == 0 || root & !(((1u64 << bits) - 1) & ADDRESS_MASK) != 0
    {
        return Err(PagingError::InvalidPhysicalAddress);
    }
    if cpuid(1).edx & (1 << 16) == 0 {
        return Err(PagingError::UnsupportedCacheLayout);
    }
    let low: u32;
    let high: u32;
    // SAFETY: CPUID reports PAT. Limine synchronizes memory types; reject a
    // mismatch before using the BSP's page-table cache indices on this CPU.
    unsafe {
        asm!("rdmsr", in("ecx") 0x277_u32, out("eax") low, out("edx") high,
        options(nomem, nostack, preserves_flags));
    }
    if (u64::from(high) << 32) | u64::from(low) != expected_pat {
        return Err(PagingError::UnsupportedCacheLayout);
    }
    let low: u32;
    let high: u32;
    let cr0: u64;
    // SAFETY: NX support was checked, other EFER/CR0 bits are preserved. These
    // changes are local. The sealed root maps every live instruction and stack.
    unsafe {
        asm!("rdmsr", in("ecx") EFER, out("eax") low, out("edx") high,
            options(nomem, nostack, preserves_flags));
        let efer = (u64::from(high) << 32) | u64::from(low) | EFER_NXE;
        asm!("wrmsr", in("ecx") EFER, in("eax") efer as u32, in("edx") (efer >> 32) as u32,
            options(nostack, preserves_flags));
        asm!("mov {}, cr0", out(reg) cr0, options(nomem, nostack, preserves_flags));
        asm!("mov cr0, {}", in(reg) (cr0 | (1 << 16)), options(nostack, preserves_flags));
        switch_root(root);
    }
    if read_cr3() & ADDRESS_MASK != root {
        return Err(PagingError::NotActive);
    }
    Ok(())
}

fn check_page_address(address: u64) -> Result<(), PagingError> {
    check_canonical(address)?;
    if address & (PAGE_SIZE - 1) != 0 {
        Err(PagingError::UnalignedAddress)
    } else {
        Ok(())
    }
}

fn check_user_address(address: u64) -> Result<(), PagingError> {
    if (PAGE_SIZE..USER_END).contains(&address) {
        Ok(())
    } else {
        Err(PagingError::InvalidUserAddress)
    }
}

/// Switch roots with a compiler barrier and a fresh PCID-zero translation
/// context. Clearing PGE first invalidates global entries inherited from any
/// previous root; without that step a bootstrap lower mapping could survive.
/// With PGE already clear, MOV CR3 with bit 63 clear flushes PCID zero. Other
/// PCIDs are never selected by this implementation.
unsafe fn switch_root(root: u64) {
    let cr4: u64;
    // SAFETY: All callers run at CPL0 with interrupts disabled on this CPU.
    // The BSP owns private user roots; an AP may only adopt the sealed kernel
    // root. CR3/CR4 changes and the resulting invalidations are CPU-local.
    unsafe { asm!("mov {}, cr4", out(reg) cr4, options(nomem, nostack, preserves_flags)) };
    if cr4 & CR4_PGE != 0 {
        // SAFETY: Only disable global pages, flushing their cached permissions
        // before switching to a root with different lower mappings.
        unsafe {
            asm!("mov cr4, {}", in(reg) (cr4 & !CR4_PGE), options(nostack, preserves_flags));
        }
    }
    // SAFETY: The validated root shares every live kernel code/data mapping.
    // Low bits are zero: PCID zero when enabled, ordinary RAM caching otherwise.
    // Bit 63 is clear so this CR3 write invalidates non-global PCID-zero state.
    unsafe { asm!("mov cr3, {}", in(reg) root, options(nostack, preserves_flags)) };
    if cr4 & CR4_PGE != 0 {
        // SAFETY: Restore the original, supported CR4 value after replacement.
        unsafe { asm!("mov cr4, {}", in(reg) cr4, options(nostack, preserves_flags)) };
    }
}

fn index(address: u64, level: u32) -> usize {
    ((address >> (12 + 9 * (level - 1))) & 0x1ff) as usize
}

fn read_cr3() -> u64 {
    let value: u64;
    // SAFETY: This module is only used by the CPL0 kernel.
    unsafe { asm!("mov {}, cr3", out(reg) value, options(nomem, nostack, preserves_flags)) };
    value
}

// Rust 1.88 declares this intrinsic unsafe; newer compilers declare it safe.
// Keep the advertised minimum compiler without lint noise on newer toolchains.
#[allow(unused_unsafe)]
fn cpuid(leaf: u32) -> CpuidResult {
    // SAFETY: CPUID is guaranteed on the x86-64 target, including extended-leaf
    // discovery. The caller checks the maximum leaf before querying features.
    unsafe { __cpuid(leaf) }
}

fn invalidate_page(address: u64) {
    // SAFETY: CPL0 on the BSP, with a validated canonical address. Callers only
    // change kernel mappings before AP publication, so a local invalidation is
    // sufficient; this is not a remote shootdown. INVLPG also invalidates a
    // matching global translation. No `nomem`: table writes must be visible
    // before this invalidation and later memory access.
    unsafe { asm!("invlpg [{}]", in(reg) address, options(nostack, preserves_flags)) };
}

/// Test active mapping changes and leave TEST_ADDRESS absent for a #PF probe.
///
/// # Safety
/// The take_ownership contract still holds, and no other code uses TEST_ADDRESS.
#[cfg(feature = "boot-smoke")]
pub unsafe fn self_test(tables: &mut PageTables, frames: &mut FrameAllocator<'_>) {
    let code = crate::_start as *const () as u64;
    let before = tables.translate(code).expect("translate kernel code");
    assert!(before.is_some(), "kernel code must stay mapped");
    assert_eq!(tables.translate(TEST_ADDRESS), Ok(None));
    assert_eq!(
        tables.translate(0x0000_8000_0000_0000),
        Err(PagingError::NonCanonicalAddress)
    );
    let physical = frames.allocate().expect("paging test data frame");
    // SAFETY: Fresh owned RAM, an absent test address, and the sole allocator.
    unsafe {
        assert_eq!(
            tables.map_page(TEST_ADDRESS + 1, physical, frames),
            Err(PagingError::UnalignedAddress)
        );
        tables
            .map_page(TEST_ADDRESS, physical, frames)
            .expect("map scratch page");
        assert_eq!(
            tables.map_page(TEST_ADDRESS, physical, frames),
            Err(PagingError::AlreadyMapped)
        );
    }
    assert_eq!(
        tables.translate(TEST_ADDRESS + 123),
        Ok(Some(physical + 123))
    );
    let alias = TEST_ADDRESS as *mut u64;
    let direct = tables.table_pointer(physical).expect("scratch direct map");
    for word in 0..ENTRIES {
        let expected = 0x4d49_5252_4f52_0000 | word as u64;
        // SAFETY: Both virtual aliases cover this exclusively owned data page.
        // Raw volatile accesses create no simultaneous Rust references.
        unsafe {
            alias.add(word).write_volatile(expected);
            assert_eq!(alias.add(word).read_volatile(), expected);
            assert_eq!(direct.add(word).read_volatile(), expected);
        }
    }
    // SAFETY: No live reference or later access uses the scratch alias.
    unsafe {
        assert_eq!(tables.unmap_page(TEST_ADDRESS), Ok(physical));
        assert_eq!(tables.unmap_page(TEST_ADDRESS), Err(PagingError::NotMapped));
    }
    assert_eq!(tables.translate(TEST_ADDRESS), Ok(None));
    assert_eq!(
        tables.translate(code).expect("kernel mapping after test"),
        before
    );
    // SAFETY: Unmapping has invalidated the scratch translation and all raw
    // accesses above have ended. Only the dormant direct-map alias remains.
    unsafe { frames.deallocate(physical) }.expect("recycle unmapped scratch frame");
    crate::serial::println!("MIRROR_PAGING_OK");
    // SAFETY: The same sole-CPU, disabled-IRQ and HHDM contracts remain valid.
    unsafe {
        self_test_userspace(tables, frames);
        self_test_user_mutation(tables, frames);
    }
}

#[cfg(feature = "boot-smoke")]
unsafe fn self_test_user_mutation(kernel: &PageTables, frames: &mut FrameAllocator<'_>) {
    const DATA: u64 = 0x5000_0000;
    const NEIGHBOR: u64 = DATA + PAGE_SIZE;
    const OTHER_TABLE: u64 = DATA + (1 << 21);
    const ACCESSED_DIRTY: u64 = (1 << 5) | (1 << 6);
    let before = frames.stats().expect("mutation test frame tracking");
    // SAFETY: The kernel root is active on the sole CPU with IF clear. The
    // unique allocator and shared supervisor mappings outlive this test root.
    let mut user = unsafe { UserAddressSpace::new(kernel, frames) }.unwrap();
    for (address, value) in [(DATA, 0x6a), (NEIGHBOR, 0x7b), (OTHER_TABLE, 0x8c)] {
        let physical = frames.allocate().expect("mutation test data page");
        let pointer = kernel.table_pointer(physical).unwrap().cast::<u8>();
        // SAFETY: Initialize this exclusively owned full frame before handing
        // it to an inactive user. No live aliases survive the raw writes.
        unsafe {
            pointer.write_bytes(value, PAGE_SIZE as usize);
            user.map_page(address, physical, UserPermissions::READ_WRITE, frames)
                .unwrap();
        }
    }
    assert_eq!(user.owned_page_count(), 3);
    assert_eq!(user.owned_table_count(), 5);
    let original_translation = user.translate(DATA).unwrap().unwrap();
    let path = user.owned_path(DATA).unwrap();
    // SAFETY: Simulate hardware-maintained leaf bits in an inactive owned PTE.
    unsafe { path.slots[3].write_volatile(path.entries[3] | ACCESSED_DIRTY) };
    let preserved = (path.entries[3] | ACCESSED_DIRTY) & !(USER | WRITABLE | NO_EXECUTE);
    for permissions in [
        Some(UserPermissions::READ_ONLY),
        None,
        Some(UserPermissions::READ_EXECUTE),
        Some(UserPermissions::READ_WRITE),
    ] {
        // SAFETY: No userspace execution or DMA can race this inactive root.
        unsafe { user.protect_page(DATA, permissions) }.unwrap();
        let leaf = user.owned_path(DATA).unwrap().entries[3];
        assert_eq!(leaf & !(USER | WRITABLE | NO_EXECUTE), preserved);
        let mut contents = [0_u8; 16];
        if let Some(permissions) = permissions {
            assert_eq!(
                user.translate(DATA),
                Ok(Some(UserTranslation {
                    physical: original_translation.physical,
                    writable: permissions.writable,
                    executable: permissions.executable,
                }))
            );
            // SAFETY: Disjoint kernel buffer and inactive initialized user data.
            unsafe { user.copy_from_user(DATA, &mut contents) }.unwrap();
            assert_eq!(contents, [0x6a; 16]);
        } else {
            assert_eq!(user.translate(DATA), Err(PagingError::UserAccessDenied));
            assert_eq!(user.next_owned_page(DATA, DATA + PAGE_SIZE), Ok(Some(DATA)));
            // SAFETY: Full validation must reject the inaccessible page before
            // touching this separate kernel destination or the owned user data.
            unsafe {
                assert_eq!(
                    user.copy_from_user(DATA, &mut contents),
                    Err(PagingError::UserAccessDenied)
                );
                assert_eq!(
                    user.copy_to_user(DATA, &[0xff]),
                    Err(PagingError::UserAccessDenied)
                );
            }
            assert_eq!(contents, [0; 16]);
            assert_eq!(leaf & (USER | WRITABLE), 0);
            assert_ne!(leaf & NO_EXECUTE, 0);
        }
    }
    for depth in 0..3 {
        let parent = path.entries[depth];
        for (altered, expected) in [
            (parent & !USER, PagingError::UserAccessDenied),
            (parent | HUGE, PagingError::InvalidTable),
        ] {
            // SAFETY: Temporarily corrupt/restrict one inactive parent to
            // exercise complete-path validation before either operation writes.
            unsafe {
                path.slots[depth].write_volatile(altered);
                assert_eq!(user.protect_page(DATA, None).unwrap_err(), expected);
                assert_eq!(user.unmap_page(DATA, frames).unwrap_err(), expected);
                assert_eq!(
                    user.next_owned_page(DATA, DATA + PAGE_SIZE).unwrap_err(),
                    expected
                );
                path.slots[depth].write_volatile(parent);
            }
            assert_eq!(user.owned_page_count(), 3);
            assert_eq!(user.owned_table_count(), 5);
            assert_eq!(user.translate(DATA), Ok(Some(original_translation)));
        }
        for (altered, permissions) in [
            (parent & !WRITABLE, UserPermissions::READ_WRITE),
            (parent | NO_EXECUTE, UserPermissions::READ_EXECUTE),
        ] {
            // SAFETY: This parent and target leaf are inactive and exclusive.
            unsafe {
                path.slots[depth].write_volatile(altered);
                assert_eq!(
                    user.protect_page(DATA, Some(permissions)),
                    Err(PagingError::InvalidUserPermissions)
                );
                path.slots[depth].write_volatile(parent);
            }
            assert_eq!(user.translate(DATA), Ok(Some(original_translation)));
        }
    }
    for address in [0, USER_END, TEST_ADDRESS] {
        // SAFETY: Invalid user page addresses must be rejected before mutation.
        unsafe {
            assert_eq!(
                user.unmap_page(address, frames),
                Err(PagingError::InvalidUserAddress)
            );
            assert_eq!(
                user.protect_page(address, None),
                Err(PagingError::InvalidUserAddress)
            );
        }
    }
    // SAFETY: Every mutation uses an inactive owned tree except the explicit
    // active-root rejections; kernel code and stack stay in the shared upper half.
    unsafe {
        assert_eq!(
            user.unmap_page(DATA + 1, frames),
            Err(PagingError::UnalignedAddress)
        );
        assert_eq!(
            user.protect_page(DATA + 1, None),
            Err(PagingError::UnalignedAddress)
        );
        assert_eq!(
            user.protect_page(
                DATA,
                Some(UserPermissions {
                    writable: true,
                    executable: true
                })
            ),
            Err(PagingError::InvalidUserPermissions)
        );
        assert_eq!(
            user.protect_page(DATA + 2 * PAGE_SIZE, None),
            Err(PagingError::NotMapped)
        );
        user.activate().unwrap();
        assert_eq!(user.unmap_page(DATA, frames), Err(PagingError::StillActive));
        assert_eq!(user.protect_page(DATA, None), Err(PagingError::StillActive));
        kernel.activate().unwrap();
        user.protect_page(DATA, None).unwrap();
        assert_eq!(user.unmap_page(DATA, frames), Ok(true));
        assert_eq!(user.unmap_page(DATA, frames), Ok(false));
    }
    assert_eq!(user.owned_page_count(), 2);
    assert_eq!(user.owned_table_count(), 5);
    let mut neighbor = [0; 16];
    // SAFETY: Neighbor belongs to a distinct owned frame and survives removal.
    unsafe { user.copy_from_user(NEIGHBOR, &mut neighbor) }.unwrap();
    assert_eq!(neighbor, [0x7b; 16]);
    // SAFETY: These inactive pages have no live aliases or users.
    unsafe { assert_eq!(user.unmap_page(NEIGHBOR, frames), Ok(true)) };
    assert_eq!(user.owned_table_count(), 4);
    assert_eq!(user.owned_page_count(), 1);
    // SAFETY: The last data leaf and its lower table chain can now be released.
    unsafe { assert_eq!(user.unmap_page(OTHER_TABLE, frames), Ok(true)) };
    assert_eq!(user.owned_table_count(), 1);
    assert_eq!(user.owned_page_count(), 0);
    assert_eq!(
        frames.stats().unwrap().allocated_frames,
        before.allocated_frames + 1
    );

    // A failed map can leave one, two, or three empty parent tables. Recreate
    // those owned states and require absent-page unmap to reclaim all of them.
    for count in 1..=3 {
        let mut physical = user.root;
        for level in (2..=4).rev().take(count) {
            let table = kernel.table_pointer(physical).unwrap();
            let child = user.allocate_table(frames).unwrap();
            // SAFETY: A fresh zeroed child becomes reachable from one inactive
            // owned parent. No leaf or other table uses this absent path.
            unsafe {
                table
                    .add(index(DATA, level))
                    .write_volatile(child | PRESENT | WRITABLE | USER);
            }
            physical = child;
        }
        assert_eq!(user.owned_table_count(), count + 1);
        // SAFETY: All partial-path tables are inactive and exclusively owned.
        unsafe { assert_eq!(user.unmap_page(DATA, frames), Ok(false)) };
        assert_eq!(user.owned_table_count(), 1);
        assert_eq!(
            frames.stats().unwrap().allocated_frames,
            before.allocated_frames + 1
        );
    }
    for _ in 0..3 {
        let physical = frames.allocate().unwrap();
        let pointer = kernel.table_pointer(physical).unwrap().cast::<u8>();
        // SAFETY: Frame recycling does not zero memory: the caller must do so
        // before exposing reused RAM to userspace. The inactive root takes ownership.
        unsafe {
            pointer.write_bytes(0, PAGE_SIZE as usize);
            user.map_page(DATA, physical, UserPermissions::READ_WRITE, frames)
                .unwrap();
        }
        let mut contents = [0xff; PAGE_SIZE as usize];
        // SAFETY: The separate stack buffer and inactive owned page do not alias.
        unsafe {
            user.copy_from_user(DATA, &mut contents).unwrap();
            assert_eq!(contents, [0; PAGE_SIZE as usize]);
            user.copy_to_user(DATA, &[0xa5; 32]).unwrap();
            assert_eq!(user.unmap_page(DATA, frames), Ok(true));
        }
        assert_eq!(user.owned_table_count(), 1);
        assert_eq!(user.owned_page_count(), 0);
    }
    // Traverse gaps at PTE, PDE, PDPTE and PML4 levels. In particular the last
    // leaf is beyond a one-TiB hole, which must not be scanned page by page.
    let sparse = [
        DATA,
        (1 << 34) + (1 << 21) + PAGE_SIZE,
        (1 << 40) + PAGE_SIZE,
    ];
    for &address in &sparse {
        let physical = frames.allocate().unwrap();
        let pointer = kernel.table_pointer(physical).unwrap().cast::<u8>();
        // SAFETY: Fresh initialized frame becomes exclusively owned by this
        // inactive root, and the temporary HHDM initialization retains no alias.
        unsafe {
            pointer.write_bytes(0, PAGE_SIZE as usize);
            user.map_page(address, physical, UserPermissions::READ_WRITE, frames)
                .unwrap();
        }
    }
    // SAFETY: Retaining the last leaf as inaccessible tests ownership traversal
    // independently from user translation while no userspace context is active.
    unsafe { user.protect_page(sparse[2], None) }.unwrap();
    assert_eq!(
        user.next_owned_page(PAGE_SIZE, USER_END),
        Ok(Some(sparse[0]))
    );
    assert_eq!(user.next_owned_page(DATA, DATA), Ok(None));
    for index in 0..sparse.len() - 1 {
        assert_eq!(
            user.next_owned_page(sparse[index] + PAGE_SIZE, sparse[index + 1]),
            Ok(None)
        );
        assert_eq!(
            user.next_owned_page(sparse[index] + PAGE_SIZE, sparse[index + 1] + PAGE_SIZE),
            Ok(Some(sparse[index + 1]))
        );
    }
    assert_eq!(
        user.next_owned_page(sparse[2] + PAGE_SIZE, USER_END),
        Ok(None)
    );
    assert_eq!(
        user.next_owned_page(0, PAGE_SIZE),
        Err(PagingError::InvalidUserAddress)
    );
    assert_eq!(
        user.next_owned_page(DATA + 1, USER_END),
        Err(PagingError::UnalignedAddress)
    );
    assert_eq!(
        user.next_owned_page(DATA, DATA + 1),
        Err(PagingError::UnalignedAddress)
    );
    assert_eq!(
        user.next_owned_page(DATA, USER_END + PAGE_SIZE),
        Err(PagingError::InvalidUserAddress)
    );
    assert_eq!(
        user.next_owned_page(DATA, DATA - PAGE_SIZE),
        Err(PagingError::AddressRangeOverflow)
    );
    for address in sparse {
        // SAFETY: Each returned leaf remains exclusively owned and inactive,
        // including the last supervisor-only leaf and its now-empty parents.
        unsafe { assert_eq!(user.unmap_page(address, frames), Ok(true)) };
    }
    assert_eq!(user.owned_page_count(), 0);
    assert_eq!(user.owned_table_count(), 1);
    // SAFETY: Only the inactive empty root remains owned.
    unsafe { user.destroy(frames) }.unwrap();
    assert_eq!(user.owned_table_count(), 0);
    assert_eq!(
        user.next_owned_page(DATA, USER_END),
        Err(PagingError::InvalidTable)
    );
    // SAFETY: The destroyed-root checks must return before touching a table.
    unsafe {
        assert_eq!(
            user.unmap_page(DATA, frames),
            Err(PagingError::InvalidTable)
        );
        assert_eq!(
            user.protect_page(DATA, None),
            Err(PagingError::InvalidTable)
        );
    }
    assert_eq!(
        frames.stats().unwrap().allocated_frames,
        before.allocated_frames
    );
    crate::serial::println!("MIRROR_USER_VM_PAGING_OK");
}

#[cfg(feature = "boot-smoke")]
unsafe fn self_test_userspace(kernel: &PageTables, frames: &mut FrameAllocator<'_>) {
    const DATA: u64 = 0x4000_0000;
    const CODE: u64 = 0x0080_0000_0000;
    let before = frames.stats().expect("user test frame tracking");
    // SAFETY: The kernel root is active, recycling is enabled, and only the
    // boot CPU owns this allocator and the borrowed upper-half hierarchy.
    let mut user = unsafe { UserAddressSpace::new(kernel, frames) }.expect("new user root");
    assert_eq!(user.owned_table_count(), 1);
    assert_eq!(user.owned_page_count(), 0);
    assert_ne!(user.root_physical_address(), kernel.root_physical_address());
    assert_eq!(user.translate(DATA), Ok(None));
    for address in [0, PAGE_SIZE - 1, USER_END, u64::MAX, TEST_ADDRESS] {
        assert_eq!(
            user.translate(address),
            Err(PagingError::InvalidUserAddress)
        );
    }
    assert_eq!(user.validate_readable(PAGE_SIZE, 0), Ok(()));
    assert_eq!(
        user.validate_readable(DATA, usize::MAX),
        Err(PagingError::AddressRangeOverflow)
    );
    assert_eq!(
        user.validate_readable(USER_END - 1, 2),
        Err(PagingError::InvalidUserAddress)
    );
    let first = frames.allocate().expect("first user test page");
    let second = frames.allocate().expect("second user test page");
    let code = frames.allocate().expect("user code test page");
    for (physical, value) in [(first, 0x11_u8), (second, 0x22), (code, 0xcc)] {
        let pointer = kernel.table_pointer(physical).expect("user test HHDM");
        // SAFETY: Each fresh exclusively owned page is fully HHDM mapped.
        unsafe { pointer.cast::<u8>().write_bytes(value, PAGE_SIZE as usize) };
    }
    // SAFETY: Failed mappings retain caller ownership. Successful mappings
    // consume distinct initialized frames without any additional live alias.
    unsafe {
        assert_eq!(
            user.map_page(TEST_ADDRESS, first, UserPermissions::READ_ONLY, frames),
            Err(PagingError::InvalidUserAddress)
        );
        assert_eq!(
            user.map_page(DATA + 1, first, UserPermissions::READ_ONLY, frames),
            Err(PagingError::UnalignedAddress)
        );
        assert_eq!(
            user.map_page(
                DATA,
                first,
                UserPermissions {
                    writable: true,
                    executable: true,
                },
                frames
            ),
            Err(PagingError::InvalidUserPermissions)
        );
        user.map_page(DATA, first, UserPermissions::READ_ONLY, frames)
            .expect("user read-only page");
        user.map_page(
            DATA + PAGE_SIZE,
            second,
            UserPermissions::READ_WRITE,
            frames,
        )
        .expect("user writable page");
        user.map_page(CODE, code, UserPermissions::READ_EXECUTE, frames)
            .expect("user executable page");
    }
    assert_eq!(user.owned_page_count(), 3);
    assert_eq!(
        user.translate(DATA + 17),
        Ok(Some(UserTranslation {
            physical: first + 17,
            writable: false,
            executable: false,
        }))
    );
    assert_eq!(
        user.translate(DATA + PAGE_SIZE),
        Ok(Some(UserTranslation {
            physical: second,
            writable: true,
            executable: false,
        }))
    );
    assert_eq!(
        user.translate(CODE),
        Ok(Some(UserTranslation {
            physical: code,
            writable: false,
            executable: true,
        }))
    );
    let mut copied = [0_u8; 32];
    // SAFETY: User has not run, all source pages are initialized, and copied is
    // a disjoint kernel-stack buffer. No concurrent writer or DMA exists.
    unsafe {
        user.copy_from_user(DATA + PAGE_SIZE - 16, &mut copied)
            .expect("cross-page user copy");
    }
    assert_eq!(&copied[..16], &[0x11; 16]);
    assert_eq!(&copied[16..], &[0x22; 16]);
    let previous = copied;
    // SAFETY: This invalid range must fail before copying any bytes; the
    // destination remains disjoint from both live user pages.
    unsafe {
        assert_eq!(
            user.copy_from_user(DATA + 2 * PAGE_SIZE - 16, &mut copied),
            Err(PagingError::NotMapped)
        );
    }
    assert_eq!(copied, previous, "invalid copy changed its destination");
    // SAFETY: The user is inactive, all frames are exclusively owned, and the
    // same sole-CPU, disabled-IRQ and HHDM contracts still apply.
    unsafe { self_test_copy_to_user(&mut user, kernel, frames, DATA, CODE) };

    // Exercise permission accumulation, not merely the leaf's permission bits.
    for level in (2..=4).rev() {
        let mut physical = user.root;
        for parent in ((level + 1)..=4).rev() {
            let table = kernel.table_pointer(physical).expect("parent test table");
            // SAFETY: This is an in-range initialized table entry.
            physical =
                unsafe { table.add(index(CODE, parent)).read_volatile() } & kernel.physical_mask;
        }
        let table = kernel
            .table_pointer(physical)
            .expect("permission test table");
        // SAFETY: The user root is inactive and this initialized slot is ours.
        let slot = unsafe { table.add(index(CODE, level)) };
        // SAFETY: The slot is aligned and points into the exclusively owned table.
        let original = unsafe { slot.read_volatile() };
        // SAFETY: Temporarily restrict the inactive table; no user can run.
        unsafe { slot.write_volatile(original & !USER) };
        assert_eq!(user.translate(CODE), Err(PagingError::UserAccessDenied));
        // SAFETY: Restore user access but impose NX above the executable leaf.
        unsafe { slot.write_volatile(original | NO_EXECUTE) };
        assert!(!user.translate(CODE).unwrap().unwrap().executable);
        // SAFETY: Restore the original inactive entry before any activation.
        unsafe { slot.write_volatile(original) };
    }
    let root = kernel.table_pointer(user.root).expect("user root HHDM");
    // SAFETY: This lower PML4 entry is in our inactive root.
    let data_slot = unsafe { root.add(index(DATA, 4)) };
    // SAFETY: It was initialized by map_page.
    let data_entry = unsafe { data_slot.read_volatile() };
    // SAFETY: Restrict the inactive entry to test effective write permissions.
    unsafe { data_slot.write_volatile(data_entry & !WRITABLE) };
    assert!(!user.translate(DATA + PAGE_SIZE).unwrap().unwrap().writable);
    // SAFETY: Restore the inactive entry before using this root.
    unsafe { data_slot.write_volatile(data_entry) };
    for entry_index in ENTRIES / 2..ENTRIES {
        // SAFETY: Every index stays within the initialized root page.
        assert_eq!(unsafe { root.add(entry_index).read_volatile() } & USER, 0);
    }
    // SAFETY: All code, data, and current stack are in the shared upper half;
    // IRQs are disabled. The root stays owned until the kernel is restored.
    unsafe {
        user.activate().expect("activate user root");
        assert_eq!(read_cr3() & ADDRESS_MASK, user.root);
        assert_eq!(user.destroy(frames), Err(PagingError::StillActive));
        kernel.activate().expect("restore kernel root");
        user.destroy(frames).expect("reclaim user root");
    }
    assert_eq!(user.owned_table_count(), 0);
    assert_eq!(user.owned_page_count(), 0);
    assert_eq!(user.root_physical_address(), 0);
    assert_eq!(user.translate(DATA), Err(PagingError::InvalidTable));
    assert_eq!(
        frames.stats().unwrap().allocated_frames,
        before.allocated_frames
    );
    crate::serial::println!("MIRROR_USER_PAGING_OK");
}

#[cfg(feature = "boot-smoke")]
unsafe fn self_test_copy_to_user(
    user: &mut UserAddressSpace<'_>,
    kernel: &PageTables,
    frames: &mut FrameAllocator<'_>,
    existing_data: u64,
    existing_code: u64,
) {
    const WRITES: u64 = 0x6000_0000;
    let first = frames.allocate().expect("copy-out first page");
    let second = frames.allocate().expect("copy-out second page");
    let readonly = frames.allocate().expect("copy-out read-only page");
    // Descending physical addresses ensure a cross-page copy cannot succeed by
    // treating the first physical page and its successor as a contiguous span.
    let (first, second) = (first.max(second), first.min(second));
    assert!(first > second);
    for (address, physical, permissions, value) in [
        (WRITES, first, UserPermissions::READ_WRITE, 0x33),
        (
            WRITES + PAGE_SIZE,
            second,
            UserPermissions::READ_WRITE,
            0x44,
        ),
        (
            WRITES + 2 * PAGE_SIZE,
            readonly,
            UserPermissions::READ_ONLY,
            0x55,
        ),
    ] {
        let pointer = kernel.table_pointer(physical).expect("copy-out HHDM");
        // SAFETY: This fresh frame is uniquely owned and fully HHDM mapped.
        // Initialization finishes before ownership is transferred to the root.
        unsafe {
            pointer.cast::<u8>().write_bytes(value, PAGE_SIZE as usize);
            user.map_page(address, physical, permissions, frames)
                .expect("copy-out test mapping");
        }
    }
    assert_eq!(user.validate_writable(PAGE_SIZE, 0), Ok(()));
    assert_eq!(
        user.validate_writable(WRITES, 2 * PAGE_SIZE as usize),
        Ok(())
    );
    assert_eq!(
        user.validate_writable(WRITES, usize::MAX),
        Err(PagingError::AddressRangeOverflow)
    );
    assert_eq!(
        user.validate_writable(USER_END - 1, 2),
        Err(PagingError::InvalidUserAddress)
    );
    let source: [u8; 32] = core::array::from_fn(|index| 0x80 + index as u8);
    let mut around_boundary = [0_u8; 48];
    // SAFETY: The inactive user's frames and these kernel buffers are disjoint.
    // No references, DMA, or concurrent accesses cover destination user bytes.
    unsafe {
        user.copy_to_user(WRITES + PAGE_SIZE - 16, &source)
            .expect("cross-page user copy-out");
        user.copy_from_user(WRITES + PAGE_SIZE - 24, &mut around_boundary)
            .expect("verify copied bytes and neighbors");
    }
    assert_eq!(&around_boundary[..8], &[0x33; 8]);
    assert_eq!(&around_boundary[8..40], &source);
    assert_eq!(&around_boundary[40..], &[0x44; 8]);

    for address in [0, PAGE_SIZE - 1, TEST_ADDRESS, USER_END - 8, u64::MAX - 8] {
        // SAFETY: Invalid integer destinations must be rejected without any
        // write; the source is a disjoint, initialized kernel buffer.
        unsafe {
            assert_eq!(
                user.copy_to_user(address, &source),
                Err(PagingError::InvalidUserAddress)
            );
        }
    }
    for (address, expected) in [
        (existing_data, 0x11),
        (existing_code, 0xcc),
        (WRITES + 2 * PAGE_SIZE, 0x55),
    ] {
        assert_eq!(
            user.validate_writable(address, 1),
            Err(PagingError::UserAccessDenied)
        );
        let mut after = [0_u8; 1];
        // SAFETY: Owned initialized user frames and disjoint kernel buffers.
        // The copy-out must fail before touching the RO or RX destination.
        unsafe {
            assert_eq!(
                user.copy_to_user(address, &source[..1]),
                Err(PagingError::UserAccessDenied)
            );
            user.copy_from_user(address, &mut after)
                .expect("read rejected copy-out destination");
        }
        assert_eq!(after, [expected]);
    }

    let readonly_boundary = WRITES + 2 * PAGE_SIZE - 16;
    let mut before = [0_u8; 32];
    let mut after = [0_u8; 32];
    // SAFETY: Both pages are readable and only the first is writable. All
    // buffers are disjoint; rejection must preserve the entire destination.
    unsafe {
        user.copy_from_user(readonly_boundary, &mut before).unwrap();
        assert_eq!(
            user.copy_to_user(readonly_boundary, &source),
            Err(PagingError::UserAccessDenied)
        );
        user.copy_from_user(readonly_boundary, &mut after).unwrap();
    }
    assert_eq!(after, before, "copy-out wrote before finding the RO page");
    let missing_boundary = existing_data + 2 * PAGE_SIZE - 16;
    let mut writable_tail = [0_u8; 16];
    // SAFETY: The first 16 bytes belong to the existing writable page, while
    // the rest is unmapped. Full-range validation must prevent partial writes.
    unsafe {
        assert_eq!(
            user.copy_to_user(missing_boundary, &source),
            Err(PagingError::NotMapped)
        );
        user.copy_from_user(missing_boundary, &mut writable_tail)
            .unwrap();
    }
    assert_eq!(writable_tail, [0x22; 16]);

    for level in (2..=4).rev() {
        let mut physical = user.root;
        for parent in ((level + 1)..=4).rev() {
            let table = kernel.table_pointer(physical).expect("copy-out parent");
            // SAFETY: This in-range entry belongs to the inactive owned tree.
            physical =
                unsafe { table.add(index(WRITES, parent)).read_volatile() } & kernel.physical_mask;
        }
        let table = kernel.table_pointer(physical).expect("copy-out table");
        // SAFETY: The root is inactive; this aligned, initialized entry is ours.
        let slot = unsafe { table.add(index(WRITES, level)) };
        // SAFETY: No concurrent software modifies the owned table.
        let original = unsafe { slot.read_volatile() };
        for denied_bit in [WRITABLE, USER] {
            // SAFETY: Restrict an inactive parent and ensure the effective
            // permission check rejects its otherwise writable user leaf.
            unsafe {
                slot.write_volatile(original & !denied_bit);
                assert_eq!(
                    user.validate_writable(WRITES, 1),
                    Err(PagingError::UserAccessDenied)
                );
                assert_eq!(
                    user.copy_to_user(WRITES, &source[..1]),
                    Err(PagingError::UserAccessDenied)
                );
                slot.write_volatile(original);
            }
        }
    }
    let mut unchanged = [0_u8; 1];
    // SAFETY: Restrictions are restored, the inactive page is readable, and
    // unchanged is a disjoint kernel buffer.
    unsafe { user.copy_from_user(WRITES, &mut unchanged) }.unwrap();
    assert_eq!(unchanged, [0x33], "parent denial did not prevent writing");
}

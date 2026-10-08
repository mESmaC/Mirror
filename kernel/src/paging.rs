//! Bootstrap ownership of the four-level x86-64 page-table hierarchy.
//!
//! We copy Limine's tables, preserving every present leaf and its permissions.
//! This keeps the kernel, bootstrap stack, and direct map available across CR3.
//! Only the boot CPU may use this module, with interrupts disabled. There is no
//! address-space isolation, huge-page splitting, SMP shootdown, or table-frame
//! reclamation yet. Bootloader-reclaimable memory stays reserved for the stack and
//! boot responses even after the page-table hierarchy has been copied.

use core::{
    arch::{
        asm,
        x86_64::{__cpuid, CpuidResult},
    },
    marker::PhantomData,
};

use crate::memory::{FrameAllocator, PAGE_SIZE};

const PRESENT: u64 = 1;
const WRITABLE: u64 = 1 << 1;
const HUGE: u64 = 1 << 7;
const NO_EXECUTE: u64 = 1 << 63;
const ADDRESS_MASK: u64 = 0x000f_ffff_ffff_f000;
const ENTRIES: usize = 512;
const CR4_LA57: u64 = 1 << 12;
const CR4_PCIDE: u64 = 1 << 17;
const EFER: u32 = 0xc000_0080;
const EFER_NXE: u64 = 1 << 11;

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
}

pub struct PageTables {
    root: u64,
    offset: u64,
    physical_mask: u64,
    owned_tables: usize,
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
    /// `physical` must be a full, valid RAM page the caller may map writable.
    /// The caller must account for aliases and Rust reference lifetimes. The
    /// allocator must remain the unique allocator used to create these tables.
    pub unsafe fn map_page(
        &mut self,
        address: u64,
        physical: u64,
        frames: &mut FrameAllocator<'_>,
    ) -> Result<(), PagingError> {
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
        unsafe { slot.write_volatile(physical | PRESENT | WRITABLE | NO_EXECUTE) };
        invalidate_page(address);
        Ok(())
    }

    /// Remove a 4 KiB leaf and return its physical frame without freeing it.
    /// Empty parent tables remain owned and available for later mappings.
    ///
    /// # Safety
    /// Run only on the boot CPU with interrupts disabled and this root active.
    /// No live reference or subsequent access may depend on the removed alias.
    /// In particular, keep the executing code, stack, and HHDM of all table
    /// frames mapped. The caller must handle any remaining physical aliases.
    pub unsafe fn unmap_page(&mut self, address: u64) -> Result<u64, PagingError> {
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

/// Deep-copy the active four-level hierarchy into USABLE frames and activate it.
/// NX support is required; NXE and supervisor write protection are enabled.
/// Allocation failures retain frames consumed by an incomplete copy. The old
/// root stays active if cloning fails; this transition has no rollback yet.
///
/// # Safety
/// Call at CPL0 on the sole running CPU, with interrupts disabled and a valid
/// Limine four-level hierarchy. `offset` must directly map every current table
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

fn check_page_address(address: u64) -> Result<(), PagingError> {
    check_canonical(address)?;
    if address & (PAGE_SIZE - 1) != 0 {
        Err(PagingError::UnalignedAddress)
    } else {
        Ok(())
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
    // SAFETY: CPL0 on the sole CPU. The caller validated a canonical address.
    // INVLPG also invalidates a matching global translation. No `nomem`: table
    // writes must be visible before this invalidation and later memory access.
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
}

//! Bootstrap ownership of the four-level x86-64 page-table hierarchy.
//!
//! We copy Limine's tables, preserving every present leaf and its permissions.
//! This keeps the kernel, bootstrap stack, and direct map available across CR3.
//! Private user roots borrow supervisor-only upper mappings and own their lower
//! tables and leaves. Only the boot CPU may mutate this module, with interrupts
//! disabled. There is no huge-page splitting or SMP shootdown yet.
//! Bootloader-reclaimable memory stays reserved for the stack and
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
const USER: u64 = 1 << 2;
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

    /// Restore the kernel root after executing an isolated user address space.
    ///
    /// # Safety
    /// Run at CPL0 on the sole CPU with interrupts disabled. The current code,
    /// stack, and all live kernel data must belong to the shared upper half.
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

/// An isolated lower half with immutable, borrowed kernel mappings above it.
/// There is deliberately no Drop: teardown requires the unique frame allocator
/// and proof that this root is inactive. Call destroy explicitly after restore.
pub struct UserAddressSpace<'kernel> {
    kernel: &'kernel PageTables,
    root: u64,
    owned_tables: usize,
    owned_pages: usize,
}

impl<'kernel> UserAddressSpace<'kernel> {
    /// Allocate an empty lower half and borrow the kernel's upper hierarchy.
    /// The upper PML4 entries always have U/S clear, irrespective of permissions
    /// below them. Bootloader lower mappings are never copied into this root.
    ///
    /// # Safety
    /// Run at CPL0 on the sole CPU with interrupts disabled and `kernel` active.
    /// Preserve its HHDM and use its unique allocator with recycling enabled.
    /// Executing code, stack, and all kernel data used while this root is active
    /// must be in the upper half. No other code may mutate the borrowed tables.
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
            // SAFETY: Both tables are full direct-mapped pages. Source is live,
            // immutable, and destination is still unreachable by the CPU.
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
    /// Run on the sole CPU with interrupts disabled, using the same allocator
    /// as new. The caller must exclusively own a full, initialized USABLE frame
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
            self.translate(current)?.ok_or(PagingError::NotMapped)?;
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
    /// The sole CPU must be in the kernel with this user suspended, and no DMA
    /// or concurrent writer may modify its mappings or page contents. The
    /// destination must not alias any source page, including through the HHDM.
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

    /// Activate this root using PCID zero and flush stale global translations.
    ///
    /// # Safety
    /// Run at CPL0 on the sole CPU with interrupts disabled. Code, current and
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
    /// Run on the sole CPU with interrupts disabled using the original unique
    /// allocator. No live references, DMA, or pending users may access these
    /// pages. Restore through PageTables::activate to invalidate stale TLB state.
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
    // SAFETY: All callers run at CPL0 with interrupts disabled on the sole CPU.
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
    // SAFETY: The same sole-CPU, disabled-IRQ and HHDM contracts remain valid.
    unsafe { self_test_userspace(tables, frames) };
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

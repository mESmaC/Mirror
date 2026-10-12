//! Per-process anonymous virtual memory. Region edits are planned before page
//! tables change; physical work runs only after the user root is suspended.
//! Release/protect skip absent paging subtrees. Resident work remains bounded
//! by 8192 pages per process and is not yet preemptible on the single boot CPU.

use crate::{
    memory::{FrameAllocator, PAGE_SIZE},
    paging::{PagingError, UserAddressSpace, UserPermissions},
};

use mirror_vm_protocol::{ARENA_END, ARENA_START, MAX_REGIONS, RESERVED_LIMIT};
const RESIDENT_BITMAP_WORDS: usize = (RESERVED_LIMIT / PAGE_SIZE / 64) as usize;
const USER_END: u64 = 1 << 47;
const READ: u64 = 1;
const READ_WRITE: u64 = 3;
const EINVAL: i64 = -22;
const ENOMEM: i64 = -12;
const EACCES: i64 = -13;
const EFAULT: i64 = -14;
const EEXIST: i64 = -17;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Region {
    start: u64,
    end: u64,
    protection: u64,
}

impl Region {
    const EMPTY: Self = Self {
        start: 0,
        end: 0,
        protection: 0,
    };

    fn allows(self, write: bool) -> bool {
        self.protection == READ_WRITE || (!write && self.protection == READ)
    }

    fn permissions(self) -> Option<UserPermissions> {
        match self.protection {
            READ => Some(UserPermissions::READ_ONLY),
            READ_WRITE => Some(UserPermissions::READ_WRITE),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Regions {
    entries: [Region; MAX_REGIONS],
    count: usize,
}

impl Regions {
    const fn new() -> Self {
        Self {
            entries: [Region::EMPTY; MAX_REGIONS],
            count: 0,
        }
    }

    fn as_slice(&self) -> &[Region] {
        &self.entries[..self.count]
    }

    fn push(&mut self, region: Region) -> Result<(), i64> {
        if region.start == region.end {
            return Ok(());
        }
        if self.count != 0 {
            let previous = &mut self.entries[self.count - 1];
            debug_assert!(previous.end <= region.start);
            if previous.end == region.start && previous.protection == region.protection {
                previous.end = region.end;
                return Ok(());
            }
        }
        if self.count == MAX_REGIONS {
            return Err(ENOMEM);
        }
        self.entries[self.count] = region;
        self.count += 1;
        Ok(())
    }

    fn containing(&self, address: u64) -> Option<Region> {
        self.as_slice()
            .iter()
            .copied()
            .find(|region| region.start <= address && address < region.end)
    }

    fn covers(&self, start: u64, end: u64) -> bool {
        let mut cursor = start;
        for region in self.as_slice() {
            if region.end <= cursor {
                continue;
            }
            if region.start > cursor {
                return false;
            }
            cursor = region.end.min(end);
            if cursor == end {
                return true;
            }
        }
        false
    }

    /// None removes the covered interval; Some changes its protection.
    fn edit(&self, start: u64, end: u64, protection: Option<u64>) -> Result<Self, i64> {
        if !self.covers(start, end) {
            return Err(EFAULT);
        }
        let mut next = Self::new();
        for &region in self.as_slice() {
            if region.end <= start || region.start >= end {
                next.push(region)?;
                continue;
            }
            next.push(Region {
                end: region.end.min(start).max(region.start),
                ..region
            })?;
            if let Some(protection) = protection {
                next.push(Region {
                    start: region.start.max(start),
                    end: region.end.min(end),
                    protection,
                })?;
            }
            next.push(Region {
                start: region.start.max(end).min(region.end),
                ..region
            })?;
        }
        Ok(next)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stats {
    pub reserved_bytes: u64,
    pub committed_bytes: u64,
    pub regions: u64,
    /// 0054-D2: the limits this process's class set at creation.
    pub reserved_limit: u64,
    pub committed_limit: u64,
}

pub struct Vm {
    regions: Regions,
    reserved: u64,
    committed: u64,
    /// 0054-D2: this process's limits, set from its memory class at creation.
    reserved_limit: u64,
    committed_limit: u64,
    /// 0054-D4: only a protected class may draw the reserve floor.
    may_draw_reserve: bool,
    // Deterministic physical-allocation failure exercises the same rollback
    // path without temporarily exhausting the machine's entire frame pool.
    #[cfg(feature = "boot-smoke")]
    allocation_budget: Option<usize>,
}

impl Vm {
    pub const fn new(reserved_limit: u64, committed_limit: u64, may_draw_reserve: bool) -> Self {
        Self {
            regions: Regions::new(),
            reserved: 0,
            committed: 0,
            reserved_limit,
            committed_limit,
            may_draw_reserve,
            #[cfg(feature = "boot-smoke")]
            allocation_budget: None,
        }
    }

    pub fn stats(&self) -> Stats {
        Stats {
            reserved_bytes: self.reserved,
            committed_bytes: self.committed,
            regions: self.regions.count as u64,
            reserved_limit: self.reserved_limit,
            committed_limit: self.committed_limit,
        }
    }

    /// Reserve promises only. A nonzero hint requests an exact address and
    /// never replaces an existing reservation, even when permissions agree.
    pub fn reserve(&mut self, hint: u64, length: u64, protection: u64) -> Result<u64, i64> {
        check_protection(protection)?;
        if length == 0 || length & (PAGE_SIZE - 1) != 0 {
            return Err(EINVAL);
        }
        if hint != 0 {
            check_interval(hint, length)?;
        } else if length > ARENA_END - ARENA_START {
            return Err(EINVAL);
        }
        let reserved = self.reserved.checked_add(length).ok_or(ENOMEM)?;
        if reserved > self.reserved_limit {
            return Err(ENOMEM);
        }
        let start = if hint == 0 {
            let mut candidate = ARENA_START;
            for region in self.regions.as_slice() {
                if candidate
                    .checked_add(length)
                    .is_some_and(|end| end <= region.start)
                {
                    break;
                }
                candidate = region.end;
            }
            candidate
        } else {
            hint
        };
        let end = check_interval(start, length)?;
        if self
            .regions
            .as_slice()
            .iter()
            .any(|region| region.start < end && start < region.end)
        {
            return Err(EEXIST);
        }
        let reservation = Region {
            start,
            end,
            protection,
        };
        let mut next = Regions::new();
        let mut inserted = false;
        for &region in self.regions.as_slice() {
            if !inserted && start < region.start {
                next.push(reservation)?;
                inserted = true;
            }
            next.push(region)?;
        }
        if !inserted {
            next.push(reservation)?;
        }
        self.regions = next;
        self.reserved = reserved;
        Ok(start)
    }

    /// Release any completely reserved page interval, including a subrange.
    /// Metadata capacity and holes are checked before any page is withdrawn.
    ///
    /// # Safety
    /// The user's root must be inactive on the sole CPU with interrupts off.
    /// Use its original allocator and exclude live references/DMA to its pages.
    pub unsafe fn release(
        &mut self,
        space: &mut UserAddressSpace,
        frames: &mut FrameAllocator<'_>,
        start: u64,
        length: u64,
    ) -> Result<(), i64> {
        let end = check_interval(start, length)?;
        let next = self.regions.edit(start, end, None)?;
        let mut cursor = start;
        while cursor < end {
            let Some(address) = space
                .next_owned_page(cursor, end)
                .expect("anonymous release encountered invalid owned tables")
            else {
                break;
            };
            // SAFETY: The caller suspended the sole owner; every address is an
            // aligned private anonymous page and allocator ownership is unique.
            let removed = unsafe { space.unmap_page(address, frames) }
                .expect("anonymous release violated paging ownership");
            assert!(removed, "owned anonymous leaf disappeared");
            self.committed -= PAGE_SIZE;
            cursor = address + PAGE_SIZE;
        }
        self.regions = next;
        self.reserved -= length;
        Ok(())
    }

    /// Change promises and existing leaves together. NONE retains physical
    /// backing while denying user access, so restoring access preserves bytes.
    ///
    /// # Safety
    /// The root is inactive on the sole CPU with interrupts disabled. No live
    /// references or DMA may rely on the old permissions during this operation.
    pub unsafe fn protect(
        &mut self,
        space: &mut UserAddressSpace,
        start: u64,
        length: u64,
        protection: u64,
    ) -> Result<(), i64> {
        check_protection(protection)?;
        let end = check_interval(start, length)?;
        let next = self.regions.edit(start, end, Some(protection))?;
        let permissions = Region {
            start,
            end,
            protection,
        }
        .permissions();
        let mut cursor = start;
        while cursor < end {
            let address = match space.next_owned_page(cursor, end) {
                Ok(Some(address)) => address,
                Ok(None) => break,
                Err(error) => {
                    // SAFETY: Restore the same inactive hierarchy's already
                    // modified prefix before reporting the malformed later path.
                    unsafe { self.restore_protection(space, start, cursor) };
                    return Err(paging_error(error));
                }
            };
            // SAFETY: Same inactive, exclusive hierarchy contract as this API.
            if let Err(error) = unsafe { space.protect_page(address, permissions) } {
                // SAFETY: Rollback restores exactly the old ledger permissions
                // on the same suspended hierarchy, excluding the failed leaf.
                unsafe { self.restore_protection(space, start, address) };
                return Err(paging_error(error));
            }
            cursor = address + PAGE_SIZE;
        }
        self.regions = next;
        Ok(())
    }

    /// Caller retains the inactive, exclusive hierarchy used by protect.
    unsafe fn restore_protection(&self, space: &mut UserAddressSpace, start: u64, end: u64) {
        let mut cursor = start;
        while cursor < end {
            let Some(address) = space
                .next_owned_page(cursor, end)
                .expect("anonymous protection rollback traversal")
            else {
                break;
            };
            let permissions = self.regions.containing(address).unwrap().permissions();
            // SAFETY: This is exactly a previously modified owned leaf in the
            // inactive root; the old ledger and its data backing remain intact.
            unsafe { space.protect_page(address, permissions) }
                .expect("anonymous protection rollback failed");
            cursor = address + PAGE_SIZE;
        }
    }

    /// Preflight every page before allocating or copying. Eager ELF/stack
    /// leaves outside the arena and valid anonymous promises may both satisfy
    /// a buffer. True means at least one granted page needs physical backing.
    pub fn preflight(
        &self,
        space: &UserAddressSpace,
        address: u64,
        length: usize,
        write: bool,
    ) -> Result<bool, i64> {
        let Some((first, end)) = user_pages(address, length)? else {
            return Ok(false);
        };
        let mut missing = false;
        for page in (first..end).step_by(PAGE_SIZE as usize) {
            let anonymous = (ARENA_START..ARENA_END).contains(&page);
            if anonymous {
                let region = self.regions.containing(page).ok_or(EFAULT)?;
                if !region.allows(write) {
                    return Err(EACCES);
                }
            }
            match space.translate(page).map_err(paging_error)? {
                Some(translation) if !write || translation.writable => {}
                Some(_) => return Err(EACCES),
                None if anonymous => missing = true,
                None => return Err(EFAULT),
            }
        }
        Ok(missing)
    }

    /// Resolve all granted missing pages in a buffer as one allocation
    /// transaction. On failure, both previously existing contents and region
    /// metadata survive; only newly installed pages/tables are reclaimed.
    ///
    /// # Safety
    /// Run with the kernel root active on the sole CPU and interrupts disabled.
    /// `frames` is this space's allocator; `direct_map` maps all allocatable RAM
    /// writable. No references or DMA may access the user's pages concurrently.
    pub unsafe fn materialize(
        &mut self,
        space: &mut UserAddressSpace,
        frames: &mut FrameAllocator<'_>,
        direct_map: u64,
        address: u64,
        length: usize,
        write: bool,
    ) -> Result<(), VmError> {
        if !self
            .preflight(space, address, length, write)
            .map_err(VmError::from_errno)?
        {
            return Ok(());
        }
        let (first, end) = user_pages(address, length)
            .map_err(VmError::from_errno)?
            .unwrap();
        // A valid contiguous range containing a missing page is anonymous and
        // bounded by this process's reservation quota. This fixed bitmap keeps
        // rollback off the heap and well below the kernel stack limit.
        if first < ARENA_START || end > ARENA_END || end - first > RESERVED_LIMIT {
            return Err(VmError::Fault);
        }
        let mut new_pages = [0_u64; RESIDENT_BITMAP_WORDS];
        let mut count = 0_u64;
        #[cfg(feature = "boot-smoke")]
        let mut installed = 0;
        for (index, page) in (first..end).step_by(PAGE_SIZE as usize).enumerate() {
            if space
                .translate(page)
                .map_err(VmError::from_paging)?
                .is_none()
            {
                new_pages[index / 64] |= 1 << (index % 64);
                count += 1;
            }
        }
        if self.committed + count * PAGE_SIZE > self.committed_limit {
            return Err(VmError::QuotaExceeded);
        }
        // 0054-D4: preflight this operation's worst-case frames — the data
        // pages plus their page-table pages — against the reserve floor. One
        // check per operation, so no partial operation straddles the floor.
        let worst_case = count + count / 512 + 2;
        if !frames.preflight_reserve(worst_case as usize, self.may_draw_reserve) {
            return Err(VmError::NoFrames);
        }
        for (index, page) in (first..end).step_by(PAGE_SIZE as usize).enumerate() {
            if new_pages[index / 64] & (1 << (index % 64)) == 0 {
                continue;
            }
            let result = (|| {
                #[cfg(feature = "boot-smoke")]
                if self
                    .allocation_budget
                    .is_some_and(|budget| installed == budget)
                {
                    return Err(VmError::NoFrames);
                }
                let physical = frames.allocate().ok_or(VmError::NoFrames)?;
                let result = (|| {
                    let destination = direct_map.checked_add(physical).ok_or(VmError::Fault)?;
                    destination
                        .checked_add(PAGE_SIZE - 1)
                        .ok_or(VmError::Fault)?;
                    let permissions = self
                        .regions
                        .containing(page)
                        .unwrap()
                        .permissions()
                        .unwrap();
                    // SAFETY: A fresh, exclusively owned full usable frame is
                    // writable through HHDM; zero before publishing to userspace.
                    unsafe {
                        (destination as *mut u8).write_bytes(0, PAGE_SIZE as usize);
                        space.map_page(page, physical, permissions, frames)
                    }
                    .map_err(VmError::from_paging)
                })();
                if result.is_err() {
                    // SAFETY: map_page transfers ownership only on success;
                    // no user or table mapping owns this rejected fresh frame.
                    unsafe { frames.deallocate(physical) }.expect("unmapped anonymous frame");
                }
                result
            })();
            if let Err(error) = result {
                // Also prune empty tables left by an unsuccessful map_page.
                // SAFETY: No mapped leaf existed at this address before the
                // transaction, and failure never transfers physical ownership.
                let removed = unsafe { space.unmap_page(page, frames) }
                    .expect("failed anonymous map cleanup");
                assert!(!removed);
                for (previous_index, previous) in
                    (first..page).step_by(PAGE_SIZE as usize).enumerate()
                {
                    if new_pages[previous_index / 64] & (1 << (previous_index % 64)) != 0 {
                        // SAFETY: These are exactly this transaction's newly
                        // installed, inactive leaves, with no surviving aliases.
                        let removed = unsafe { space.unmap_page(previous, frames) }
                            .expect("anonymous materialization rollback");
                        assert!(removed);
                    }
                }
                return Err(error);
            }
            #[cfg(feature = "boot-smoke")]
            {
                installed += 1;
            }
        }
        self.committed += count * PAGE_SIZE;
        Ok(())
    }

    /// Only ordinary CPL3 nonpresent data reads/writes may populate a promise.
    /// Protection violations, instruction fetches and reserved-bit faults stay
    /// fatal to the process, including accesses to NONE reservations.
    pub fn permits_fault(&self, address: u64, error: u64) -> bool {
        matches!(error, 4 | 6)
            && self
                .regions
                .containing(address)
                .is_some_and(|region| region.allows(error & 2 != 0))
    }
}

fn check_protection(protection: u64) -> Result<(), i64> {
    if matches!(protection, 0 | READ | READ_WRITE) {
        Ok(())
    } else {
        Err(EINVAL)
    }
}

fn check_interval(start: u64, length: u64) -> Result<u64, i64> {
    let end = start.checked_add(length).ok_or(EINVAL)?;
    if length == 0
        || start & (PAGE_SIZE - 1) != 0
        || length & (PAGE_SIZE - 1) != 0
        || start < ARENA_START
        || end > ARENA_END
    {
        return Err(EINVAL);
    }
    Ok(end)
}

fn user_pages(address: u64, length: usize) -> Result<Option<(u64, u64)>, i64> {
    if !(PAGE_SIZE..USER_END).contains(&address) {
        return Err(EFAULT);
    }
    if length == 0 {
        return Ok(None);
    }
    let last = address.checked_add(length as u64 - 1).ok_or(EFAULT)?;
    if last >= USER_END {
        return Err(EFAULT);
    }
    Ok(Some((
        address & !(PAGE_SIZE - 1),
        (last & !(PAGE_SIZE - 1)) + PAGE_SIZE,
    )))
}

fn paging_error(error: PagingError) -> i64 {
    match error {
        PagingError::OutOfFrames => ENOMEM,
        PagingError::UserAccessDenied => EACCES,
        _ => EFAULT,
    }
}

/// Why a VM operation failed (0054-D5): frame exhaustion is distinct from the
/// per-process quota, so victim selection can tell them apart; both become
/// -12 at the syscall boundary.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VmError {
    /// The process's own reserve/commit limit was reached.
    QuotaExceeded,
    /// The frame pool or the reserve floor could not serve the operation.
    NoFrames,
    /// The address or span is invalid.
    Fault,
    /// The protection or ownership check failed.
    AccessDenied,
}

impl VmError {
    /// The syscall-boundary errno (0054-D5: NoFrames becomes -12 only here).
    pub fn errno(self) -> i64 {
        match self {
            VmError::QuotaExceeded | VmError::NoFrames => ENOMEM,
            VmError::Fault => EFAULT,
            VmError::AccessDenied => EACCES,
        }
    }

    fn from_paging(error: PagingError) -> Self {
        match error {
            PagingError::OutOfFrames => VmError::NoFrames,
            PagingError::UserAccessDenied => VmError::AccessDenied,
            _ => VmError::Fault,
        }
    }

    fn from_errno(code: i64) -> Self {
        match code {
            ENOMEM => VmError::QuotaExceeded,
            EACCES => VmError::AccessDenied,
            _ => VmError::Fault,
        }
    }
}

#[cfg(feature = "boot-smoke")]
fn ledger_self_test() {
    use mirror_vm_protocol::COMMITTED_LIMIT;
    let mut vm = Vm::new(RESERVED_LIMIT, COMMITTED_LIMIT, false);
    let base = vm.reserve(0, 3 * PAGE_SIZE, READ_WRITE).unwrap();
    assert_eq!(base, ARENA_START);
    assert_eq!(vm.stats().committed_bytes, 0);
    let original = vm.regions;
    for (hint, length, protection, error) in [
        (base, PAGE_SIZE, READ_WRITE, EEXIST),
        (base + 1, PAGE_SIZE, READ, EINVAL),
        (base, 1, READ, EINVAL),
        (base, 0, READ, EINVAL),
        (base, PAGE_SIZE, 2, EINVAL),
        (base, PAGE_SIZE, 4, EINVAL),
        (base, PAGE_SIZE, 7, EINVAL),
        (ARENA_END, PAGE_SIZE, READ, EINVAL),
        (u64::MAX - PAGE_SIZE + 1, PAGE_SIZE, READ, EINVAL),
    ] {
        assert_eq!(vm.reserve(hint, length, protection), Err(error));
        assert_eq!(vm.regions, original);
        assert_eq!(vm.stats().reserved_bytes, 3 * PAGE_SIZE);
    }
    let split = vm
        .regions
        .edit(base + PAGE_SIZE, base + 2 * PAGE_SIZE, Some(READ))
        .unwrap();
    assert_eq!(split.count, 3);
    assert_eq!(
        split
            .edit(base, base + 3 * PAGE_SIZE, Some(READ_WRITE))
            .unwrap(),
        original
    );
    let hole = vm
        .regions
        .edit(base + PAGE_SIZE, base + 2 * PAGE_SIZE, None)
        .unwrap();
    assert_eq!(hole.count, 2);
    assert_eq!(
        hole.edit(base, base + 3 * PAGE_SIZE, Some(READ)),
        Err(EFAULT)
    );
    vm.regions = hole;
    vm.reserved -= PAGE_SIZE;
    assert_eq!(vm.reserve(0, PAGE_SIZE, READ_WRITE), Ok(base + PAGE_SIZE));
    assert_eq!(vm.regions, original);
    assert!(vm.permits_fault(base, 4));
    assert!(vm.permits_fault(base, 6));
    for error in [0, 2, 5, 7, 12, 14, 20, 22, 0x8000_0000_0000_0004] {
        assert!(!vm.permits_fault(base, error));
    }
    assert!(!vm.permits_fault(base - 1, 4));
    assert!(!vm.permits_fault(base + 3 * PAGE_SIZE, 4));

    let mut quota = Vm::new(RESERVED_LIMIT, COMMITTED_LIMIT, false);
    quota.reserve(0, RESERVED_LIMIT, READ).unwrap();
    let original = quota.stats();
    assert_eq!(quota.reserve(0, PAGE_SIZE, READ), Err(ENOMEM));
    assert_eq!(quota.stats(), original);
    assert_eq!(quota.reserve(0, ARENA_END, READ), Err(EINVAL));

    let mut full = Vm::new(RESERVED_LIMIT, COMMITTED_LIMIT, false);
    for index in 0..MAX_REGIONS {
        full.reserve(
            base + index as u64 * 4 * PAGE_SIZE,
            3 * PAGE_SIZE,
            READ_WRITE,
        )
        .unwrap();
    }
    let original = full.regions;
    assert_eq!(
        full.regions
            .edit(base + PAGE_SIZE, base + 2 * PAGE_SIZE, None),
        Err(ENOMEM)
    );
    assert_eq!(
        full.regions
            .edit(base + PAGE_SIZE, base + 2 * PAGE_SIZE, Some(READ)),
        Err(ENOMEM)
    );
    assert_eq!(full.reserve(0, PAGE_SIZE, READ), Err(ENOMEM));
    assert_eq!(full.regions, original);
    // Filling a gap with identical protection coalesces its neighbors, freeing
    // a descriptor even though the table was full before the reservation.
    assert_eq!(
        full.reserve(base + 3 * PAGE_SIZE, PAGE_SIZE, READ_WRITE),
        Ok(base + 3 * PAGE_SIZE)
    );
    assert_eq!(full.stats().regions, (MAX_REGIONS - 1) as u64);
}

/// Validate real owned page tables, frame rollback, protection and isolation.
#[cfg(feature = "boot-smoke")]
pub fn self_test(
    kernel: &crate::paging::PageTables,
    frames: &mut FrameAllocator<'_>,
    direct_map: u64,
) {
    use mirror_vm_protocol::COMMITTED_LIMIT;
    ledger_self_test();
    let baseline = frames.stats().unwrap().allocated_frames;
    // SAFETY: run_init invokes this with the kernel root active, IF=0, and no
    // active user. Both local roots remain inactive, never escape this function,
    // use the same unique allocator and borrow the live kernel HHDM throughout.
    unsafe {
        let mut space = UserAddressSpace::new(kernel, frames).unwrap();
        let mut vm = Vm::new(RESERVED_LIMIT, COMMITTED_LIMIT, false);
        let base = vm.reserve(0, 4 * PAGE_SIZE, READ_WRITE).unwrap();
        let unmapped_frames = frames.stats().unwrap().allocated_frames;
        assert_eq!(
            vm.preflight(&space, base, PAGE_SIZE as usize, true),
            Ok(true)
        );
        assert_eq!(
            vm.preflight(&space, base, 5 * PAGE_SIZE as usize, true),
            Err(EFAULT)
        );
        assert_eq!(vm.preflight(&space, base, usize::MAX, true), Err(EFAULT));
        assert_eq!(vm.preflight(&space, 0, 0, false), Err(EFAULT));
        assert_eq!(vm.preflight(&space, PAGE_SIZE, 0, true), Ok(false));
        assert_eq!(frames.stats().unwrap().allocated_frames, unmapped_frames);
        vm.materialize(
            &mut space,
            frames,
            direct_map,
            base + PAGE_SIZE - 16,
            32,
            true,
        )
        .unwrap();
        assert_eq!(vm.stats().committed_bytes, 2 * PAGE_SIZE);
        let mut bytes = [0x55; 32];
        space
            .copy_from_user(base + PAGE_SIZE - 16, &mut bytes)
            .unwrap();
        assert_eq!(bytes, [0; 32]);
        let pattern = [0xa7; 32];
        space.copy_to_user(base + PAGE_SIZE - 16, &pattern).unwrap();

        vm.protect(&mut space, base, PAGE_SIZE, 0).unwrap();
        assert_eq!(vm.preflight(&space, base, 1, false), Err(EACCES));
        assert_eq!(space.translate(base), Err(PagingError::UserAccessDenied));
        assert_eq!(vm.stats().committed_bytes, 2 * PAGE_SIZE);
        assert!(!vm.permits_fault(base, 4));
        vm.protect(&mut space, base, PAGE_SIZE, READ).unwrap();
        assert_eq!(vm.preflight(&space, base, 1, true), Err(EACCES));
        space
            .copy_from_user(base + PAGE_SIZE - 16, &mut bytes)
            .unwrap();
        assert_eq!(bytes, pattern);
        vm.protect(&mut space, base, PAGE_SIZE, READ_WRITE).unwrap();
        assert_eq!(vm.stats().regions, 1);

        let readonly = vm.reserve(base + 4 * PAGE_SIZE, PAGE_SIZE, READ).unwrap();
        let before = frames.stats().unwrap().allocated_frames;
        assert_eq!(
            vm.materialize(&mut space, frames, direct_map, readonly, 1, true),
            Err(VmError::AccessDenied)
        );
        assert_eq!(frames.stats().unwrap().allocated_frames, before);
        vm.materialize(&mut space, frames, direct_map, readonly, 1, false)
            .unwrap();
        assert!(!space.translate(readonly).unwrap().unwrap().writable);
        let none = vm.reserve(base + 5 * PAGE_SIZE, PAGE_SIZE, 0).unwrap();
        assert!(!vm.permits_fault(none, 4));
        vm.protect(&mut space, none, PAGE_SIZE, READ).unwrap();
        assert!(vm.permits_fault(none, 4));
        assert!(!vm.permits_fault(none, 6));

        let before = frames.stats().unwrap().allocated_frames;
        let original_regions = vm.regions;
        let original_stats = vm.stats();
        assert_eq!(
            vm.protect(&mut space, base, 7 * PAGE_SIZE, READ),
            Err(EFAULT)
        );
        assert_eq!(
            vm.release(&mut space, frames, base, 7 * PAGE_SIZE),
            Err(EFAULT)
        );
        assert_eq!(vm.regions, original_regions);
        assert_eq!(vm.stats(), original_stats);
        assert_eq!(frames.stats().unwrap().allocated_frames, before);
        assert!(space.translate(base).unwrap().unwrap().writable);

        // Trigger an allocation failure only after pages on opposite sides of
        // a one-GiB paging boundary exist, requiring leaf and table rollback.
        let rollback = vm
            .reserve(base + (1 << 30) - PAGE_SIZE, 3 * PAGE_SIZE, READ_WRITE)
            .unwrap();
        let before = frames.stats().unwrap().allocated_frames;
        let before_tables = space.owned_table_count();
        let before_stats = vm.stats();
        vm.allocation_budget = Some(2);
        assert_eq!(
            vm.materialize(
                &mut space,
                frames,
                direct_map,
                rollback,
                3 * PAGE_SIZE as usize,
                true
            ),
            Err(VmError::NoFrames)
        );
        vm.allocation_budget = None;
        assert_eq!(vm.stats(), before_stats);
        assert_eq!(frames.stats().unwrap().allocated_frames, before);
        assert_eq!(space.owned_table_count(), before_tables);
        for page in (rollback..rollback + 3 * PAGE_SIZE).step_by(PAGE_SIZE as usize) {
            assert!(space.translate(page).unwrap().is_none());
        }
        space
            .copy_from_user(base + PAGE_SIZE - 16, &mut bytes)
            .unwrap();
        assert_eq!(bytes, pattern);
        vm.materialize(
            &mut space,
            frames,
            direct_map,
            rollback,
            3 * PAGE_SIZE as usize,
            true,
        )
        .unwrap();
        vm.protect(&mut space, rollback + PAGE_SIZE, PAGE_SIZE, 0)
            .unwrap();
        vm.release(&mut space, frames, rollback, 3 * PAGE_SIZE)
            .unwrap();
        assert_eq!(frames.stats().unwrap().allocated_frames, before);
        assert_eq!(space.owned_table_count(), before_tables);

        // A quota failure happens before the first page/table allocation.
        let quota = vm.reserve(0, COMMITTED_LIMIT, READ_WRITE).unwrap();
        let before = frames.stats().unwrap().allocated_frames;
        assert_eq!(
            vm.materialize(
                &mut space,
                frames,
                direct_map,
                quota,
                COMMITTED_LIMIT as usize,
                true
            ),
            Err(VmError::QuotaExceeded)
        );
        assert_eq!(frames.stats().unwrap().allocated_frames, before);
        assert!(space.translate(quota).unwrap().is_none());
        vm.release(&mut space, frames, quota, COMMITTED_LIMIT)
            .unwrap();

        let mut sibling = UserAddressSpace::new(kernel, frames).unwrap();
        let mut sibling_vm = Vm::new(RESERVED_LIMIT, COMMITTED_LIMIT, false);
        sibling_vm.reserve(base, 2 * PAGE_SIZE, READ_WRITE).unwrap();
        sibling_vm
            .materialize(
                &mut sibling,
                frames,
                direct_map,
                base,
                2 * PAGE_SIZE as usize,
                true,
            )
            .unwrap();
        sibling
            .copy_to_user(base + PAGE_SIZE - 16, &[0x19; 32])
            .unwrap();
        assert_ne!(
            space.translate(base).unwrap().unwrap().physical,
            sibling.translate(base).unwrap().unwrap().physical
        );
        space
            .copy_from_user(base + PAGE_SIZE - 16, &mut bytes)
            .unwrap();
        assert_eq!(bytes, pattern);
        sibling.destroy(frames).unwrap();

        // A full-size sparse reservation should edit only its two resident
        // leaves, preserving hidden data and reclaiming their sparse tables.
        let before_sparse = frames.stats().unwrap().allocated_frames;
        let mut sparse_space = UserAddressSpace::new(kernel, frames).unwrap();
        let mut sparse_vm = Vm::new(RESERVED_LIMIT, COMMITTED_LIMIT, false);
        let sparse = sparse_vm.reserve(0, RESERVED_LIMIT, READ_WRITE).unwrap();
        let last = sparse + RESERVED_LIMIT - PAGE_SIZE;
        for (address, value) in [(sparse, 0x41), (last, 0x52)] {
            sparse_vm
                .materialize(&mut sparse_space, frames, direct_map, address, 1, true)
                .unwrap();
            sparse_space.copy_to_user(address, &[value]).unwrap();
        }
        sparse_vm
            .protect(&mut sparse_space, sparse, RESERVED_LIMIT, 0)
            .unwrap();
        assert_eq!(sparse_vm.stats().committed_bytes, 2 * PAGE_SIZE);
        sparse_vm
            .protect(&mut sparse_space, sparse, RESERVED_LIMIT, READ)
            .unwrap();
        for (address, value) in [(sparse, 0x41), (last, 0x52)] {
            sparse_space
                .copy_from_user(address, &mut bytes[..1])
                .unwrap();
            assert_eq!(bytes[0], value);
        }
        sparse_vm
            .release(&mut sparse_space, frames, sparse, RESERVED_LIMIT)
            .unwrap();
        assert_eq!(
            sparse_vm.stats(),
            Vm::new(RESERVED_LIMIT, COMMITTED_LIMIT, false).stats()
        );
        assert_eq!(sparse_space.owned_page_count(), 0);
        assert_eq!(sparse_space.owned_table_count(), 1);
        sparse_space.destroy(frames).unwrap();
        assert_eq!(frames.stats().unwrap().allocated_frames, before_sparse);

        // Partial release removes only its page. Reusing its virtual address
        // must expose zeroes even when the allocator recycles the same frame.
        vm.release(&mut space, frames, base, PAGE_SIZE).unwrap();
        assert!(space.translate(base).unwrap().is_none());
        assert!(space.translate(base + PAGE_SIZE).unwrap().is_some());
        assert!(!vm.permits_fault(base, 4));
        assert_eq!(vm.reserve(0, PAGE_SIZE, READ_WRITE), Ok(base));
        vm.materialize(
            &mut space,
            frames,
            direct_map,
            base,
            PAGE_SIZE as usize,
            true,
        )
        .unwrap();
        space
            .copy_from_user(base + PAGE_SIZE - 16, &mut bytes[..16])
            .unwrap();
        assert_eq!(bytes[..16], [0; 16]);
        space
            .copy_from_user(base + PAGE_SIZE, &mut bytes[..16])
            .unwrap();
        assert_eq!(bytes[..16], [0xa7; 16]);
        // Destroy must also reclaim a resident inaccessible anonymous leaf.
        vm.protect(&mut space, base, PAGE_SIZE, 0).unwrap();
        space.destroy(frames).unwrap();
    }
    assert_eq!(frames.stats().unwrap().allocated_frames, baseline);
    crate::serial::println!("MIRROR_VM_TESTS_OK");
}

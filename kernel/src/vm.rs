//! Per-process anonymous virtual memory. Region edits are planned before page
//! tables change, and only the BSP changes them, while sibling threads may
//! run the space on other CPUs (spec 0051, points 7 and 8). A page-in takes
//! every frame it needs before it publishes anything. A release or protection
//! change updates the ledger first, then changes resident leaves in batches;
//! each batch's CPUs flush before its frames are freed. Release/protect skip
//! absent paging subtrees. Resident work remains bounded by 8192 pages per
//! process.

use crate::{
    memory::{FrameAllocator, PAGE_SIZE},
    paging::{
        FrameReserve, PagingError, UserAddressSpace, UserPermissions, WITHDRAW_BATCH, Withdrawal,
    },
    smp,
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

    /// Begin releasing a completely reserved page interval, including a
    /// subrange. Metadata capacity and holes are checked first; then the
    /// ledger drops the interval, so no new access, wait or page-in can
    /// start in it, and `step` withdraws its resident pages.
    pub fn begin_release(&mut self, start: u64, length: u64) -> Result<Removal, i64> {
        let end = check_interval(start, length)?;
        self.regions = self.regions.edit(start, end, None)?;
        self.reserved -= length;
        Ok(Removal {
            cursor: start,
            end,
            change: None,
        })
    }

    /// Begin changing promises and existing leaves together. The interval and
    /// every resident leaf in it are checked first, so the change cannot fail
    /// partway; then the ledger changes and `step` changes the leaves. NONE
    /// retains physical backing while denying user access, so restoring
    /// access preserves bytes.
    pub fn begin_protect(
        &mut self,
        space: &UserAddressSpace,
        start: u64,
        length: u64,
        protection: u64,
    ) -> Result<Removal, i64> {
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
            let Some(address) = space.next_owned_page(cursor, end).map_err(paging_error)? else {
                break;
            };
            space
                .check_protect(address, permissions)
                .map_err(paging_error)?;
            cursor = address + PAGE_SIZE;
        }
        self.regions = next;
        Ok(Removal {
            cursor: start,
            end,
            change: Some(permissions),
        })
    }

    /// Withdraw or change the removal's next batch of resident leaves, at
    /// most WITHDRAW_BATCH entries, and return how many leaves changed. The
    /// caller then reads the space's shootdown targets and passes them, with
    /// the batch, to `finish_batch` (spec 0051, point 8).
    ///
    /// # Safety
    /// Run on the BSP with IF=0, on the Vm and space the removal began on;
    /// this CPU does not run the space. No live reference or DMA may access
    /// its pages or rely on their old permissions.
    pub unsafe fn step(
        &mut self,
        removal: &mut Removal,
        space: &mut UserAddressSpace,
        batch: &mut Withdrawal,
    ) -> usize {
        let mut changed = 0;
        while removal.cursor < removal.end && batch.has_room() && changed < WITHDRAW_BATCH {
            let Some(address) = space
                .next_owned_page(removal.cursor, removal.end)
                .expect("anonymous removal encountered invalid owned tables")
            else {
                removal.cursor = removal.end;
                break;
            };
            match removal.change {
                None => {
                    // SAFETY: The caller's contract; every address is an
                    // aligned private anonymous page.
                    let removed = unsafe { space.withdraw_page(address, batch) }
                        .expect("anonymous release violated paging ownership");
                    assert!(removed, "owned anonymous leaf disappeared");
                    self.committed -= PAGE_SIZE;
                }
                // SAFETY: As above; begin_protect checked this leaf, and only
                // this removal has changed the tables since.
                Some(permissions) => unsafe { space.protect_page(address, permissions) }
                    .expect("checked anonymous protection change"),
            }
            changed += 1;
            removal.cursor = address + PAGE_SIZE;
        }
        changed
    }

    /// Release a completely reserved interval, flushing each batch's CPUs
    /// before its frames are freed. The scheduler drives a process's release
    /// batch by batch instead, so it can release its lock for each flush.
    ///
    /// # Safety
    /// As for `step` and `finish_batch`.
    #[cfg(feature = "boot-smoke")]
    pub unsafe fn release(
        &mut self,
        space: &mut UserAddressSpace,
        frames: &mut FrameAllocator<'_>,
        start: u64,
        length: u64,
    ) -> Result<(), i64> {
        let mut removal = self.begin_release(start, length)?;
        while !removal.is_done() {
            let mut batch = Withdrawal::empty();
            // SAFETY: The caller's contract.
            let changed = unsafe { self.step(&mut removal, space, &mut batch) };
            let targets = if changed == 0 {
                0
            } else {
                space.shootdown_targets()
            };
            // SAFETY: The targets were read after the batch's withdrawals.
            unsafe { finish_batch(space.root_physical_address(), targets, &mut batch, frames) };
        }
        Ok(())
    }

    /// Change promises and existing leaves together, flushing each batch's
    /// CPUs. The scheduler drives a process's change batch by batch instead.
    ///
    /// # Safety
    /// As for `step` and `finish_batch`.
    #[cfg(feature = "boot-smoke")]
    pub unsafe fn protect(
        &mut self,
        space: &mut UserAddressSpace,
        start: u64,
        length: u64,
        protection: u64,
    ) -> Result<(), i64> {
        let mut removal = self.begin_protect(space, start, length, protection)?;
        while !removal.is_done() {
            let mut batch = Withdrawal::empty();
            // SAFETY: The caller's contract.
            if unsafe { self.step(&mut removal, space, &mut batch) } != 0 {
                flush(space.root_physical_address(), space.shootdown_targets());
            }
            assert!(batch.is_empty(), "a protection change withdrew frames");
        }
        Ok(())
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
    /// transaction. Every frame it needs, data pages and new tables alike, is
    /// taken before anything is published, so a failure leaves nothing that a
    /// sibling on another CPU could have seen (spec 0051, point 7). On
    /// failure, both previously existing contents and region metadata survive.
    ///
    /// # Safety
    /// Run on the BSP with IF=0; this CPU does not run the space. `frames` is
    /// this space's allocator; `direct_map` maps all allocatable RAM writable.
    /// No DMA may access the user's pages.
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
        // the transaction off the heap and well below the kernel stack limit.
        if first < ARENA_START || end > ARENA_END || end - first > RESERVED_LIMIT {
            return Err(VmError::Fault);
        }
        let mut new_pages = [0_u64; RESIDENT_BITMAP_WORDS];
        let mut count = 0_u64;
        let mut tables = 0;
        let mut previous = None;
        for (index, page) in (first..end).step_by(PAGE_SIZE as usize).enumerate() {
            if space
                .translate(page)
                .map_err(VmError::from_paging)?
                .is_none()
            {
                new_pages[index / 64] |= 1 << (index % 64);
                count += 1;
                tables += space
                    .missing_tables(page, previous)
                    .map_err(VmError::from_paging)?;
                previous = Some(page);
            }
        }
        if self.committed + count * PAGE_SIZE > self.committed_limit {
            return Err(VmError::QuotaExceeded);
        }
        // 0054-D4: preflight this operation's frames against the reserve
        // floor; one check per operation, so no partial operation straddles
        // the floor. The walk above counted exactly the data pages and the
        // page tables their paths lack (spec 0051, point 7), which is the
        // operation's worst case.
        if !frames.preflight_reserve(count as usize + tables, self.may_draw_reserve) {
            return Err(VmError::NoFrames);
        }
        // Deterministic allocation failure (boot-smoke) stops the reservation
        // after this many frames, through the same rollback as exhaustion.
        #[cfg(feature = "boot-smoke")]
        let budget = self.allocation_budget.unwrap_or(usize::MAX);
        #[cfg(not(feature = "boot-smoke"))]
        let budget = usize::MAX;
        let mut reserve = FrameReserve::empty();
        let mut reserved = Ok(());
        for taken in 0..count as usize + tables {
            if taken == budget {
                reserved = Err(VmError::NoFrames);
                break;
            }
            if let Err(error) = space.reserve_frame(&mut reserve, frames) {
                reserved = Err(VmError::from_paging(error));
                break;
            }
        }
        if let Err(error) = reserved {
            // SAFETY: Nothing was published; the reserve alone owns its frames.
            unsafe { space.release_reserve(&mut reserve, frames) };
            return Err(error);
        }
        for (index, page) in (first..end).step_by(PAGE_SIZE as usize).enumerate() {
            if new_pages[index / 64] & (1 << (index % 64)) == 0 {
                continue;
            }
            let physical = space
                .take_reserved(&mut reserve)
                .expect("reserved anonymous page");
            let destination = direct_map
                .checked_add(physical)
                .expect("reserved frame direct map");
            let permissions = self
                .regions
                .containing(page)
                .unwrap()
                .permissions()
                .unwrap();
            // SAFETY: A reserved, exclusively owned full usable frame, writable
            // through the HHDM; it is zeroed before its Release publication.
            // The reserve holds every table its path lacks, and the walks above
            // checked that path, so nothing here can fail partway.
            unsafe {
                (destination as *mut u8).write_bytes(0, PAGE_SIZE as usize);
                space.map_reserved(page, physical, permissions, &mut reserve)
            }
            .expect("reserved anonymous mapping");
        }
        assert!(reserve.is_empty(), "a page-in reserved unused frames");
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

/// A release or protection change whose ledger change is made, with its
/// resident leaves still to change, one batch per `Vm::step` (spec 0051,
/// point 8).
pub struct Removal {
    cursor: u64,
    end: u64,
    /// None withdraws the pages; Some gives them these permissions.
    change: Option<Option<UserPermissions>>,
}

impl Removal {
    pub fn is_done(&self) -> bool {
        self.cursor >= self.end
    }
}

/// Make every CPU in `targets` drop its translations through `root`, and
/// return how many had the root loaded. No targets, no IPI.
fn flush(root: u64, targets: u64) -> usize {
    if targets == 0 {
        0
    } else {
        smp::flush_user(root, targets)
    }
}

/// End one removal batch (spec 0051, point 8): flush every CPU in `targets`,
/// then free the batch's frames. Returns how many targets had the root
/// loaded. With targets, the caller holds no scheduler lock: the flush waits
/// for other CPUs.
///
/// # Safety
/// Run on the BSP with IF=0 and the allocator the frames came from.
/// `targets` was read from the space's `shootdown_targets` after every
/// withdrawal in the batch.
pub unsafe fn finish_batch(
    root: u64,
    targets: u64,
    batch: &mut Withdrawal,
    frames: &mut FrameAllocator<'_>,
) -> usize {
    let reloaded = flush(root, targets);
    // SAFETY: Every CPU that could translate through the batch has flushed.
    unsafe { batch.release(frames) };
    reloaded
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

        // Pages on both sides of a one-GiB paging boundary need three data
        // frames and three tables (a PT, then a PD and its PT). The page-in
        // takes all six before it publishes anything (spec 0051, point 7), so
        // a failure at the third frame publishes nothing and only returns the
        // two reserved frames.
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
        assert_eq!(frames.stats().unwrap().allocated_frames, before + 6);
        assert_eq!(space.owned_table_count(), before_tables + 3);
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

/// A scratch space for the user flush tests: one anonymous page holding
/// `value` in its first word, kept as a supervisor-only leaf (PROT_NONE), so
/// an AP can read it with SMAP on. Returns the space, its ledger, the page's
/// address and its frame.
#[cfg(feature = "boot-smoke")]
fn flush_test_space(
    kernel: &crate::paging::PageTables,
    frames: &mut FrameAllocator<'_>,
    direct_map: u64,
    value: u64,
) -> (UserAddressSpace, Vm, u64, u64) {
    // SAFETY: The BSP with IF=0 and the kernel root active, with the unique
    // allocator. The caller destroys the root, or ends the boot.
    let mut space = unsafe { UserAddressSpace::new(kernel, frames) }.expect("user flush test root");
    let mut vm = Vm::new(RESERVED_LIMIT, mirror_vm_protocol::COMMITTED_LIMIT, false);
    let base = vm.reserve(0, PAGE_SIZE, READ_WRITE).unwrap();
    // SAFETY: As above; no CPU has the root loaded yet.
    unsafe {
        vm.materialize(&mut space, frames, direct_map, base, 8, true)
            .unwrap();
        space.copy_to_user(base, &value.to_le_bytes()).unwrap();
    }
    let frame = space.translate(base).unwrap().unwrap().physical;
    // SAFETY: As above.
    unsafe { vm.protect(&mut space, base, PAGE_SIZE, 0) }.unwrap();
    (space, vm, base, frame)
}

/// Spec 0051 step 2: a release from a root that every online AP has loaded
/// flushes each AP before its frames return, each proving its CR3 reload, and
/// each AP then reads the page's new frame. A flush target that already left
/// the root only acknowledges, and destroy refuses while any CPU has the root
/// loaded. QEMU's TCG drops an AP's cached translations between commands, so
/// there a stale read cannot show a missing flush; the reload proofs do, and
/// the step 5 battery's canary checks with threads running. With one CPU
/// online the release runs the same path with nothing to flush.
#[cfg(feature = "boot-smoke")]
pub fn user_flush_self_test(
    kernel: &crate::paging::PageTables,
    frames: &mut FrameAllocator<'_>,
    direct_map: u64,
) {
    const OLD: u64 = 0x4d49_5252_4f52_0001;
    const NEW: u64 = 0x4d49_5252_4f52_0002;
    let baseline = frames.stats().unwrap().allocated_frames;
    let aps = smp::online_aps();
    let (mut space, mut vm, base, old) = flush_test_space(kernel, frames, direct_map, OLD);
    let root = space.root_physical_address();
    // The replacement is taken while the old frame is still allocated, so
    // the two differ: an AP that kept a stale entry would read the old one.
    let replacement = frames.allocate().expect("user flush replacement frame");
    let pointer = direct_map
        .checked_add(replacement)
        .expect("replacement direct map") as *mut u64;
    // SAFETY: A fresh, exclusively owned frame that the direct map covers.
    unsafe {
        pointer.cast::<u8>().write_bytes(0, PAGE_SIZE as usize);
        pointer.write(NEW);
    }
    space.note_loaded_for_test(aps, true);
    smp::enter_user_root_for_test(aps, root, base, OLD);
    assert_eq!(space.loaded_cpus(), aps);
    if aps != 0 {
        // SAFETY: Destroy must refuse before it frees anything.
        let refused = unsafe { space.destroy(frames) };
        assert_eq!(refused, Err(PagingError::StillActive));
    }
    // The removal path itself: one batch withdraws the leaf and its three
    // tables, which the release empties.
    let mut removal = vm.begin_release(base, PAGE_SIZE).unwrap();
    let mut batch = Withdrawal::empty();
    // SAFETY: The BSP with IF=0 does not run the scratch root, and nothing
    // else uses its page.
    assert_eq!(unsafe { vm.step(&mut removal, &mut space, &mut batch) }, 1);
    assert!(removal.is_done());
    assert_eq!(batch.frames().len(), 4);
    assert_eq!(batch.frames()[0], old);
    let targets = space.shootdown_targets();
    assert_eq!(targets, aps);
    // SAFETY: The targets were read after the withdrawal.
    let reloaded = unsafe { finish_batch(root, targets, &mut batch, frames) };
    assert_eq!(
        reloaded,
        aps.count_ones() as usize,
        "a CPU with the root loaded skipped its flush"
    );
    // SAFETY: As above; the replacement is initialized and owned, and becomes
    // supervisor-only like the old page.
    unsafe {
        space
            .map_page(base, replacement, UserPermissions::READ_WRITE, frames)
            .unwrap();
        space.protect_page(base, None).unwrap();
    }
    smp::enter_user_root_for_test(aps, root, base, NEW);
    // The last AP leaves the root; a flush still sent to it only acknowledges.
    let mut skipped = 0;
    if aps != 0 {
        let last = 1 << (63 - aps.leading_zeros());
        smp::leave_user_root_for_test(last);
        space.note_loaded_for_test(last, false);
        assert_eq!(
            smp::flush_user(root, aps),
            aps.count_ones() as usize - 1,
            "a CPU without the root reloaded it"
        );
        skipped = 1;
    }
    let loaded = space.loaded_cpus();
    smp::leave_user_root_for_test(loaded);
    space.note_loaded_for_test(loaded, false);
    assert_eq!(space.loaded_cpus(), 0);
    // SAFETY: No CPU has the root loaded, and its pages have no other users.
    unsafe { space.destroy(frames) }.expect("user flush test root teardown");
    assert_eq!(frames.stats().unwrap().allocated_frames, baseline);
    crate::serial::println!(
        "MIRROR_USER_FLUSH_TESTS_OK cpus={} reloaded={} skipped={}",
        aps.count_ones() + 1,
        reloaded,
        skipped
    );
}

/// The UserFlushTimeout fault (spec 0051, step 2): the last AP withholds its
/// OP_FLUSH_USER acknowledgement, so the release's flush must panic before
/// the batch's frames reach the allocator. This boot ends there.
#[cfg(feature = "boot-smoke")]
pub fn user_flush_timeout_self_test(
    kernel: &crate::paging::PageTables,
    frames: &mut FrameAllocator<'_>,
    direct_map: u64,
) -> ! {
    const OLD: u64 = 0x4d49_5252_4f52_0003;
    let aps = smp::online_aps();
    assert_ne!(aps, 0, "the user flush timeout needs an online AP");
    let (mut space, mut vm, base, old) = flush_test_space(kernel, frames, direct_map, OLD);
    let root = space.root_physical_address();
    space.note_loaded_for_test(aps, true);
    smp::enter_user_root_for_test(aps, root, base, OLD);
    smp::arm_user_flush_timeout_for_test();
    let mut removal = vm.begin_release(base, PAGE_SIZE).unwrap();
    let mut batch = Withdrawal::empty();
    // SAFETY: The BSP with IF=0 does not run the scratch root, and nothing
    // else uses its page.
    assert_eq!(unsafe { vm.step(&mut removal, &mut space, &mut batch) }, 1);
    let targets = space.shootdown_targets();
    crate::serial::println!(
        "MIRROR_USER_FLUSH_TIMEOUT_EXPECT root={:#x} frame={:#x}",
        root,
        old
    );
    // SAFETY: The targets were read after the withdrawal. The withheld ACK
    // must stop this call before it frees the batch.
    unsafe { finish_batch(root, targets, &mut batch, frames) };
    crate::serial::println!("MIRROR_USER_FLUSH_TIMEOUT_UNEXPECTED_RETURN");
    panic!("a user shootdown returned without every acknowledgement");
}

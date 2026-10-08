//! One synchronous native process on the BSP. No scheduling or handles yet.
//! User buffers are checked and copied through the direct map before use.

use core::sync::atomic::{AtomicPtr, Ordering};

use crate::{
    arch, elf,
    interrupts::{self, TrapFrame},
    memory::{FrameAllocator, PAGE_SIZE},
    paging::{PageTables, PagingError, UserAddressSpace, UserPermissions},
    serial, timer,
};

const STACK_BASE: u64 = 0x7fff_0000;
const STACK_TOP: u64 = 0x8000_0000;
const KERNEL_STACK_SIZE: usize = 64 * 1024;
const MAX_WRITE: usize = 256;
const CHECKS_PASSED: &[u8] = b"Mirror init: userspace checks passed\n";

#[repr(C, align(16))]
struct KernelStack([u8; KERNEL_STACK_SIZE]);
static mut KERNEL_STACK: KernelStack = KernelStack([0; KERNEL_STACK_SIZE]);

// Only the BSP accesses this slot, with IF=0. It is populated immediately before
// entry and cleared after the synchronous return; it never owns the context.
static ACTIVE: AtomicPtr<()> = AtomicPtr::new(core::ptr::null_mut());

#[derive(Debug, PartialEq, Eq)]
enum Outcome {
    Exit(u64),
    Fault {
        vector: u64,
        error: u64,
        address: u64,
        rip: u64,
    },
    InvalidStack(u64),
}

struct Process<'kernel> {
    space: UserAddressSpace<'kernel>,
    outcome: Option<Outcome>,
    checks_passed: bool,
    bad_pointers: usize,
    unknown_calls: usize,
}

pub fn run_init(
    kernel: &PageTables,
    frames: &mut FrameAllocator<'_>,
    bytes: &[u8],
    direct_map: u64,
    scenario: u64,
) {
    assert!(!arch::interrupts_enabled());
    let image = elf::parse(bytes).expect("invalid native init ELF");
    #[cfg(feature = "boot-smoke")]
    test_load_rollback(kernel, frames, bytes, &image, direct_map);
    run_one(kernel, frames, bytes, &image, direct_map, scenario);
    #[cfg(feature = "boot-smoke")]
    // Relaunch from the same ELF after teardown. Fresh BSS and successful entry
    // exercise frame reuse and continuation recovery after both exit and fault.
    run_one(kernel, frames, bytes, &image, direct_map, 0);
}

#[cfg(feature = "boot-smoke")]
fn test_load_rollback(
    kernel: &PageTables,
    frames: &mut FrameAllocator<'_>,
    bytes: &[u8],
    image: &elf::Image,
    direct_map: u64,
) {
    let before = frames.stats().unwrap().allocated_frames;
    // SAFETY: Same exclusive, inactive process construction as run_one.
    let mut space = unsafe { UserAddressSpace::new(kernel, frames) }.unwrap();
    // Force a real map failure after ELF pages have already been installed.
    map_initialized(
        &mut space,
        frames,
        direct_map,
        STACK_BASE,
        UserPermissions::READ_WRITE,
        &[],
    )
    .unwrap();
    assert_eq!(
        load(&mut space, frames, bytes, image, direct_map),
        Err(PagingError::AlreadyMapped)
    );
    assert!(
        space.owned_page_count() > 1,
        "load failed before installing image pages"
    );
    // SAFETY: This root has never been active and no references escaped load.
    unsafe { space.destroy(frames) }.unwrap();
    assert_eq!(
        frames.stats().unwrap().allocated_frames,
        before,
        "partial load leaked"
    );
    serial::println!("MIRROR_USER_LOAD_ROLLBACK_OK");
}

fn run_one(
    kernel: &PageTables,
    frames: &mut FrameAllocator<'_>,
    bytes: &[u8],
    image: &elf::Image,
    direct_map: u64,
    scenario: u64,
) {
    let before = frames.stats().expect("recycling required").allocated_frames;
    // SAFETY: The BSP is alone with IF=0, kernel root active, and the shared
    // upper mappings and unique allocator remain live throughout this run.
    let space = unsafe { UserAddressSpace::new(kernel, frames) }.expect("user page tables");
    let mut process = Process {
        space,
        outcome: None,
        checks_passed: false,
        bad_pointers: 0,
        unknown_calls: 0,
    };
    if let Err(error) = load(&mut process.space, frames, bytes, image, direct_map) {
        // SAFETY: This root has never been active; mapped frames belong to it.
        unsafe { process.space.destroy(frames) }.expect("failed-load cleanup");
        assert_eq!(frames.stats().unwrap().allocated_frames, before);
        panic!("native init load failed: {error:?}");
    }
    serial::println!(
        "Native init: entry={:#x} root={:#x} pages={} tables={}",
        image.entry,
        process.space.root_physical_address(),
        process.space.owned_page_count(),
        process.space.owned_table_count()
    );
    let stack_top = (&raw const KERNEL_STACK) as u64 + KERNEL_STACK_SIZE as u64;
    assert!(ACTIVE.load(Ordering::SeqCst).is_null());
    // SAFETY: Dedicated static upper-half stack, IF=0, no active user process.
    unsafe { interrupts::set_kernel_stack(stack_top) };
    ACTIVE.store((&raw mut process).cast(), Ordering::SeqCst);
    // SAFETY: Loaded pages and the stack obey the validated user layout. The
    // upper mappings cover our suspended stack, handler stack and all Rust data.
    // No locks/interrupt guards span entry; handlers return before this resumes.
    unsafe {
        process.space.activate().expect("activate user root");
        interrupts::enter_user(image.entry, STACK_TOP - 8, scenario);
    }
    ACTIVE.store(core::ptr::null_mut(), Ordering::SeqCst);
    // SAFETY: User execution has ended, IF=0, and no user pointers survive the
    // handlers. Restore the owner before releasing the inactive private tree.
    unsafe {
        kernel.activate().expect("restore kernel root");
        process
            .space
            .destroy(frames)
            .expect("release user address space");
    }
    assert_eq!(
        frames.stats().unwrap().allocated_frames,
        before,
        "user frame leak"
    );
    serial::println!("MIRROR_USER_CLEANUP_OK allocated={}", before);
    assert!(
        process.checks_passed,
        "init checks did not complete: {:?}",
        process.outcome
    );
    assert_eq!(process.bad_pointers, 4);
    assert_eq!(process.unknown_calls, 1);
    match (scenario, process.outcome.expect("missing process outcome")) {
        (0, Outcome::Exit(0)) => {}
        #[cfg(feature = "boot-smoke")]
        (6, Outcome::InvalidStack(0x0000_8000_0000_0000)) => {
            serial::println!("MIRROR_USER_RETURN_REJECTED rsp=0x800000000000");
        }
        #[cfg(feature = "boot-smoke")]
        (
            probe,
            Outcome::Fault {
                vector,
                error,
                address,
                rip,
            },
        ) => {
            let expected = match probe {
                1 => (14, 5, 0xffff_ffff_8000_0000),
                2 => (14, 7, 0x400000),
                3 => {
                    assert!((STACK_BASE..STACK_TOP).contains(&address));
                    assert_eq!(rip, address);
                    (14, 0x15, address)
                }
                4 => (14, 4, STACK_BASE - PAGE_SIZE),
                5 => (13, 0, address), // #GP leaves CR2 unchanged.
                _ => panic!("unexpected probe {probe}"),
            };
            assert_eq!((vector, error, address), expected, "wrong user fault");
            serial::println!(
                "MIRROR_USER_FAULT_CONTAINED vector={} error={:#x} cr2={:#x}",
                vector,
                error,
                address
            );
        }
        (_, outcome) => panic!("unexpected init outcome: {outcome:?}"),
    }
    serial::println!("MIRROR_USERSPACE_OK scenario={}", scenario);
}

fn load(
    space: &mut UserAddressSpace<'_>,
    frames: &mut FrameAllocator<'_>,
    bytes: &[u8],
    image: &elf::Image,
    direct_map: u64,
) -> Result<(), PagingError> {
    for segment in image.segments() {
        let permissions = if segment.executable {
            UserPermissions::READ_EXECUTE
        } else if segment.writable {
            UserPermissions::READ_WRITE
        } else {
            UserPermissions::READ_ONLY
        };
        let pages = segment.memory_size.div_ceil(PAGE_SIZE);
        for page in 0..pages {
            let offset = (page * PAGE_SIZE) as usize;
            let length = segment
                .file_size
                .saturating_sub(offset)
                .min(PAGE_SIZE as usize);
            let source = if length == 0 {
                &[]
            } else {
                &bytes[segment.file_offset + offset..segment.file_offset + offset + length]
            };
            map_initialized(
                space,
                frames,
                direct_map,
                segment.virtual_address + page * PAGE_SIZE,
                permissions,
                source,
            )?;
        }
    }
    for address in (STACK_BASE..STACK_TOP).step_by(PAGE_SIZE as usize) {
        map_initialized(
            space,
            frames,
            direct_map,
            address,
            UserPermissions::READ_WRITE,
            &[],
        )?;
    }
    assert!(space.translate(STACK_BASE - PAGE_SIZE)?.is_none());
    assert!(space.translate(STACK_TOP)?.is_none());
    Ok(())
}

fn map_initialized(
    space: &mut UserAddressSpace<'_>,
    frames: &mut FrameAllocator<'_>,
    direct_map: u64,
    address: u64,
    permissions: UserPermissions,
    source: &[u8],
) -> Result<(), PagingError> {
    let frame = frames.allocate().ok_or(PagingError::OutOfFrames)?;
    let result = (|| {
        let destination = direct_map
            .checked_add(frame)
            .ok_or(PagingError::DirectMapOverflow)? as *mut u8;
        // SAFETY: This fresh, unique usable frame has a full HHDM mapping. The
        // validated file slice is at most one page and cannot alias the frame.
        unsafe {
            destination.write_bytes(0, PAGE_SIZE as usize);
            core::ptr::copy_nonoverlapping(source.as_ptr(), destination, source.len());
            space.map_page(address, frame, permissions, frames)
        }
    })();
    if result.is_err() {
        // SAFETY: map_page transfers ownership only on success; no Rust
        // references, user mapping or hardware access survive this failure.
        unsafe { frames.deallocate(frame) }.expect("release unmapped user frame");
    }
    result
}

fn with_active(frame: &TrapFrame, operation: impl FnOnce(&mut Process<'_>)) {
    assert_eq!(frame.cs & 3, 3, "userspace handler requires CPL3");
    assert!(!arch::interrupts_enabled());
    let pointer = ACTIVE.load(Ordering::SeqCst).cast::<Process<'_>>();
    assert!(!pointer.is_null(), "no active process");
    // SAFETY: run_one suspends its stack while this pointer is published. Only
    // this BSP, with IF=0, accesses it; no handler retains a reference or reenters
    // userspace. Kernel/NMI faults never invoke this function. Closure references
    // cannot escape this call, and the borrowed kernel root remains live.
    unsafe { operation(&mut *pointer) };
}

pub(crate) fn syscall(frame: &mut TrapFrame) {
    let mut result = -38_i64;
    let mut finished = false;
    with_active(frame, |process| {
        match frame.rax {
            1 => {
                if frame.rsi > MAX_WRITE as u64 {
                    result = -22;
                    return;
                }
                let length = frame.rsi as usize;
                if length == 0 {
                    result = 0;
                    return;
                }
                let mut buffer = [0_u8; MAX_WRITE];
                // SAFETY: Tables are immutable while active, the process owns
                // its pages, and no other CPU can race validation/copy. The API
                // checks the entire user range before touching the destination.
                if unsafe {
                    process
                        .space
                        .copy_from_user(frame.rdi, &mut buffer[..length])
                }
                .is_err()
                {
                    process.bad_pointers += 1;
                    result = -14;
                    return;
                }
                // Diagnostic console ABI accepts UTF-8 and escapes controls so
                // an application cannot manufacture kernel result log lines.
                if let Ok(message) = core::str::from_utf8(&buffer[..length]) {
                    serial::println!("user: {}", message.escape_debug());
                    process.checks_passed |= &buffer[..length] == CHECKS_PASSED;
                    result = length as i64;
                } else {
                    result = -22;
                }
            }
            2 => result = timer::ticks() as i64,
            3 => {
                assert!(process.outcome.is_none());
                process.outcome = Some(Outcome::Exit(frame.rdi));
                finished = true;
            }
            _ => process.unknown_calls += 1,
        }
    });
    frame.rax = result as u64;
    if finished {
        interrupts::return_from_user(frame);
    }
}

pub(crate) fn fault(frame: &mut TrapFrame, cr2: u64) {
    with_active(frame, |process| {
        assert!(process.outcome.is_none());
        process.outcome = Some(Outcome::Fault {
            vector: frame.vector,
            error: frame.error,
            address: cr2,
            rip: frame.rip,
        });
    });
    serial::println!(
        "MIRROR_USER_FAULT vector={} error={:#x} rip={:#x} cr2={:#x}",
        frame.vector,
        frame.error,
        frame.rip,
        cr2
    );
    interrupts::return_from_user(frame);
}

/// Validate saved RSP against the native userspace address contract before
/// IRETQ. Check every user return, including timer IRQs, and end an invalid
/// context on the trusted continuation instead. Unmapped canonical user stacks can
/// return safely; a subsequent user access is handled as an ordinary user fault.
pub(crate) fn prepare_return(frame: &mut TrapFrame) {
    if !(PAGE_SIZE..(1_u64 << 47)).contains(&frame.rsp) {
        with_active(frame, |process| {
            assert!(process.outcome.is_none());
            process.outcome = Some(Outcome::InvalidStack(frame.rsp));
        });
        interrupts::return_from_user(frame);
    }
}

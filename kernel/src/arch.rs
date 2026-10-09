//! CPU-local operations, interrupt-state guards, and idle/fatal halt paths.

use core::arch::asm;

pub fn disable_interrupts() {
    // SAFETY: The kernel runs at CPL0. Keep a compiler memory barrier so memory
    // operations in an interrupt-disabled critical section cannot move outside.
    unsafe { asm!("cli", options(nostack)) };
}

pub fn interrupts_enabled() -> bool {
    let flags: u64;
    // SAFETY: PUSHFQ/POP are balanced, and the kernel stack is mapped writable.
    unsafe { asm!("pushfq", "pop {}", out(reg) flags, options(preserves_flags)) };
    flags & (1 << 9) != 0
}

/// Save this CPU's IF state and disable its maskable interrupts. This does not
/// serialize access from another CPU; callers still need shared-state ownership.
pub fn save_and_disable_interrupts() -> bool {
    let flags: u64;
    // SAFETY: CPL0, balanced stack use. Do not use `nomem`: this is also the
    // compiler barrier at the beginning of an interrupt-safe critical section.
    unsafe { asm!("pushfq", "pop {}", "cli", out(reg) flags) };
    flags & (1 << 9) != 0
}

/// Restore a previously saved interrupt state after leaving a critical section.
///
/// # Safety
/// Interrupts must currently be disabled. If `was_enabled` is true, all active
/// interrupt sources must have installed handlers and consistent shared state.
pub unsafe fn restore_interrupts(was_enabled: bool) {
    if was_enabled {
        // SAFETY: The caller guarantees handlers and shared state are ready.
        // This compiler memory barrier publishes critical-section writes first.
        unsafe { asm!("sti", options(nostack)) };
    }
}

/// Wait for interrupts once boot has installed and initialized all IRQ sources.
///
/// # Safety
/// Every unmasked interrupt must have a handler. All boot state must be ready
/// for handlers to observe it. This is an idle loop, not a task scheduler.
#[cfg(not(feature = "boot-smoke"))]
pub unsafe fn idle() -> ! {
    loop {
        // SAFETY: The caller has initialized all unmasked sources. STI's
        // interrupt shadow closes the race between enabling interrupts and HLT.
        unsafe { asm!("sti", "hlt", options(nostack)) };
    }
}

pub fn halt() -> ! {
    loop {
        // SAFETY: CPL0; fatal halt must not resume ordinary IRQ processing.
        unsafe { asm!("cli", "hlt", options(nomem, nostack)) };
    }
}

/// Invalidate one page on this CPU, including any matching global translation.
/// This is not a remote shootdown: callers must coordinate other processors.
///
/// # Safety
/// Run at CPL0 with IF clear and a canonical page-aligned address. Page-table
/// writes must precede this call; retain removed frames until every CPU using
/// the mapping has completed its own invalidation. All current roots use PCID0.
pub(crate) unsafe fn invalidate_tlb_page(address: u64) {
    // SAFETY: The caller validates the address (noncanonical INVLPG is a no-op)
    // and lifetime protocol. No nomem: publication must precede invalidation,
    // and a completion ACK must not move ahead of it. Intel SDM vol3 §5.10.4.1.
    unsafe { asm!("invlpg [{}]", in(reg) address, options(nostack, preserves_flags)) };
}

/// # Safety
/// The caller must own the specified I/O port and run with I/O privileges.
pub unsafe fn out8(port: u16, value: u8) {
    // SAFETY: The caller provides port ownership and I/O privileges.
    unsafe {
        asm!("out dx, al", in("dx") port, in("al") value, options(nomem, nostack, preserves_flags))
    };
}

/// # Safety
/// The caller must own the specified I/O port and run with I/O privileges.
pub unsafe fn in8(port: u16) -> u8 {
    let value: u8;
    // SAFETY: The caller provides port ownership and I/O privileges.
    unsafe {
        asm!("in al, dx", in("dx") port, out("al") value, options(nomem, nostack, preserves_flags))
    };
    value
}

/// # Safety
/// The caller must own both I/O byte lanes beginning at `port`, run with I/O
/// privileges, and ensure the device supports a 16-bit write at that port.
pub unsafe fn out16(port: u16, value: u16) {
    // SAFETY: The caller provides port ownership, width, and I/O privileges.
    unsafe {
        asm!("out dx, ax", in("dx") port, in("ax") value, options(nomem, nostack, preserves_flags))
    };
}

/// # Safety
/// The caller must own both I/O byte lanes beginning at `port`, run with I/O
/// privileges, and ensure the device supports a 16-bit read at that port.
pub unsafe fn in16(port: u16) -> u16 {
    let value: u16;
    // SAFETY: The caller provides port ownership, width, and I/O privileges.
    unsafe {
        asm!("in ax, dx", in("dx") port, out("ax") value, options(nomem, nostack, preserves_flags))
    };
    value
}

/// # Safety
/// The caller must own all four I/O byte lanes beginning at `port`, run with
/// I/O privileges, and ensure the device supports a 32-bit write at that port.
pub unsafe fn out32(port: u16, value: u32) {
    // SAFETY: The caller provides port ownership, width, and I/O privileges.
    unsafe {
        asm!("out dx, eax", in("dx") port, in("eax") value, options(nomem, nostack, preserves_flags))
    };
}

/// # Safety
/// The caller must own all four I/O byte lanes beginning at `port`, run with
/// I/O privileges, and ensure the device supports a 32-bit read at that port.
pub unsafe fn in32(port: u16) -> u32 {
    let value: u32;
    // SAFETY: The caller provides port ownership, width, and I/O privileges.
    unsafe {
        asm!("in eax, dx", in("dx") port, out("eax") value, options(nomem, nostack, preserves_flags))
    };
    value
}

#[cfg(feature = "boot-smoke")]
pub fn exit_qemu(success: bool) -> ! {
    let code: u32 = if success { 0x10 } else { 0x11 };
    // SAFETY: The smoke runner attaches isa-debug-exit at 0xf4. This code is
    // excluded from normal builds, which must never write this test-only port.
    unsafe {
        asm!("out dx, eax", in("dx") 0xf4_u16, in("eax") code, options(nomem, nostack, preserves_flags))
    };
    halt()
}

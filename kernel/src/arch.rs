//! Early x86-64 operations. Only the boot CPU runs; interrupts remain disabled.

use core::arch::asm;

pub fn disable_interrupts() {
    // SAFETY: The kernel runs at CPL0; boot keeps maskable interrupts disabled.
    unsafe { asm!("cli", options(nomem, nostack)) };
}

pub fn halt() -> ! {
    loop {
        // SAFETY: CPL0; IRQ handling and scheduling are not enabled yet.
        unsafe { asm!("cli", "hlt", options(nomem, nostack)) };
    }
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

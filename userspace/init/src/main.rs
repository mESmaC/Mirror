#![no_std]
#![no_main]

use core::arch::{asm, global_asm};
use core::panic::PanicInfo;
use mirror_vm_protocol::{
    ARENA_END, ARENA_START, COMMITTED_LIMIT, MAX_REGIONS, PAGE_SIZE, PROT_NONE, PROT_READ,
    PROT_READ_WRITE, RESERVED_LIMIT, STATS_SIZE, SYS_VM_PROTECT, SYS_VM_RELEASE, SYS_VM_RESERVE,
    SYS_VM_STATS,
};

mod clock;
mod fpu;
mod sync;
mod threads;
mod tls;

// Relink when the package-specific script changes, without a host build script.
const _: &str = include_str!("../linker.ld");

const SYS_WRITE: u64 = 1;
const SYS_TICKS: u64 = 2;
const SYS_EXIT: u64 = 3;
const SYS_YIELD: u64 = 4;
const SYS_SLEEP: u64 = 5;
const SYS_PID: u64 = 6;
const SYS_BOOT_HANDLE: u64 = 7;
const SYS_SEND: u64 = 8;
const SYS_RECV: u64 = 9;
const SYS_RECV_WAIT: u64 = 10;
const SYS_CLOSE: u64 = 11;
const SYS_DUP: u64 = 12;
const SYS_SURFACE_INFO: u64 = 13;
const SYS_SURFACE_PRESENT: u64 = 14;
const RIGHT_SEND: u64 = 1;
const RIGHT_RECEIVE: u64 = 2;
const RIGHT_WRITE: u64 = 4;
const EBADF: i64 = -9;
const EAGAIN: i64 = -11;
const ENOMEM: i64 = -12;
const EACCES: i64 = -13;
const EFAULT: i64 = -14;
const EEXIST: i64 = -17;
const EINVAL: i64 = -22;
const EMFILE: i64 = -24;
const EPIPE: i64 = -32;
const ENOSYS: i64 = -38;
const EMSGSIZE: i64 = -90;
const ETIMEDOUT: i64 = -110;
const KERNEL_BASE: u64 = 0xffff_ffff_8000_0000;
const CODE_BASE: u64 = 0x0040_0000;
const STACK_GUARD: u64 = 0x7ffe_f000;
const DATA_SENTINEL: u64 = 0xfedc_ba98_7654_3210;
const BSS_SENTINEL: u64 = 0x1122_3344_5566_7788;
const STACK_SENTINEL: u64 = 0xcafe_babe_0123_4567;
const SURFACE_INFO_BYTES: usize = 24;
const TILE_BYTES: usize = 16 + 4 * 16 * 16;
const VM_COPY: u64 = ARENA_START + 0x0100_0000;
const VM_FAULT: u64 = ARENA_START + 0x0200_0000;
const VM_SENTINEL: u64 = 0x7631_937d_a52c_4800;

// A valid 1x1 tile genuinely resides in the ELF's read-only segment. Its X byte
// is deliberately nonzero: RGBX padding carries no authority or alpha blending.
static READONLY_TILE: [u8; 20] = [
    0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 1, 0, 0, 0, 37, 173, 231, 255,
];

// RDI packs the probe in its low byte and the process role in the next byte.
// The entry trampoline establishes the System V alignment for the Rust call.
global_asm!(
    ".section .text.entry,\"ax\"",
    ".global _start",
    ".type _start,@function",
    "_start:",
    "fxsave64 [rip + MIRROR_INIT_INITIAL_FX]",
    "xor rbp, rbp",
    "and rsp, -16",
    "call init_main",
    "ud2",
    ".size _start, .-_start",
    ".previous",
);

unsafe extern "C" {
    static __readonly_end: u8;
}

static mut INITIALIZED: u64 = 0x0123_4567_89ab_cdef;
static mut ZERO_INITIALIZED: u64 = 0;

/// Ordinary native calls preserve all registers except the result in RAX.
/// THREAD_JOIN uses a separate wrapper for its additional RDX result.
/// Integer addresses let the kernel reject invalid pointers without constructing
/// invalid Rust references in the application.
fn syscall(number: u64, arg0: u64, arg1: u64, arg2: u64) -> i64 {
    let result: i64;
    // SAFETY: vector 0x80 is the kernel-provided userspace entry. Its handler
    // restores the user stack and every general-purpose register except RAX.
    unsafe {
        asm!(
            "int 0x80",
            inout("rax") number => result,
            in("rdi") arg0,
            in("rsi") arg1,
            in("rdx") arg2,
            options(nostack),
        );
    }
    result
}

fn write(console: u64, message: &[u8]) {
    if syscall(
        SYS_WRITE,
        message.as_ptr() as u64,
        message.len() as u64,
        console,
    ) != message.len() as i64
    {
        exit(10);
    }
}

fn exit(status: u64) -> ! {
    syscall(SYS_EXIT, status, 0, 0);
    // SAFETY: An exit returning is an ABI failure. UD2 makes that visible to the
    // kernel, which ends the process at the trap, so the block never returns.
    unsafe { asm!("ud2", options(noreturn, nostack)) }
}

fn require(condition: bool, status: u64) {
    if !condition {
        exit(status);
    }
}

fn duplicate(handle: u64, rights: u64, status: u64) -> u64 {
    let token = syscall(SYS_DUP, handle, rights, 0);
    require(token > 0 && token as u64 != handle, status);
    token as u64
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct VmStats {
    reserved: u64,
    committed: u64,
    regions: u64,
}

fn vm_word(bytes: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap())
}

fn vm_stats() -> VmStats {
    let mut bytes = [0_u8; STATS_SIZE];
    require(
        syscall(
            SYS_VM_STATS,
            bytes.as_mut_ptr() as u64,
            STATS_SIZE as u64,
            0,
        ) == STATS_SIZE as i64,
        200,
    );
    require(u32::from_le_bytes(bytes[..4].try_into().unwrap()) == 1, 201);
    require(
        u32::from_le_bytes(bytes[4..8].try_into().unwrap()) == PAGE_SIZE as u32,
        202,
    );
    require(vm_word(&bytes, 32) == RESERVED_LIMIT, 203);
    require(vm_word(&bytes, 40) == COMMITTED_LIMIT, 204);
    VmStats {
        reserved: vm_word(&bytes, 8),
        committed: vm_word(&bytes, 16),
        regions: vm_word(&bytes, 24),
    }
}

fn vm_reserve(address: u64, pages: u64, protection: u64) -> u64 {
    let result = syscall(SYS_VM_RESERVE, address, pages * PAGE_SIZE, protection);
    require(result > 0, 205);
    require(address == 0 || result as u64 == address, 206);
    require((result as u64).is_multiple_of(PAGE_SIZE), 207);
    result as u64
}

fn vm_release(address: u64, pages: u64) {
    require(
        syscall(SYS_VM_RELEASE, address, pages * PAGE_SIZE, 0) == 0,
        208,
    );
}

fn vm_protect(address: u64, pages: u64, protection: u64) {
    require(
        syscall(SYS_VM_PROTECT, address, pages * PAGE_SIZE, protection) == 0,
        209,
    );
}

fn vm_read(address: u64) -> u64 {
    // SAFETY: Callers supply an aligned byte range in a live readable anonymous
    // reservation. Its first access may legitimately demand a zero-filled page.
    unsafe { (address as *const u64).read_volatile() }
}

fn vm_write(address: u64, value: u64) {
    // SAFETY: Callers own an aligned writable word in their live reservation.
    unsafe { (address as *mut u64).write_volatile(value) };
}

fn vm_zero_page(address: u64) {
    for offset in (0..PAGE_SIZE).step_by(8) {
        require(vm_read(address + offset) == 0, 210);
    }
}

fn check_vm(console: u64, pid: u64) {
    let empty = VmStats {
        reserved: 0,
        committed: 0,
        regions: 0,
    };
    require(vm_stats() == empty, 211);
    for (address, length, protection) in [
        (0, 0, PROT_READ_WRITE),
        (ARENA_START + 1, PAGE_SIZE, PROT_READ_WRITE),
        (ARENA_START, PAGE_SIZE - 1, PROT_READ_WRITE),
        (ARENA_START - PAGE_SIZE, PAGE_SIZE, PROT_READ_WRITE),
        (ARENA_END, PAGE_SIZE, PROT_READ_WRITE),
        (ARENA_END - PAGE_SIZE, 2 * PAGE_SIZE, PROT_READ_WRITE),
        (ARENA_START, u64::MAX, PROT_READ_WRITE),
        (ARENA_START, PAGE_SIZE, 2),
        (ARENA_START, PAGE_SIZE, 5),
        (ARENA_START, PAGE_SIZE, u64::MAX),
    ] {
        require(
            syscall(SYS_VM_RESERVE, address, length, protection) == EINVAL,
            212,
        );
    }
    let mut stats_buffer = [0xa5_u8; STATS_SIZE];
    for length in [0, STATS_SIZE as u64 - 1, STATS_SIZE as u64 + 1, u64::MAX] {
        require(
            syscall(SYS_VM_STATS, stats_buffer.as_mut_ptr() as u64, length, 0) == EINVAL,
            213,
        );
    }
    require(
        syscall(
            SYS_VM_STATS,
            stats_buffer.as_mut_ptr() as u64,
            STATS_SIZE as u64,
            1,
        ) == EINVAL,
        214,
    );
    require(stats_buffer == [0xa5; STATS_SIZE], 215);
    for address in [0, CODE_BASE, KERNEL_BASE, u64::MAX - 23] {
        require(
            syscall(SYS_VM_STATS, address, STATS_SIZE as u64, 0) == EFAULT,
            216,
        );
    }
    require(vm_stats() == empty, 217);
    for number in [SYS_VM_RELEASE, SYS_VM_PROTECT] {
        for (address, length) in [
            (ARENA_START + 1, PAGE_SIZE),
            (ARENA_START, 0),
            (ARENA_START, 1),
            (ARENA_END, PAGE_SIZE),
        ] {
            require(syscall(number, address, length, 0) == EINVAL, 249);
        }
        require(syscall(number, ARENA_START, PAGE_SIZE, 0) == EFAULT, 250);
    }
    require(
        syscall(SYS_VM_RELEASE, ARENA_START, PAGE_SIZE, 1) == EINVAL,
        251,
    );
    require(
        syscall(SYS_VM_PROTECT, ARENA_START, PAGE_SIZE, 7) == EINVAL,
        252,
    );

    let base = vm_reserve(0, 4, PROT_READ_WRITE);
    require(base == ARENA_START, 218);
    require(
        vm_stats()
            == VmStats {
                reserved: 4 * PAGE_SIZE,
                committed: 0,
                regions: 1,
            },
        219,
    );
    require(
        syscall(SYS_VM_RESERVE, base, PAGE_SIZE, PROT_READ) == EEXIST,
        220,
    );
    // Rejected authority must not populate an otherwise valid lazy buffer.
    require(syscall(SYS_WRITE, base, 1, u64::MAX) == EBADF, 263);
    require(vm_stats().committed == 0, 264);
    require(
        syscall(SYS_SURFACE_INFO, console, base, SURFACE_INFO_BYTES as u64) == EACCES,
        265,
    );
    require(vm_stats().committed == 0, 266);
    vm_zero_page(base);
    let last = base + 3 * PAGE_SIZE;
    vm_write(last, VM_SENTINEL ^ pid);
    require(vm_stats().committed == 2 * PAGE_SIZE, 221);
    require(syscall(SYS_YIELD, 0, 0, 0) == 0, 222);
    // Both processes reserve this exact VA. A context switch must not expose
    // the other process's private physical page or its PID-derived value.
    require(vm_read(last) == VM_SENTINEL ^ pid, 223);
    vm_protect(last, 1, PROT_READ);
    require(
        syscall(SYS_VM_STATS, last, STATS_SIZE as u64, 0) == EFAULT,
        224,
    );
    require(vm_read(last) == VM_SENTINEL ^ pid, 225);
    vm_protect(last, 1, PROT_NONE);
    require(
        syscall(SYS_VM_STATS, last, STATS_SIZE as u64, 0) == EFAULT,
        226,
    );
    vm_protect(last, 1, PROT_READ_WRITE);
    require(vm_read(last) == VM_SENTINEL ^ pid, 227);
    vm_write(last, !pid);
    vm_release(base + PAGE_SIZE, 2);
    let split = vm_stats();
    require(
        split.reserved == 2 * PAGE_SIZE && split.committed == 2 * PAGE_SIZE,
        228,
    );
    require(split.regions == 2, 229);
    require(
        syscall(SYS_VM_RELEASE, base, 4 * PAGE_SIZE, 0) == EFAULT,
        230,
    );
    require(
        syscall(SYS_VM_PROTECT, base, 4 * PAGE_SIZE, PROT_READ) == EFAULT,
        231,
    );
    require(vm_stats() == split, 232);
    vm_write(base, pid);
    require(vm_read(last) == !pid, 233);
    vm_release(base, 1);
    vm_release(last, 1);
    require(vm_stats() == empty, 234);

    // The allocator may recycle a page from either process. Every byte of a
    // new anonymous page must be zero regardless of its previous owner.
    vm_reserve(base, 1, PROT_READ_WRITE);
    vm_zero_page(base);
    vm_release(base, 1);
    vm_reserve(base, 1, PROT_READ);
    require(vm_stats().committed == 0, 235);
    require(syscall(SYS_WRITE, base, 1, console) == 1, 236);
    require(vm_stats().committed == PAGE_SIZE, 237);
    vm_release(base, 1);

    // The kernel must resolve an entirely untouched syscall output buffer,
    // including a record straddling two non-contiguous physical pages.
    vm_reserve(base, 2, PROT_READ_WRITE);
    require(
        syscall(SYS_VM_STATS, base + PAGE_SIZE - 24, STATS_SIZE as u64, 0) == STATS_SIZE as i64,
        238,
    );
    require(vm_stats().committed == 2 * PAGE_SIZE, 239);
    vm_release(base, 2);
    vm_reserve(base, 1, PROT_READ_WRITE);
    require(
        syscall(SYS_VM_STATS, base + PAGE_SIZE - 24, STATS_SIZE as u64, 0) == EFAULT,
        261,
    );
    require(vm_stats().committed == 0, 262);
    for offset in [PAGE_SIZE - 24, PAGE_SIZE - 16, PAGE_SIZE - 8] {
        vm_write(base + offset, VM_SENTINEL);
    }
    let before_invalid = vm_stats();
    require(
        syscall(SYS_VM_STATS, base + PAGE_SIZE - 24, STATS_SIZE as u64, 0) == EFAULT,
        240,
    );
    for offset in [PAGE_SIZE - 24, PAGE_SIZE - 16, PAGE_SIZE - 8] {
        require(vm_read(base + offset) == VM_SENTINEL, 241);
    }
    require(vm_stats() == before_invalid, 242);
    vm_release(base, 1);

    // Reservation and region limits must not consume physical pages or lose
    // existing reservations when the next allocation is rejected.
    vm_reserve(base, RESERVED_LIMIT / PAGE_SIZE, PROT_READ_WRITE);
    require(
        syscall(SYS_VM_RESERVE, 0, PAGE_SIZE, PROT_READ_WRITE) == ENOMEM,
        243,
    );
    require(
        vm_stats().reserved == RESERVED_LIMIT && vm_stats().committed == 0,
        244,
    );
    vm_release(base, RESERVED_LIMIT / PAGE_SIZE);
    for index in 0..MAX_REGIONS {
        vm_reserve(base + index as u64 * 2 * PAGE_SIZE, 1, PROT_READ_WRITE);
    }
    require(vm_stats().regions == MAX_REGIONS as u64, 245);
    require(
        syscall(
            SYS_VM_RESERVE,
            base + MAX_REGIONS as u64 * 2 * PAGE_SIZE,
            PAGE_SIZE,
            PROT_READ_WRITE,
        ) == ENOMEM,
        246,
    );
    for index in 0..MAX_REGIONS {
        vm_release(base + index as u64 * 2 * PAGE_SIZE, 1);
    }
    require(vm_stats() == empty, 247);

    // Leave a populated page and an untouched reservation for exit/fault
    // teardown, and verify this private value again after all other probes.
    vm_reserve(base, 2, PROT_READ_WRITE);
    vm_write(base, VM_SENTINEL ^ pid);
    require(
        vm_stats()
            == VmStats {
                reserved: 2 * PAGE_SIZE,
                committed: PAGE_SIZE,
                regions: 1,
            },
        248,
    );
    write(console, b"Mirror init: VM checks passed\n");
}

fn surface_word(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
}

fn tile_header(bytes: &mut [u8; TILE_BYTES], x: u32, y: u32, width: u32, height: u32) {
    for (index, value) in [x, y, width, height].into_iter().enumerate() {
        bytes[index * 4..index * 4 + 4].copy_from_slice(&value.to_le_bytes());
    }
}

fn surface_pixel(x: u32, y: u32, width: u32, height: u32) -> [u8; 4] {
    let u = x * 256 / width;
    let v = y * 128 / height;
    if !(3..253).contains(&u) || !(3..125).contains(&v) {
        return [66, 211, 230, 255];
    }
    // Original five-by-seven glyphs for this userspace-authored MIRROR panel.
    // The kernel only receives pixels; it does not supply text or decoration.
    const TITLE: [[u8; 7]; 6] = [
        [17, 27, 21, 21, 17, 17, 17], // M
        [31, 4, 4, 4, 4, 4, 31],      // I
        [30, 17, 17, 30, 20, 18, 17], // R
        [30, 17, 17, 30, 20, 18, 17], // R
        [14, 17, 17, 17, 17, 17, 14], // O
        [30, 17, 17, 30, 20, 18, 17], // R
    ];
    if (74..182).contains(&u) && (30..51).contains(&v) {
        let column = (u - 74) / 3;
        let glyph = column / 6;
        let bit = column % 6;
        let row = (v - 30) / 3;
        if bit < 5 && TITLE[glyph as usize][row as usize] & (1 << (4 - bit)) != 0 {
            return [226, 239, 255, 17];
        }
    }
    let distance = u.abs_diff(128);
    if (68..106).contains(&v) && distance < (v - 56) * 2 && distance > 8 {
        return if u < 128 {
            [46, (140 + v) as u8, 222, 0]
        } else {
            [(u - 40) as u8, 104, 237, 99]
        };
    }
    [
        12 + (u / 24) as u8,
        20 + (v / 8) as u8,
        39 + (u / 16) as u8,
        231,
    ]
}

fn draw_surface(handle: u64, width: u32, height: u32) {
    let mut packet = [0u8; TILE_BYTES];
    let mut y = 0;
    while y < height {
        let tile_height = (height - y).min(16);
        let mut x = 0;
        while x < width {
            let tile_width = (width - x).min(16);
            tile_header(&mut packet, x, y, tile_width, tile_height);
            for row in 0..tile_height {
                for column in 0..tile_width {
                    let offset = 16 + 4 * (row * tile_width + column) as usize;
                    packet[offset..offset + 4].copy_from_slice(&surface_pixel(
                        x + column,
                        y + row,
                        width,
                        height,
                    ));
                }
            }
            let length = 16 + 4 * u64::from(tile_width * tile_height);
            require(
                syscall(SYS_SURFACE_PRESENT, handle, packet.as_ptr() as u64, length)
                    == length as i64,
                175,
            );
            x += tile_width;
        }
        y += tile_height;
    }
}

fn check_surface(console: u64, role: u64) -> u64 {
    let boot = syscall(SYS_BOOT_HANDLE, 3, 0, 0);
    if role == 1 {
        require(boot == EBADF, 120);
        return 0;
    }
    // A headless boot has no surface authority and emits no display marker.
    if boot == EBADF {
        return 0;
    }
    require(boot > 0, 121);
    let handle = boot as u64;
    require(syscall(SYS_BOOT_HANDLE, 3, 0, 0) == boot, 122);
    let mut info = [0u8; SURFACE_INFO_BYTES];
    let info_pointer = info.as_mut_ptr() as u64;
    require(
        syscall(SYS_SURFACE_INFO, handle, info_pointer, 24) == 24,
        123,
    );
    require(
        surface_word(&info, 0) == 1
            && surface_word(&info, 12) == 1
            && surface_word(&info, 16) == 16
            && surface_word(&info, 20) == 0,
        124,
    );
    let width = surface_word(&info, 4);
    let height = surface_word(&info, 8);
    require(
        (1..=256).contains(&width) && (1..=128).contains(&height),
        125,
    );
    let expected_info = info;
    let mut packet = [0u8; TILE_BYTES];
    tile_header(&mut packet, width - 1, 0, 1, 1);
    packet[16..20].copy_from_slice(&[203, 97, 242, 77]);
    let packet_pointer = packet.as_ptr() as u64;

    // Handle authority is checked before the length or any user memory access.
    require(syscall(SYS_SURFACE_INFO, u64::MAX, 0, 0) == EBADF, 126);
    require(syscall(SYS_SURFACE_PRESENT, u64::MAX, 0, 0) == EBADF, 127);
    require(
        syscall(SYS_SURFACE_INFO, console, info_pointer, 24) == EACCES,
        128,
    );
    require(
        syscall(SYS_SURFACE_PRESENT, console, packet_pointer, 20) == EACCES,
        129,
    );
    let inert = duplicate(handle, 0, 130);
    require(syscall(SYS_SURFACE_INFO, inert, 0, 0) == EACCES, 131);
    require(syscall(SYS_SURFACE_PRESENT, inert, 0, 0) == EACCES, 132);
    require(syscall(SYS_DUP, inert, RIGHT_WRITE, 0) == EACCES, 133);
    require(syscall(SYS_CLOSE, inert, 0, 0) == 0, 134);
    require(
        syscall(SYS_SURFACE_INFO, inert, info_pointer, 24) == EBADF,
        135,
    );
    require(
        syscall(SYS_SURFACE_PRESENT, inert, packet_pointer, 20) == EBADF,
        136,
    );

    let writer = duplicate(handle, RIGHT_WRITE, 137);
    require(
        syscall(SYS_SURFACE_INFO, writer, info_pointer, 24) == 24,
        138,
    );
    require(info == expected_info, 139);
    // Exactly three successful 1x1 probes precede the complete tiled image:
    // duplicate authority/top-right, a read-only source/top-left, bottom-right.
    require(
        syscall(SYS_SURFACE_PRESENT, writer, packet_pointer, 20) == 20,
        140,
    );
    require(syscall(SYS_CLOSE, writer, 0, 0) == 0, 141);
    require(
        syscall(SYS_SURFACE_INFO, writer, info_pointer, 24) == EBADF,
        142,
    );
    require(
        syscall(SYS_SURFACE_PRESENT, writer, packet_pointer, 20) == EBADF,
        143,
    );
    require(
        syscall(
            SYS_SURFACE_PRESENT,
            handle,
            READONLY_TILE.as_ptr() as u64,
            20,
        ) == 20,
        144,
    );
    tile_header(&mut packet, width - 1, height - 1, 1, 1);
    require(
        syscall(SYS_SURFACE_PRESENT, handle, packet_pointer, 20) == 20,
        145,
    );

    info.fill(0xa5);
    for (length, status) in [(0, 146), (23, 147), (25, 148), (u64::MAX, 149)] {
        require(
            syscall(SYS_SURFACE_INFO, handle, info_pointer, length) == EINVAL,
            status,
        );
    }
    for (pointer, status) in [
        (0, 150),
        (CODE_BASE, 151),
        (u64::MAX - 12, 152),
        (STACK_GUARD, 153),
    ] {
        require(
            syscall(SYS_SURFACE_INFO, handle, pointer, 24) == EFAULT,
            status,
        );
    }
    require(info == [0xa5; SURFACE_INFO_BYTES], 154);

    let readonly_end = (&raw const __readonly_end) as u64;
    // The last source crosses the ELF's unmapped read-only-segment guard. Its
    // complete packet must be checked/copied before parsing or touching pixels.
    for (pointer, status) in [
        (0, 155),
        (KERNEL_BASE, 156),
        (u64::MAX - 1, 157),
        (readonly_end - 16, 158),
    ] {
        require(
            syscall(SYS_SURFACE_PRESENT, handle, pointer, 20) == EFAULT,
            status,
        );
    }
    for (length, status) in [
        (0, 159),
        (15, 160),
        (19, 161),
        (21, 162),
        (1041, 163),
        (u64::MAX, 164),
    ] {
        require(
            syscall(SYS_SURFACE_PRESENT, handle, packet_pointer, length) == EINVAL,
            status,
        );
    }
    for (x, y, tile_width, tile_height, status) in [
        (width, 0, 1, 1, 165),
        (0, height, 1, 1, 166),
        (u32::MAX, 0, 1, 1, 167),
        (0, u32::MAX, 1, 1, 168),
        (0, 0, 0, 1, 169),
        (0, 0, 1, 0, 170),
        (0, 0, 17, 1, 171),
        (0, 0, 1, 17, 172),
        (0, 0, u32::MAX, 1, 173),
        (0, 0, 1, u32::MAX, 174),
    ] {
        tile_header(&mut packet, x, y, tile_width, tile_height);
        require(
            syscall(SYS_SURFACE_PRESENT, handle, packet_pointer, 20) == EINVAL,
            status,
        );
    }
    draw_surface(handle, width, height);
    require(syscall(SYS_BOOT_HANDLE, 3, 0, 0) == boot, 176);
    require(
        syscall(SYS_SURFACE_INFO, handle, info_pointer, 24) == 24,
        177,
    );
    require(info == expected_info, 178);
    write(console, b"Mirror init: surface checks passed\n");
    // Retain the original live token through the peer's foreign-handle checks
    // and process teardown; no surface duplicate survives this function.
    handle
}

fn check_foreign_surface(console: u64, foreign: u64) {
    require(foreign <= i64::MAX as u64, 182);
    let mut info = [0u8; SURFACE_INFO_BYTES];
    require(
        syscall(SYS_SURFACE_INFO, foreign, info.as_mut_ptr() as u64, 24) == EBADF,
        183,
    );
    require(
        syscall(
            SYS_SURFACE_PRESENT,
            foreign,
            READONLY_TILE.as_ptr() as u64,
            20,
        ) == EBADF,
        184,
    );
    write(console, b"Mirror init: surface denied\n");
}

fn check_capabilities(console: u64, outbound: u64, inbound: u64, surface: u64) -> u64 {
    let mut buffer = [0u8; 64];
    let pointer = buffer.as_mut_ptr() as u64;
    require(syscall(SYS_SEND, u64::MAX, pointer, 64) == EBADF, 63);
    require(syscall(SYS_CLOSE, u64::MAX, 0, 0) == EBADF, 64);
    require(syscall(SYS_SEND, inbound, pointer, 64) == EACCES, 65);
    require(syscall(SYS_RECV, outbound, pointer, 64) == EACCES, 66);
    require(syscall(SYS_WRITE, pointer, 1, outbound) == EACCES, 67);
    require(
        syscall(SYS_DUP, outbound, RIGHT_SEND | RIGHT_RECEIVE, 0) == EACCES,
        68,
    );

    let inert = duplicate(outbound, 0, 69);
    require(syscall(SYS_SEND, inert, pointer, 64) == EACCES, 70);
    require(syscall(SYS_DUP, inert, RIGHT_SEND, 0) == EACCES, 71);
    require(syscall(SYS_CLOSE, inert, 0, 0) == 0, 72);

    // A displayed primary has four boot capabilities; peers/headless processes
    // have three. Surface probe duplicates have already been closed here.
    let mut clones = [0u64; 5];
    let clone_count = clones.len() - usize::from(surface != 0);
    for index in 0..clone_count {
        let clone = duplicate(console, RIGHT_WRITE, 73);
        require(!clones[..index].contains(&clone), 74);
        clones[index] = clone;
    }
    require(syscall(SYS_DUP, console, RIGHT_WRITE, 0) == EMFILE, 75);
    let stale = clones[0];
    require(syscall(SYS_CLOSE, stale, 0, 0) == 0, 76);
    let replacement = duplicate(console, RIGHT_WRITE, 77);
    require(replacement != stale, 78);
    clones[0] = replacement;
    require(syscall(SYS_WRITE, pointer, 1, stale) == EBADF, 79);
    write(replacement, b"Mirror init: console capability checked\n");
    for &clone in &clones[..clone_count] {
        require(syscall(SYS_CLOSE, clone, 0, 0) == 0, 80);
    }
    require(syscall(SYS_WRITE, pointer, 1, replacement) == EBADF, 81);
    require(syscall(SYS_BOOT_HANDLE, 0, 0, 0) == console as i64, 82);

    let sender = duplicate(outbound, RIGHT_SEND, 83);
    require(syscall(SYS_CLOSE, outbound, 0, 0) == 0, 84);
    require(syscall(SYS_SEND, outbound, pointer, 64) == EBADF, 85);
    require(syscall(SYS_BOOT_HANDLE, 1, 0, 0) == EBADF, 86);
    require(syscall(SYS_DUP, outbound, RIGHT_SEND, 0) == EBADF, 87);
    require(syscall(SYS_SEND, sender, 0, 64) == EFAULT, 88);
    require(syscall(SYS_SEND, sender, pointer, 65) == EMSGSIZE, 89);
    sender
}

fn message(role: u64, tag: u8, sender: u64, surface: u64, tid: u64) -> [u8; 64] {
    let mut bytes = [0u8; 64];
    for (index, byte) in bytes.iter_mut().enumerate() {
        *byte = (index as u8).wrapping_mul(17) ^ tag ^ (role as u8).wrapping_mul(0x5b);
    }
    bytes[0] = tag;
    bytes[1] = role as u8;
    bytes[8..16].copy_from_slice(&sender.to_le_bytes());
    bytes[16..24].copy_from_slice(&surface.to_le_bytes());
    bytes[24..32].copy_from_slice(&tid.to_le_bytes());
    bytes
}

fn send_message(sender: u64, role: u64, tag: u8, surface: u64) {
    let bytes = message(role, tag, sender, surface, threads::current_id());
    require(
        syscall(SYS_SEND, sender, bytes.as_ptr() as u64, 64) == 64,
        100,
    );
}

fn validate_message(bytes: &[u8; 64], role: u64, tag: u8) -> u64 {
    let foreign_sender = u64::from_le_bytes(bytes[8..16].try_into().unwrap());
    let foreign_surface = u64::from_le_bytes(bytes[16..24].try_into().unwrap());
    let foreign_tid = u64::from_le_bytes(bytes[24..32].try_into().unwrap());
    require(foreign_sender > 0, 101);
    require(
        *bytes == message(role, tag, foreign_sender, foreign_surface, foreign_tid),
        102,
    );
    // Learning another process's integer token does not grant its authority.
    require(
        syscall(SYS_SEND, foreign_sender, bytes.as_ptr() as u64, 64) == EBADF,
        103,
    );
    threads::check_foreign(foreign_tid);
    foreign_surface
}

fn receive_message(receiver: u64, role: u64, tag: u8, check_rejections: bool) -> u64 {
    let mut bytes = [0u8; 64];
    let pointer = bytes.as_mut_ptr() as u64;
    if check_rejections {
        require(
            syscall(SYS_RECV_WAIT, receiver, pointer, 1) == EMSGSIZE,
            104,
        );
        require(syscall(SYS_RECV, receiver, CODE_BASE, 64) == EFAULT, 105);
        let baseline = vm_stats();
        vm_reserve(VM_COPY, 1, PROT_READ_WRITE);
        for offset in [
            PAGE_SIZE - 32,
            PAGE_SIZE - 24,
            PAGE_SIZE - 16,
            PAGE_SIZE - 8,
        ] {
            vm_write(VM_COPY + offset, VM_SENTINEL);
        }
        require(
            syscall(SYS_RECV, receiver, VM_COPY + PAGE_SIZE - 32, 64) == EFAULT,
            253,
        );
        for offset in [
            PAGE_SIZE - 32,
            PAGE_SIZE - 24,
            PAGE_SIZE - 16,
            PAGE_SIZE - 8,
        ] {
            require(vm_read(VM_COPY + offset) == VM_SENTINEL, 254);
        }
        vm_release(VM_COPY, 1);
        vm_reserve(VM_COPY, 2, PROT_READ);
        require(
            syscall(SYS_RECV, receiver, VM_COPY + PAGE_SIZE - 32, 64) == EFAULT,
            255,
        );
        require(vm_stats().committed == baseline.committed, 256);
        vm_protect(VM_COPY, 2, PROT_READ_WRITE);
        // All invalid copies must preserve the FIFO head. The successful retry
        // spans two untouched pages and must commit them before copying once.
        require(
            syscall(SYS_RECV_WAIT, receiver, VM_COPY + PAGE_SIZE - 32, 64) == 64,
            257,
        );
        require(
            vm_stats().committed == baseline.committed + 2 * PAGE_SIZE,
            258,
        );
        // SAFETY: A successful receive materialized and initialized every byte
        // of this valid user range; the local array is disjoint from its pages.
        unsafe {
            core::ptr::copy_nonoverlapping(
                (VM_COPY + PAGE_SIZE - 32) as *const u8,
                bytes.as_mut_ptr(),
                bytes.len(),
            );
        }
        vm_release(VM_COPY, 2);
        require(vm_stats() == baseline, 259);
        return validate_message(&bytes, role, tag);
    }
    // Both failed copies above must leave the full message at the FIFO head.
    require(syscall(SYS_RECV_WAIT, receiver, pointer, 64) == 64, 106);
    validate_message(&bytes, role, tag)
}

fn check_ipc(console: u64, role: u64, probe: u64, surface: u64) {
    let outbound = syscall(SYS_BOOT_HANDLE, 1, 0, 0);
    let inbound = syscall(SYS_BOOT_HANDLE, 2, 0, 0);
    require(outbound > 0, 61);
    require(inbound > 0, 62);
    let receiver = inbound as u64;
    // A channel has RECEIVE rights but is not an input object. Neither probe
    // process receives slot 4, and rejected calls must not touch an input lease.
    require(syscall(SYS_BOOT_HANDLE, 4, 0, 0) == EBADF, 185);
    let mut input_bytes = [0u8; 32];
    for call in [15, 16] {
        require(
            syscall(call, receiver, input_bytes.as_mut_ptr() as u64, 32) == EACCES,
            186,
        );
        require(
            syscall(call, u64::MAX, input_bytes.as_mut_ptr() as u64, 32) == EBADF,
            187,
        );
    }
    let sender = check_capabilities(console, outbound as u64, receiver, surface);
    if role == 0 {
        require(receive_message(receiver, 1, 1, true) == 0, 180);
        send_message(sender, 0, 2, surface);
        // The kernel times this receive, from the peer's sending syscall to
        // this thread's dispatch (spec 0030a, acceptance 4). Message 3 always
        // finds it blocked: the peer sends it only after receiving message 2
        // through VM calls that each return the peer to the scheduler.
        write(console, b"Mirror init: preempt receive\n");
        require(receive_message(receiver, 1, 3, false) == 0, 181);
        // Give the peer a deterministic opportunity to block waiting for EOF.
        require(syscall(SYS_YIELD, 0, 0, 0) == 0, 108);
        // The deadline half runs while the peer waits, far inside its
        // 100-tick receive deadline.
        threads::check_wake_preemption(console);
        if probe == 9 {
            require(syscall(SYS_SLEEP, 110, 0, 0) == 0, 109);
        }
        if probe != 8 {
            require(syscall(SYS_CLOSE, sender, 0, 0) == 0, 110);
        }
        // Probe 8 intentionally retains this last sender until process teardown.
    } else {
        // The primary cannot send its reply until we send the first message,
        // so this queue is empty regardless of timer preemption.
        let mut empty_buffer = [0u8; 64];
        require(
            syscall(SYS_RECV, receiver, empty_buffer.as_mut_ptr() as u64, 64) == EAGAIN,
            107,
        );
        require(syscall(SYS_SLEEP, 4, 0, 0) == 0, 111);
        send_message(sender, 1, 1, 0);
        let foreign_surface = receive_message(receiver, 0, 2, true);
        if foreign_surface != 0 {
            check_foreign_surface(console, foreign_surface);
        }
        send_message(sender, 1, 3, 0);
        let mut buffer = [0u8; 64];
        let mut saw_eof = false;
        for _ in 0..3 {
            let result = syscall(SYS_RECV_WAIT, receiver, buffer.as_mut_ptr() as u64, 64);
            if result == EPIPE {
                saw_eof = true;
                break;
            }
            require(result == ETIMEDOUT, 112);
        }
        require(saw_eof, 113);
        require(syscall(SYS_CLOSE, sender, 0, 0) == 0, 114);
    }
    write(console, b"Mirror init: IPC checks passed\n");
}

fn check_syscall_registers(pid: i64) {
    let result: u64;
    let seed = (pid as u64).rotate_left(32);
    let mut rcx = seed ^ 0x11;
    let mut rdi = seed ^ 0x22;
    let mut rsi = seed ^ 0x33;
    let mut rdx = seed ^ 0x44;
    let mut r8 = seed ^ 0x55;
    let mut r9 = seed ^ 0x66;
    let mut r10 = seed ^ 0x77;
    let mut r11 = seed ^ 0x88;
    let mut r12 = seed ^ 0x99;
    let mut r13 = seed ^ 0xaa;
    let mut r14 = seed ^ 0xbb;
    let mut r15 = seed ^ 0xcc;
    // SAFETY: YIELD restores the user stack, and the assembly pops everything it
    // pushes. Declare every other general-purpose register as an output so broken
    // restoration can be detected without compiler assumptions hiding the
    // problem. RBX/RBP are reserved by LLVM; save, test, and restore them inside
    // the assembly instead. Enter with DF and AC set (POPFQ may set AC at CPL3;
    // every stack access in the window is aligned), capture both after
    // rescheduling, and clear both before any Rust code resumes.
    unsafe {
        asm!(
            "push rbx",
            "push rbp",
            "push rsp",
            "mov rbx, r8",
            "xor rbx, 0x88",
            "mov rbp, r8",
            "xor rbp, 0xbb",
            "pushfq",
            "bts qword ptr [rsp], 18",
            "popfq",
            "std",
            "int 0x80",
            "pushfq",
            "pushfq",
            "btr qword ptr [rsp], 18",
            "btr qword ptr [rsp], 10",
            "popfq",
            "test rax, rax",
            "jnz 2f",
            "xor rbx, 0x88",
            "cmp rbx, r8",
            "jne 2f",
            "xor rbp, 0xbb",
            "cmp rbp, r8",
            "jne 2f",
            "test qword ptr [rsp], 0x400",
            "jz 2f",
            "test qword ptr [rsp], 0x40000",
            "jz 2f",
            "lea rax, [rsp + 16]",
            "cmp rax, [rsp + 8]",
            "jne 2f",
            "xor eax, eax",
            "jmp 3f",
            "2:",
            "mov eax, 1",
            "3:",
            "add rsp, 16",
            "pop rbp",
            "pop rbx",
            inout("rax") SYS_YIELD => result,
            inout("rcx") rcx,
            inout("rdi") rdi,
            inout("rsi") rsi,
            inout("rdx") rdx,
            inout("r8") r8,
            inout("r9") r9,
            inout("r10") r10,
            inout("r11") r11,
            inout("r12") r12,
            inout("r13") r13,
            inout("r14") r14,
            inout("r15") r15,
        );
    }
    require(
        result == 0
            && rcx == seed ^ 0x11
            && rdi == seed ^ 0x22
            && rsi == seed ^ 0x33
            && rdx == seed ^ 0x44
            && r8 == seed ^ 0x55
            && r9 == seed ^ 0x66
            && r10 == seed ^ 0x77
            && r11 == seed ^ 0x88
            && r12 == seed ^ 0x99
            && r13 == seed ^ 0xaa
            && r14 == seed ^ 0xbb
            && r15 == seed ^ 0xcc,
        28,
    );
}

fn check_private_state(pid: i64, stack_value: &u64) {
    require(vm_read(ARENA_START) == VM_SENTINEL ^ pid as u64, 260);
    require(syscall(SYS_PID, 0, 0, 0) == pid, 37);
    // SAFETY: These statics and the borrowed local are live in this process's
    // private address space. Volatile reads force rechecking after each switch.
    unsafe {
        require(
            (&raw const INITIALIZED).read_volatile() == DATA_SENTINEL ^ pid as u64,
            38,
        );
        require(
            (&raw const ZERO_INITIALIZED).read_volatile() == BSS_SENTINEL ^ pid as u64,
            39,
        );
        require(
            core::ptr::read_volatile(stack_value) == STACK_SENTINEL ^ pid as u64,
            40,
        );
    }
}

fn check_preempted_arithmetic(pid: i64) {
    const ITERATIONS: u64 = 1024;
    let first_tick = syscall(SYS_TICKS, 0, 0, 0);
    require(first_tick >= 0, 52);
    let mut previous_tick = first_tick;
    let mut completed = false;
    for block in 0..1_000_000u64 {
        let low_seed = DATA_SENTINEL
            ^ (pid as u64).rotate_left(17)
            ^ block.wrapping_mul(0x9e37_79b9_7f4a_7c15);
        let high_seed = BSS_SENTINEL ^ (pid as u64).rotate_left(41) ^ block;
        let sum = low_seed as u128 + (u64::MAX - 4) as u128 * ITERATIONS as u128;
        let expected_high = high_seed.wrapping_add((sum >> 64) as u64);
        let mut low = low_seed;
        let mut high = high_seed;
        let remaining: u64;
        // SAFETY: This finite block touches only declared register outputs.
        // Timer IRQs may land between ADD and ADC, so preserving the carry flag
        // is required in addition to saving RAX/RDX/RCX across preemption.
        unsafe {
            asm!(
                "2:",
                "add rax, -5",
                "adc rdx, 0",
                "dec rcx",
                "jnz 2b",
                inout("rax") low,
                inout("rdx") high,
                inout("rcx") ITERATIONS => remaining,
                options(nomem, nostack),
            );
        }
        require(low == sum as u64, 53);
        require(high == expected_high, 54);
        require(remaining == 0, 55);
        // TICKS only observes time; this phase never voluntarily reschedules.
        let tick = syscall(SYS_TICKS, 0, 0, 0);
        require(tick >= previous_tick, 56);
        previous_tick = tick;
        if tick - first_tick >= 8 {
            completed = true;
            break;
        }
    }
    require(completed, 57);
}

#[unsafe(no_mangle)]
extern "C" fn init_main(launch: u64) -> ! {
    let probe = launch & 0xff;
    let role = launch >> 8;
    require(role <= 1 && probe <= 22 && (role == 0 || probe == 0), 33);
    fpu::check_initial();
    tls::check_initial();
    let code_segment: u16;
    // SAFETY: reading CS is permitted at every privilege level.
    unsafe {
        asm!("mov {selector:x}, cs", selector = out(reg) code_segment, options(nomem, nostack))
    };
    require(code_segment & 3 == 3, 11);

    let console = syscall(SYS_BOOT_HANDLE, 0, 0, 0);
    require(console > 0, 60);
    let console = console as u64;
    write(console, b"Mirror init: running Rust in ring 3\n");
    let pid = syscall(SYS_PID, 0, 0, 0);
    require(pid > 0, 34);
    require(syscall(SYS_PID, 0, 0, 0) == pid, 35);
    tls::start(pid as u64);
    fpu::check(console, pid as u64);
    tls::finish(console, pid as u64);
    clock::check(console);
    threads::check(console, pid as u64);
    if role == 0 {
        // Both roles run the battery at once, so only one fills its thread
        // table: a pair session then peaks at 16 + 4 live threads. The FIFO
        // wait-queue check follows with nine (spec 0030a, acceptance 3).
        threads::check_capacity(console);
        sync::check_wait_queue(console);
    }
    sync::check(console);

    // SAFETY: Both statics are live in this process's private address space, and
    // only this thread accesses them, always through raw pointers. These volatile
    // accesses ensure both file-backed data and zero-filled BSS are exercised
    // even in optimized builds.
    unsafe {
        require(
            (&raw const INITIALIZED).read_volatile() == 0x0123_4567_89ab_cdef,
            12,
        );
        require((&raw const ZERO_INITIALIZED).read_volatile() == 0, 13);
        (&raw mut INITIALIZED).write_volatile(DATA_SENTINEL ^ pid as u64);
        (&raw mut ZERO_INITIALIZED).write_volatile(BSS_SENTINEL ^ pid as u64);
        require(
            (&raw const INITIALIZED).read_volatile() == DATA_SENTINEL ^ pid as u64,
            14,
        );
        require(
            (&raw const ZERO_INITIALIZED).read_volatile() == BSS_SENTINEL ^ pid as u64,
            15,
        );
    }

    let mut stack_value = 0u64;
    // SAFETY: this local remains live and writable for both accesses.
    unsafe {
        (&raw mut stack_value).write_volatile(STACK_SENTINEL ^ pid as u64);
        require(
            (&raw const stack_value).read_volatile() == STACK_SENTINEL ^ pid as u64,
            16,
        );
    }

    check_vm(console, pid as u64);
    let surface = check_surface(console, role);
    check_ipc(console, role, probe, surface);

    require(syscall(SYS_WRITE, KERNEL_BASE, 1, console) == EFAULT, 20);
    require(syscall(SYS_WRITE, 0, 1, console) == EFAULT, 21);
    require(syscall(SYS_WRITE, u64::MAX - 1, 4, console) == EFAULT, 22);
    let readonly_end = (&raw const __readonly_end) as u64;
    require(
        syscall(SYS_WRITE, readonly_end - 1, 2, console) == EFAULT,
        23,
    );
    require(syscall(u64::MAX, 0, 0, 0) == ENOSYS, 24);
    require(syscall(SYS_WRITE, 0, 0, console) == 0, 29);
    require(syscall(SYS_WRITE, CODE_BASE, 257, console) == EINVAL, 32);
    check_syscall_registers(pid);

    let initial_ticks = syscall(SYS_TICKS, 0, 0, 0);
    require(initial_ticks >= 0, 25);
    let mut previous_ticks = initial_ticks;
    let mut timer_progressed = false;
    for _ in 0..1_000_000 {
        let ticks = syscall(SYS_TICKS, 0, 0, 0);
        require(ticks >= previous_ticks, 26);
        if ticks > initial_ticks {
            timer_progressed = true;
            break;
        }
        previous_ticks = ticks;
        core::hint::spin_loop();
    }
    require(timer_progressed, 27);

    require(syscall(SYS_SLEEP, 1001, 0, 0) == EINVAL, 45);
    require(syscall(SYS_SLEEP, u64::MAX, 0, 0) == EINVAL, 46);
    require(syscall(SYS_SLEEP, 0, 0, 0) == 0, 47);
    check_private_state(pid, &stack_value);
    for _ in 0..4 {
        require(syscall(SYS_YIELD, 0, 0, 0) == 0, 36);
        check_private_state(pid, &stack_value);
        let before_sleep = syscall(SYS_TICKS, 0, 0, 0);
        require(before_sleep >= 0, 41);
        require(syscall(SYS_SLEEP, 2, 0, 0) == 0, 42);
        let after_sleep = syscall(SYS_TICKS, 0, 0, 0);
        require(after_sleep >= before_sleep, 43);
        require(after_sleep - before_sleep >= 2, 44);
        check_private_state(pid, &stack_value);
    }
    check_preempted_arithmetic(pid);
    check_private_state(pid, &stack_value);
    if role == 1 {
        // Let the primary reach its final probe first. In the CPU-bound case,
        // this peer must wake and finish while timer preemption handles the hog.
        let before_sleep = syscall(SYS_TICKS, 0, 0, 0);
        require(before_sleep >= 0, 48);
        require(syscall(SYS_SLEEP, 4, 0, 0) == 0, 49);
        let after_sleep = syscall(SYS_TICKS, 0, 0, 0);
        require(after_sleep >= before_sleep, 50);
        require(after_sleep - before_sleep >= 4, 51);
        check_private_state(pid, &stack_value);
    }

    if (10..=13).contains(&probe) {
        vm_reserve(
            VM_FAULT,
            1,
            if probe == 13 {
                PROT_NONE
            } else {
                PROT_READ_WRITE
            },
        );
        if probe != 13 {
            vm_write(VM_FAULT, 0xc3);
        }
        if probe == 10 {
            vm_protect(VM_FAULT, 1, PROT_READ);
        } else if probe == 12 {
            vm_release(VM_FAULT, 1);
        }
    }
    write(console, b"Mirror init: userspace checks passed\n");

    // Each boot runs at most one destructive protection probe. Assembly avoids
    // expressing deliberately invalid accesses as undefined Rust operations.
    match probe {
        0 | 8 | 9 => exit(0),
        // SAFETY: This read of the supervisor-only kernel mapping deliberately
        // raises #PF. The kernel ends the process at the fault without resuming it.
        1 => unsafe {
            asm!("mov rax, [rdi]", in("rdi") KERNEL_BASE, out("rax") _, options(nostack));
        },
        // SAFETY: This store to the read-execute code page deliberately raises
        // #PF. The kernel ends the process at the fault without resuming it.
        2 => unsafe {
            asm!("mov byte ptr [rdi], 0x90", in("rdi") CODE_BASE, options(nostack));
        },
        // SAFETY: Jumping into the RET byte pushed onto the non-executable stack
        // deliberately raises #PF. The kernel ends the process, so it never returns.
        3 => unsafe {
            // If execution protection is missing, RET will not give a false
            // success: the kernel must observe a page fault on this stack page.
            asm!("push 0xc3", "jmp rsp", options(noreturn));
        },
        // SAFETY: This read of the unmapped guard page below the initial stack
        // deliberately raises #PF. The kernel ends the process at the fault.
        4 => unsafe {
            asm!("mov rax, [rdi]", in("rdi") STACK_GUARD, out("rax") _, options(nostack));
        },
        // SAFETY: CLI at CPL 3 with IOPL 0 deliberately raises #GP instead of
        // masking interrupts. The kernel ends the process at the fault.
        5 => unsafe { asm!("cli", options(nostack)) },
        // SAFETY: This syscall deliberately enters the kernel with a noncanonical
        // RSP, and no Rust code runs afterward. The kernel rejects the return
        // frame and ends the process; UD2 traps if the syscall ever returns.
        6 => unsafe {
            // A syscall can arrive with an unusable user stack. The kernel must
            // reject this return frame against the native address contract.
            // No Rust code may run after changing RSP to the noncanonical value.
            asm!(
                "mov rsp, 0x0000800000000000",
                "mov rax, {ticks}",
                "int 0x80",
                "ud2",
                ticks = const SYS_TICKS,
                options(noreturn),
            );
        },
        7 => threads::budget(),
        // SAFETY: This store to the arena page made read-only above deliberately
        // raises #PF. The kernel ends the process at the fault without resuming it.
        10 => unsafe {
            asm!("mov byte ptr [rdi], 0x90", in("rdi") VM_FAULT, options(nostack));
        },
        // SAFETY: Jumping into the writable, non-executable arena page deliberately
        // raises #PF on the fetch. The kernel ends the process, so it never returns.
        11 => unsafe {
            asm!("jmp rdi", in("rdi") VM_FAULT, options(noreturn, nostack));
        },
        // SAFETY: This read of the released (12) or PROT_NONE (13) arena page
        // deliberately raises #PF. The kernel ends the process at the fault.
        12 | 13 => unsafe {
            asm!("mov rax, [rdi]", in("rdi") VM_FAULT, out("rax") _, options(nostack));
        },
        // SAFETY: This argument-free routine deliberately divides by zero with the
        // x87 exception unmasked, so FWAIT raises #MF and the kernel ends the process.
        14 => unsafe { fpu::mirror_probe_x87_fault() },
        // SAFETY: This argument-free routine deliberately divides by zero with the
        // SSE exception unmasked, so DIVSD raises #XM and the kernel ends the process.
        15 => unsafe { fpu::mirror_probe_sse_fault() },
        // SAFETY: This argument-free routine deliberately executes a VEX-encoded
        // instruction while the kernel leaves OSXSAVE clear, so it raises #UD and
        // the kernel ends the process.
        16 => unsafe { fpu::mirror_probe_avx_fault() },
        // SAFETY: This argument-free routine deliberately executes WRFSBASE while
        // the kernel leaves CR4.FSGSBASE clear, so it raises #UD and the kernel
        // ends the process.
        17 => unsafe { tls::mirror_probe_fsgsbase_fault() },
        18..=20 => threads::scenario(probe),
        // SAFETY: SYSCALL deliberately raises #UD while the kernel keeps
        // EFER.SCE clear. The kernel ends the process at the fault. SYSCALL
        // would write RCX and R11, so both are declared clobbered.
        21 => unsafe { asm!("syscall", out("rcx") _, out("r11") _, options(nostack)) },
        // SAFETY: SGDT at CPL3 deliberately raises #GP(0) while the kernel sets
        // CR4.UMIP, and the kernel ends the process at the fault. Without UMIP
        // it stores the 10-byte descriptor-table register in this local.
        22 => unsafe {
            let mut table = [0_u8; 10];
            asm!("sgdt [{}]", in(reg) table.as_mut_ptr(), options(nostack));
        },
        _ => exit(30),
    }
    // Reaching here means a protection probe unexpectedly succeeded.
    exit(31)
}

#[panic_handler]
fn panic(_info: &PanicInfo<'_>) -> ! {
    exit(99)
}

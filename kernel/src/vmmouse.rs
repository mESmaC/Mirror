//! VMware vmmouse absolute pointing over the backdoor I/O port (spec 0017c).
//!
//! The PS/2 mouse stays the transport. While the vmmouse is active, a PS/2
//! packet is only a doorbell: it queues nothing and triggers a drain of the
//! vmmouse queue (0017-D13). Absolute positions keep device units, 0..=0xFFFF
//! across the framebuffer (0017-D14); consumers convert them at use with
//! `mouse::to_pixel`. A runtime protocol error disables the device and returns
//! the mouse to relative PS/2 decoding until the next boot (0017-D16).
//!
//! Protocol references (no implementation copied): linux
//! `drivers/input/mouse/vmmouse.c` and `arch/x86/include/asm/vmware.h`;
//! https://github.com/qemu/qemu/blob/v10.1.0/hw/i386/vmmouse.c and
//! https://github.com/qemu/qemu/blob/v10.1.0/hw/i386/vmport.c

use core::arch::{asm, x86_64::__cpuid};
use core::sync::atomic::{AtomicBool, AtomicU8, Ordering};

use crate::mouse::{self, LEFT, MIDDLE, MouseEvent, PositionEvent, RIGHT};
use crate::{arch, serial};

const MAGIC: u32 = 0x564d_5868;
const PORT: u16 = 0x5658;
const GETVERSION: u32 = 10;
const DATA: u32 = 39;
const STATUS: u32 = 40;
const COMMAND: u32 = 41;
const RESTRICT: u32 = 86;
const READ_ID: u32 = 0x4541_4552;
const DISABLE: u32 = 0xf5;
const REQUEST_ABSOLUTE: u32 = 0x5342_4152;
const VERSION_ID: u32 = 0x3442_554a;
/// Linux `VMMOUSE_RESTRICT_CPL0`: commands 39-41 and 86 from CPL0 only.
const RESTRICT_CPL0: u32 = 1;
const HYPERVISOR: u32 = 1 << 31;
const PACKET_WORDS: u32 = 4;
/// Packets per drain (0017-D13). A longer queue sets the pending flag.
const DRAIN_PACKETS: u32 = 64;
const STATUS_ERROR: u32 = 0xffff;
const STATUS_LEFT: u32 = 0x20;
const STATUS_RIGHT: u32 = 0x10;
const STATUS_MIDDLE: u32 = 0x08;
const RELATIVE: u32 = 1 << 16;
const NO_FALLBACK: u8 = 0;

/// Why init left the mouse relative. Each is a content-free device marker.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Skip {
    NoMouse,
    NoHypervisor,
    NoBackdoor,
    NoDevice,
    Version,
    Status,
}

impl Skip {
    fn name(self) -> &'static str {
        match self {
            Self::NoMouse => "no_mouse",
            Self::NoHypervisor => "no_hypervisor",
            Self::NoBackdoor => "no_backdoor",
            Self::NoDevice => "no_device",
            Self::Version => "version",
            Self::Status => "status",
        }
    }
}

/// A runtime protocol error (0017-D16).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Fallback {
    Status = 1,
    QueueLength = 2,
}

/// The outcome of one drain.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Drain {
    /// The queue held at most one drain's worth of packets.
    Done,
    /// More than 64 packets were queued; the rest wait for the pending flag.
    More,
    /// The device was disabled after a protocol error.
    Failed(Fallback),
}

/// One backdoor call: the command in ECX and its argument in EBX. Returns EAX,
/// EBX, ECX and EDX. Self-tests substitute fakes; only `Port` does port I/O.
pub(crate) trait Backdoor {
    fn call(&mut self, command: u32, argument: u32) -> [u32; 4];
}

/// The real backdoor port.
pub(crate) struct Port {
    _private: (),
}

impl Port {
    /// # Safety
    /// The caller runs at CPL0 on the BSP with IF clear, CPUID.1:ECX bit 31
    /// (hypervisor) is set, and nothing else uses port 0x5658 concurrently.
    /// Once `init` has enabled the vmmouse, mouse doorbell mode records that
    /// the backdoor answered GETVERSION.
    pub(crate) unsafe fn new() -> Self {
        Self { _private: () }
    }
}

impl Backdoor for Port {
    fn call(&mut self, command: u32, argument: u32) -> [u32; 4] {
        // SAFETY: `Port::new`'s caller established CPL0, BSP, IF clear and a
        // hypervisor that answers port 0x5658 reads.
        unsafe { backdoor(command, argument) }
    }
}

/// One VMware backdoor call: EAX = magic, EBX = `argument`, ECX = `command`,
/// DX = 0x5658, then a 32-bit `in eax, dx`. Returns EAX to EDX; ESI and EDI are
/// clobbered (linux `arch/x86/include/asm/vmware.h`). LLVM reserves RBX for
/// inline assembly, so EBX is swapped through a scratch register. Only a 4-byte
/// access is used: QEMU's vmport implements only that width.
///
/// # Safety
/// CPL0 on the BSP with IF clear, and CPUID.1:ECX bit 31 set: the hypervisor
/// then services the read. Without a backdoor the read returns all ones and has
/// no side effect. No other code may use port 0x5658 concurrently.
unsafe fn backdoor(command: u32, argument: u32) -> [u32; 4] {
    let (eax, ebx, ecx, edx): (u64, u64, u64, u64);
    // SAFETY: The caller guarantees CPL0 and the hypervisor precondition. Full
    // 64-bit inputs leave no undefined upper bits for the hypervisor to read.
    // RBX is restored by the second XCHG; RSI and RDI are declared clobbered,
    // and flags and memory are not assumed preserved.
    unsafe {
        asm!(
            "xchg {swap}, rbx",
            "in eax, dx",
            "xchg {swap}, rbx",
            swap = inout(reg) u64::from(argument) => ebx,
            inout("rax") u64::from(MAGIC) => eax,
            inout("rcx") u64::from(command) => ecx,
            inout("rdx") u64::from(PORT) => edx,
            out("rsi") _,
            out("rdi") _,
            options(nostack),
        );
    }
    [eax as u32, ebx as u32, ecx as u32, edx as u32]
}

fn status_error(status: u32) -> bool {
    status >> 16 == STATUS_ERROR
}

/// GETVERSION with EBX preloaded to the magic's complement, so a port that
/// leaves EBX unchanged cannot pass (0017-D11); then the D12 enable sequence.
/// Any failure after the probe sends DISABLE, and the mouse stays relative.
fn enable(backdoor: &mut impl Backdoor) -> Result<(), Skip> {
    let [version, magic, ..] = backdoor.call(GETVERSION, !MAGIC);
    if magic != MAGIC || version == u32::MAX {
        return Err(Skip::NoBackdoor);
    }
    let result = request_absolute(backdoor);
    if result.is_err() {
        backdoor.call(COMMAND, DISABLE);
    }
    result
}

fn request_absolute(backdoor: &mut impl Backdoor) -> Result<(), Skip> {
    backdoor.call(COMMAND, READ_ID);
    let status = backdoor.call(STATUS, 0)[0];
    if status & 0xffff == 0 || status_error(status) {
        return Err(Skip::NoDevice);
    }
    if backdoor.call(DATA, 1)[0] != VERSION_ID {
        return Err(Skip::Version);
    }
    // QEMU does not register command 86; its answer is ignored (0017-D12).
    backdoor.call(RESTRICT, RESTRICT_CPL0);
    backdoor.call(COMMAND, REQUEST_ABSOLUTE);
    if status_error(backdoor.call(STATUS, 0)[0]) {
        return Err(Skip::Status);
    }
    Ok(())
}

/// Decode one DATA packet (0017-D14). Bit 16 marks a relative packet; an
/// absolute coordinate above 0xFFFF saturates. The wheel word is ignored.
pub(crate) fn decode([status, x, y, _wheel]: [u32; 4]) -> mouse::Event {
    let mut buttons = 0;
    for (bit, button) in [
        (STATUS_LEFT, LEFT),
        (STATUS_RIGHT, RIGHT),
        (STATUS_MIDDLE, MIDDLE),
    ] {
        if status & bit != 0 {
            buttons |= button;
        }
    }
    if status & RELATIVE != 0 {
        let saturate = |value: i64| value.clamp(i16::MIN.into(), i16::MAX.into()) as i16;
        mouse::Event::Relative(MouseEvent {
            dx: saturate(i64::from(x as i32)),
            dy: saturate(-i64::from(y as i32)),
            buttons,
        })
    } else {
        mouse::Event::Absolute(PositionEvent {
            x: x.min(0xffff) as u16,
            y: y.min(0xffff) as u16,
            buttons,
        })
    }
}

/// One drain (0017-D13): STATUS, then a four-dword DATA read per queued
/// packet, at most 64. A STATUS error or a queue length that is not a multiple
/// of 4 sends DISABLE (0017-D16); the caller records the pointer loss.
pub(crate) fn drain(backdoor: &mut impl Backdoor, mut publish: impl FnMut(mouse::Event)) -> Drain {
    let status = backdoor.call(STATUS, 0)[0];
    let length = status & 0xffff;
    let failure = if status_error(status) {
        Some(Fallback::Status)
    } else if !length.is_multiple_of(PACKET_WORDS) {
        Some(Fallback::QueueLength)
    } else {
        None
    };
    if let Some(reason) = failure {
        backdoor.call(COMMAND, DISABLE);
        return Drain::Failed(reason);
    }
    let packets = length / PACKET_WORDS;
    for _ in 0..packets.min(DRAIN_PACKETS) {
        publish(decode(backdoor.call(DATA, PACKET_WORDS)));
    }
    if packets > DRAIN_PACKETS {
        Drain::More
    } else {
        Drain::Done
    }
}

static PENDING: AtomicBool = AtomicBool::new(false);
static FALLBACK: AtomicU8 = AtomicU8::new(NO_FALLBACK);

/// Record a runtime drain's outcome with IF clear. This never prints: the IRQ
/// side only records a fallback, and `report_fallback` prints it.
pub(crate) fn record(result: Drain) {
    PENDING.store(result == Drain::More, Ordering::Relaxed);
    if let Drain::Failed(reason) = result {
        FALLBACK.store(reason as u8, Ordering::Relaxed);
    }
}

/// A drain stopped at its 64-packet bound; `ps2::poll_pending` continues it.
pub(crate) fn pending() -> bool {
    PENDING.load(Ordering::Relaxed)
}

/// Print a recorded runtime fallback once (0017-D16), from `ps2::next_event`.
/// A content-free device marker, printed in every build flavor.
pub(crate) fn report_fallback() {
    if FALLBACK.load(Ordering::Relaxed) == NO_FALLBACK {
        return;
    }
    let reason = match FALLBACK.swap(NO_FALLBACK, Ordering::Relaxed) {
        reason if reason == Fallback::Status as u8 => "status",
        reason if reason == Fallback::QueueLength as u8 => "queue_length",
        _ => return,
    };
    serial::println!("MIRROR_VMMOUSE_FALLBACK reason={}", reason);
}

/// Run once at boot after the PS/2 mouse result prints and before IRQ12 is
/// unmasked (0017-D11), with IF clear on the BSP. On success the mouse enters
/// doorbell mode; otherwise it stays relative.
pub fn init(mouse_ready: bool) {
    assert!(
        !arch::interrupts_enabled(),
        "vmmouse init requires IF clear"
    );
    let result = if !mouse_ready {
        Err(Skip::NoMouse)
    } else if __cpuid(1).ecx & HYPERVISOR == 0 {
        Err(Skip::NoHypervisor)
    } else {
        // SAFETY: Boot runs at CPL0 on the BSP with IF clear (asserted), the
        // hypervisor bit is set, and no other code uses port 0x5658.
        enable(&mut unsafe { Port::new() })
    };
    if let Err(skip) = result {
        serial::println!("MIRROR_VMMOUSE_SKIPPED reason={}", skip.name());
        return;
    }
    mouse::enter_doorbell();
    serial::println!("MIRROR_VMMOUSE_READY mode=absolute");
    #[cfg(feature = "boot-smoke")]
    if matches!(
        crate::boot::smoke_mode(),
        crate::boot::SmokeMode::VmmouseError
    ) {
        // Smoke fault VmmouseError: a DATA read from the empty queue, which
        // QEMU answers by disabling the device, then one drain as a doorbell
        // would run it. Its STATUS error must end in FALLBACK reason=status.
        let mut port = {
            // SAFETY: Same CPL0, BSP, IF-clear boot context; the enable above
            // found the backdoor.
            unsafe { Port::new() }
        };
        port.call(DATA, PACKET_WORDS);
        mouse::drain_vmmouse();
    }
}

/// A scripted backdoor: each call must match the next expected command and
/// argument, and returns its scripted registers.
#[cfg(feature = "boot-smoke")]
struct Script<'a> {
    steps: &'a [((u32, u32), [u32; 4])],
    next: usize,
}

#[cfg(feature = "boot-smoke")]
impl Backdoor for Script<'_> {
    fn call(&mut self, command: u32, argument: u32) -> [u32; 4] {
        let ((expected_command, expected_argument), registers) = self.steps[self.next];
        assert_eq!(
            (command, argument),
            (expected_command, expected_argument),
            "unexpected vmmouse call {}",
            self.next
        );
        self.next += 1;
        registers
    }
}

#[cfg(feature = "boot-smoke")]
impl Script<'_> {
    fn finished(&self) -> bool {
        self.next == self.steps.len()
    }
}

/// A fake device queue for the mouse ring tests: `remaining` packets of four
/// dwords, packet `n` at position (n, 2n), behind `stray` extra dwords that
/// make the queue length malformed, or a STATUS error.
#[cfg(feature = "boot-smoke")]
pub(crate) struct Stream {
    pub(crate) remaining: u32,
    next: u32,
    flags: u32,
    pub(crate) stray: u32,
    pub(crate) error: bool,
    pub(crate) disabled: bool,
    pub(crate) drains: usize,
}

#[cfg(feature = "boot-smoke")]
impl Stream {
    /// Packets with these mouse button levels, relative or absolute.
    pub(crate) fn new(packets: u32, buttons: u8, relative: bool) -> Self {
        let mut flags = if relative { RELATIVE } else { 0 };
        for (bit, button) in [
            (STATUS_LEFT, LEFT),
            (STATUS_RIGHT, RIGHT),
            (STATUS_MIDDLE, MIDDLE),
        ] {
            if buttons & button != 0 {
                flags |= bit;
            }
        }
        Self {
            remaining: packets,
            next: 0,
            flags,
            stray: 0,
            error: false,
            disabled: false,
            drains: 0,
        }
    }
}

#[cfg(feature = "boot-smoke")]
impl Backdoor for Stream {
    fn call(&mut self, command: u32, argument: u32) -> [u32; 4] {
        assert!(!self.disabled, "vmmouse call after DISABLE");
        match (command, argument) {
            (STATUS, 0) => {
                self.drains += 1;
                let high = if self.error { STATUS_ERROR << 16 } else { 0 };
                [high | (self.remaining * PACKET_WORDS + self.stray), 0, 0, 0]
            }
            (DATA, PACKET_WORDS) => {
                assert!(self.remaining != 0, "DATA read from an empty queue");
                self.remaining -= 1;
                let index = self.next;
                self.next += 1;
                [self.flags, index, index * 2, 0]
            }
            (COMMAND, DISABLE) => {
                self.disabled = true;
                [0; 4]
            }
            _ => panic!("unexpected vmmouse call"),
        }
    }
}

#[cfg(feature = "boot-smoke")]
pub fn self_test() {
    const MISSING: u32 = 0xffff_ffff;
    const PROBE: (u32, u32) = (GETVERSION, !MAGIC);
    const VERSION: [u32; 4] = [6, MAGIC, 0, 0];
    const IDLE: [u32; 4] = [0; 4];

    // Probe (0017-D11): a port that leaves EBX at the magic's complement, or
    // answers EAX all ones, has no backdoor; nothing else is sent to it.
    for registers in [
        [MISSING, !MAGIC, GETVERSION, u32::from(PORT)],
        [MISSING, MAGIC, 0, 0],
        [6, 0, 0, 0],
    ] {
        let steps = [(PROBE, registers)];
        let mut script = Script {
            steps: &steps,
            next: 0,
        };
        assert_eq!(enable(&mut script), Err(Skip::NoBackdoor));
        assert!(script.finished());
    }

    // Enable (0017-D12): the full sequence, and DISABLE after any failure.
    let read_id = ((COMMAND, READ_ID), IDLE);
    let ready = ((STATUS, 0), [1, 0, 0, 0]);
    let version = ((DATA, 1), [VERSION_ID, 0, 0, 0]);
    let restrict = ((RESTRICT, RESTRICT_CPL0), [MISSING, 0, 0, 0]);
    let absolute = ((COMMAND, REQUEST_ABSOLUTE), IDLE);
    let disable = ((COMMAND, DISABLE), IDLE);
    let steps = [
        (PROBE, VERSION),
        read_id,
        ready,
        version,
        restrict,
        absolute,
        ((STATUS, 0), IDLE),
    ];
    let mut script = Script {
        steps: &steps,
        next: 0,
    };
    assert_eq!(enable(&mut script), Ok(()));
    assert!(script.finished());
    for status in [0, 0xffff_0000, 0xffff_0001] {
        let steps = [
            (PROBE, VERSION),
            read_id,
            ((STATUS, 0), [status, 0, 0, 0]),
            disable,
        ];
        let mut script = Script {
            steps: &steps,
            next: 0,
        };
        assert_eq!(enable(&mut script), Err(Skip::NoDevice));
        assert!(script.finished());
    }
    let steps = [
        (PROBE, VERSION),
        read_id,
        ready,
        ((DATA, 1), [VERSION_ID + 1, 0, 0, 0]),
        disable,
    ];
    let mut script = Script {
        steps: &steps,
        next: 0,
    };
    assert_eq!(enable(&mut script), Err(Skip::Version));
    assert!(script.finished());
    let steps = [
        (PROBE, VERSION),
        read_id,
        ready,
        version,
        restrict,
        absolute,
        ((STATUS, 0), [0xffff_0000, 0, 0, 0]),
        disable,
    ];
    let mut script = Script {
        steps: &steps,
        next: 0,
    };
    assert_eq!(enable(&mut script), Err(Skip::Status));
    assert!(script.finished());

    // Runtime errors (0017-D16): status 0xFFFF0000 and a queue length of 6
    // publish nothing and send DISABLE.
    for (status, reason) in [
        (0xffff_0000, Fallback::Status),
        (0xffff_0004, Fallback::Status),
        (6, Fallback::QueueLength),
    ] {
        let steps = [((STATUS, 0), [status, 0, 0, 0]), disable];
        let mut script = Script {
            steps: &steps,
            next: 0,
        };
        let result = drain(&mut script, |_| panic!("published after an error"));
        assert_eq!(result, Drain::Failed(reason));
        assert!(script.finished());
    }
    let steps = [((STATUS, 0), IDLE)];
    let mut script = Script {
        steps: &steps,
        next: 0,
    };
    assert_eq!(drain(&mut script, |_| panic!("empty queue")), Drain::Done);
    assert!(script.finished());

    // Buttons map as D14 for both packet kinds; other status bits are ignored.
    // Status bits 3, 4 and 5 are MIDDLE, RIGHT and LEFT.
    let levels = [
        0,
        MIDDLE,
        RIGHT,
        RIGHT | MIDDLE,
        LEFT,
        LEFT | MIDDLE,
        LEFT | RIGHT,
        LEFT | RIGHT | MIDDLE,
    ];
    for (bits, expected) in (0..8_u32).zip(levels) {
        let status = bits << 3;
        assert_eq!(
            decode([status | 0xffc0_0007, 1, 2, 3]),
            mouse::Event::Absolute(PositionEvent {
                x: 1,
                y: 2,
                buttons: expected
            })
        );
        assert_eq!(
            decode([status | RELATIVE, 1, 2, 3]),
            mouse::Event::Relative(MouseEvent {
                dx: 1,
                dy: -2,
                buttons: expected
            })
        );
    }
    // Bit 16: dx = x and dy = -y as signed dwords, saturated to i16.
    for (x, y, dx, dy) in [
        (0, 0, 0, 0),
        (u32::MAX, u32::MAX, -1, 1),
        (0x7fff, 0x8000, i16::MAX, i16::MIN),
        (0x8000, 0x7fff, i16::MAX, -i16::MAX),
        ((-0x8000_i32) as u32, 0x8000_0000, i16::MIN, i16::MAX),
        (0x7fff_ffff, 0x7fff_ffff, i16::MAX, i16::MIN),
        (0x8000_0000, (-0x8001_i32) as u32, i16::MIN, i16::MAX),
    ] {
        assert_eq!(
            decode([RELATIVE, x, y, 0]),
            mouse::Event::Relative(MouseEvent { dx, dy, buttons: 0 })
        );
    }
    // Absolute coordinates keep device units; larger values saturate.
    for (value, expected) in [
        (0, 0),
        (0xfffe, 0xfffe),
        (0xffff, 0xffff),
        (0x1_0000, 0xffff),
        (u32::MAX, 0xffff),
    ] {
        assert_eq!(
            decode([0, value, value, 0]),
            mouse::Event::Absolute(PositionEvent {
                x: expected,
                y: expected,
                buttons: 0
            })
        );
    }

    // 0 and 0xFFFF are the first and last pixel (0017-D14).
    for width in [1_usize, 1280, 1920] {
        assert_eq!(mouse::to_pixel(0, width - 1), 0);
        assert_eq!(mouse::to_pixel(0xffff, width - 1), width - 1);
    }
    // QMP's 0..0x7FFF range, scaled by x = v * 0xFFFF / 0x7FFF, round-trips
    // every pixel p when v = round(p * 0x7FFF / (W - 1)).
    for width in [1280_u64, 1920, 3840] {
        let last = width - 1;
        for pixel in 0..width {
            let qmp = (2 * pixel * 0x7fff + last) / (2 * last);
            let device = u16::try_from(qmp * 0xffff / 0x7fff).expect("device unit");
            assert_eq!(mouse::to_pixel(device, last as usize), pixel as usize);
        }
    }
    // The widest extent cannot overflow and still ends at its last pixel.
    assert_eq!(mouse::to_pixel(0xffff, usize::MAX), usize::MAX);
    assert_eq!(mouse::to_pixel(1, usize::MAX), usize::MAX / 0xffff);

    // Ring, doorbell and fallback behavior through the mouse decoder state.
    mouse::doorbell_self_test();
    assert!(!pending());
    serial::println!("MIRROR_VMMOUSE_TESTS_OK");
}

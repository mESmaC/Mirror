//! Machine restart without AML (spec 0058a): the FADT reset register, the
//! 8042 reset pulse, port 0xCF9, then a triple fault (ledger S16), each tried
//! once in a single pass that ends in the one method x86 cannot survive (D1).
//!
//! Contract: BSP only, IF clear, after `timer::init` (the stopwatch's PIT
//! fallback), with every other CPU idle in `smp.rs`. The crash owner (spec
//! 0051, point 12) may also restart, on any CPU, once it has stopped every
//! other CPU it could, and its lines skip the serial lock. Nothing is mapped,
//! allocated or read from firmware here: `init` stored the register at boot
//! (D3). This module owns port 0xCF9 (D5) and, once restart begins, COM1; it
//! reaches the i8042 only through `ps2` (D4). Each step prints a content-free
//! lifecycle marker in every flavor (D8), and every wait is bounded by the
//! stopwatch with `timer::RESET_POLL_LIMIT` as its backstop (D7).
//!
//! References, read for how others reset x86 (nothing copied): Linux
//! arch/x86/kernel/reboot.c and FreeBSD sys/x86/x86/cpu_machdep.c.

use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use mirror_acpi::{FadtReset, KBC_COMMAND_PORT, KBC_RESET_PULSE, RESET_CONTROL_PORT};

use crate::timer::{RESET_POLL_LIMIT, RESET_WAIT_MS, Stopwatch};
use crate::{arch, interrupts, ps2, serial};

const NANOS_PER_MS: u64 = 1_000_000;
/// RST_CNT bit 1 selects a hard (platform) reset; bit 2 starts the reset.
const SYSTEM_RESET: u8 = 0x02;
const RESET_CPU: u8 = 0x04;

/// The validated register as `port << 8 | value`; 0 means none (ports are
/// never 0). Written once by `init`, before any AP or user code runs.
static REGISTER: AtomicU32 = AtomicU32::new(0);
static INITIALIZED: AtomicBool = AtomicBool::new(false);

/// Why the machine restarts, as `MIRROR_RESET_BEGIN cause=` reports it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cause {
    /// The kernel console's `restart` command.
    #[cfg_attr(feature = "boot-smoke", allow(dead_code))]
    Console,
    /// A boot-smoke restart case (D11).
    #[cfg(feature = "boot-smoke")]
    Test,
    /// The crash path's reboot (spec 0051, point 12).
    Panic,
}

impl Cause {
    fn name(self) -> &'static str {
        match self {
            Self::Console => "console",
            #[cfg(feature = "boot-smoke")]
            Self::Test => "test",
            Self::Panic => "panic",
        }
    }
}

/// S16's methods in order. The derived order is the chain's.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Method {
    Fadt,
    Kbc,
    Cf9,
    TripleFault,
}

impl Method {
    fn name(self) -> &'static str {
        match self {
            Self::Fadt => "fadt",
            Self::Kbc => "kbc",
            Self::Cf9 => "cf9",
            Self::TripleFault => "triple_fault",
        }
    }
}

/// Why a method writes nothing (D8).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Reason {
    /// The FADT gave no usable register (`MIRROR_ACPI_RESET_SKIPPED`).
    Unavailable,
    /// The 8042 status port reads 0xFF.
    Absent,
    /// The 8042 input buffer stayed full.
    Busy,
    /// A boot-smoke case starts the chain at a later method.
    #[cfg(feature = "boot-smoke")]
    TestSkip,
}

impl Reason {
    fn name(self) -> &'static str {
        match self {
            Self::Unavailable => "unavailable",
            Self::Absent => "absent",
            Self::Busy => "busy",
            #[cfg(feature = "boot-smoke")]
            Self::TestSkip => "test_skip",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Register {
    port: u16,
    value: u8,
}

impl Register {
    fn encode(self) -> u32 {
        u32::from(self.port) << 8 | u32::from(self.value)
    }

    fn decode(raw: u32) -> Option<Self> {
        let port = (raw >> 8) as u16;
        (port != 0).then_some(Self {
            port,
            value: raw as u8,
        })
    }
}

impl From<FadtReset> for Register {
    fn from(reset: FadtReset) -> Self {
        Self {
            port: reset.port,
            value: reset.value,
        }
    }
}

/// How one method runs, decided before any I/O.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Step {
    Unavailable,
    /// The 8042 pulse, which `ps2` may still skip as absent or busy.
    Pulse,
    /// Two writes to 0xCF9: the hard-reset select latched first, then the
    /// reset bit (D5). VMware's PIIX4 needs bit 1 latched before bit 2.
    ResetControl {
        first: u8,
        second: u8,
    },
    TripleFault,
}

/// D2 and D5: the FADT step uses the stored register's own shape, the `cf9`
/// step always writes 0x02 then 0x06, and the 8042 step always pulses.
fn plan(method: Method, register: Option<Register>) -> Step {
    match (method, register) {
        (Method::Fadt, Some(Register { port, value })) if port == RESET_CONTROL_PORT => {
            Step::ResetControl {
                first: value & !RESET_CPU,
                second: value,
            }
        }
        (Method::Fadt, Some(Register { port, value }))
            if port == KBC_COMMAND_PORT && value == KBC_RESET_PULSE =>
        {
            Step::Pulse
        }
        (Method::Fadt, _) => Step::Unavailable,
        (Method::Kbc, _) => Step::Pulse,
        (Method::Cf9, _) => Step::ResetControl {
            first: SYSTEM_RESET,
            second: SYSTEM_RESET | RESET_CPU,
        },
        (Method::TripleFault, _) => Step::TripleFault,
    }
}

/// The I/O and reporting side of a restart: hardware for a real one, a
/// recorder for the policy self-test.
trait Machine {
    fn skipped(&mut self, method: Method, reason: Reason);
    /// Announce a method before its first write.
    fn attempt(&mut self, method: Method);
    /// D4: wait for the 8042's input buffer, writing nothing.
    fn prepare_pulse(&mut self) -> Result<(), ps2::ResetSkip>;
    fn write(&mut self, method: Method, port: u16, value: u8);
    /// D7: wait 500 ms after a method's writes, then report that it failed.
    fn timed_out(&mut self, method: Method);
}

/// The whole sequence, once (D1). Methods before `first` are skipped as
/// `test_skip`; only boot-smoke cases pass anything but `Fadt`. Returns after
/// announcing the triple fault, which the caller then raises.
fn run(machine: &mut impl Machine, register: Option<Register>, first: Method) {
    for method in [Method::Fadt, Method::Kbc, Method::Cf9] {
        if method < first {
            #[cfg(feature = "boot-smoke")]
            machine.skipped(method, Reason::TestSkip);
            continue;
        }
        match plan(method, register) {
            Step::Unavailable => machine.skipped(method, Reason::Unavailable),
            Step::Pulse => match machine.prepare_pulse() {
                Err(ps2::ResetSkip::Absent) => machine.skipped(method, Reason::Absent),
                Err(ps2::ResetSkip::Busy) => machine.skipped(method, Reason::Busy),
                Ok(()) => {
                    machine.attempt(method);
                    machine.write(method, KBC_COMMAND_PORT, KBC_RESET_PULSE);
                    machine.timed_out(method);
                }
            },
            Step::ResetControl { first, second } => {
                machine.attempt(method);
                machine.write(method, RESET_CONTROL_PORT, first);
                machine.write(method, RESET_CONTROL_PORT, second);
                machine.timed_out(method);
            }
            Step::TripleFault => unreachable!("the triple fault ends the chain"),
        }
    }
    machine.attempt(Method::TripleFault);
}

/// Store the FADT register that discovery validated (D3). Once, on the BSP
/// with IF clear, after `acpi::discover` and before AP release. Prints nothing.
pub fn init(register: Option<FadtReset>) {
    assert!(!arch::interrupts_enabled(), "reset init requires IF clear");
    assert!(
        !INITIALIZED.swap(true, Ordering::AcqRel),
        "reset already initialized"
    );
    if let Some(register) = register {
        REGISTER.store(Register::from(register).encode(), Ordering::Release);
    }
}

/// Whether `init` ran, so `restart` may (the crash path's reboot asks).
pub(crate) fn is_initialized() -> bool {
    INITIALIZED.load(Ordering::Acquire)
}

/// Restart the machine (D1). Never returns: the chain ends in a triple fault.
pub fn restart(cause: Cause) -> ! {
    arch::disable_interrupts();
    assert!(
        !crate::smp::is_secondary() || crate::crash::is_owner(),
        "restart runs on the BSP or the crash owner"
    );
    assert!(INITIALIZED.load(Ordering::Acquire), "restart before init");
    serial::println!("MIRROR_RESET_BEGIN cause={}", cause.name());
    let register = Register::decode(REGISTER.load(Ordering::Acquire));
    #[cfg(feature = "boot-smoke")]
    let (first, suppressed) = match (cause, crate::boot::smoke_mode()) {
        (Cause::Test, crate::boot::SmokeMode::Reset(test)) => match test {
            crate::boot::ResetTest::Fadt => (Method::Fadt, false),
            crate::boot::ResetTest::Kbc => (Method::Kbc, false),
            crate::boot::ResetTest::Cf9 => (Method::Cf9, false),
            crate::boot::ResetTest::TripleFault => (Method::TripleFault, false),
            crate::boot::ResetTest::Suppressed => (Method::Fadt, true),
        },
        _ => (Method::Fadt, false),
    };
    #[cfg(not(feature = "boot-smoke"))]
    let first = Method::Fadt;
    let mut hardware = Hardware {
        stopwatch: Stopwatch::start_restart(),
        pulse: None,
        #[cfg(feature = "boot-smoke")]
        suppressed,
    };
    run(&mut hardware, register, first);
    interrupts::trigger_triple_fault()
}

struct Hardware {
    stopwatch: Stopwatch,
    pulse: Option<ps2::ResetPulse>,
    /// `reset-suppressed`: report each write instead of making it.
    #[cfg(feature = "boot-smoke")]
    suppressed: bool,
}

impl Machine for Hardware {
    fn skipped(&mut self, method: Method, reason: Reason) {
        serial::println!(
            "MIRROR_RESET_SKIPPED method={} reason={}",
            method.name(),
            reason.name()
        );
    }

    fn attempt(&mut self, method: Method) {
        serial::println!("MIRROR_RESET_ATTEMPT method={}", method.name());
        // The line must leave the UART before a write that may end the VM.
        serial::flush();
    }

    fn prepare_pulse(&mut self) -> Result<(), ps2::ResetSkip> {
        self.pulse = Some(ps2::reset_prepare(&mut self.stopwatch)?);
        Ok(())
    }

    fn write(&mut self, method: Method, port: u16, value: u8) {
        #[cfg(feature = "boot-smoke")]
        if self.suppressed {
            self.pulse = None;
            serial::println!(
                "MIRROR_RESET_SUPPRESSED method={} port={:#x} value={:#x}",
                method.name(),
                port,
                value
            );
            return;
        }
        let _ = method;
        if port == KBC_COMMAND_PORT {
            assert_eq!(value, KBC_RESET_PULSE);
            self.pulse.take().expect("prepared 8042 pulse").pulse();
        } else {
            assert_eq!(port, RESET_CONTROL_PORT);
            // SAFETY: This module owns port 0xCF9 (pci.rs uses only CF8 and
            // CFC). The value is D5's or a validated FADT value, the BSP runs
            // with IF clear and every other CPU idles, and a restart never
            // returns to code that depends on the machine staying up.
            unsafe { arch::out8(RESET_CONTROL_PORT, value) };
        }
    }

    fn timed_out(&mut self, method: Method) {
        let start_ns = self.stopwatch.elapsed_ns();
        let mut waited_ns = 0;
        for _ in 0..RESET_POLL_LIMIT {
            waited_ns = self.stopwatch.elapsed_ns().saturating_sub(start_ns);
            if waited_ns >= RESET_WAIT_MS * NANOS_PER_MS {
                break;
            }
            core::hint::spin_loop();
        }
        serial::println!(
            "MIRROR_RESET_TIMEOUT method={} elapsed_ms={} clock={}",
            method.name(),
            waited_ns / NANOS_PER_MS,
            self.stopwatch.name()
        );
    }
}

/// Test 3: the plan and the whole chain against a recorder, for every first
/// method, register state and 8042 outcome, and D4's choice.
#[cfg(feature = "boot-smoke")]
pub fn policy_self_test() {
    use alloc::vec::Vec;

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum Event {
        Skipped(Method, Reason),
        Attempt(Method),
        Write(Method, u16, u8),
        Timeout(Method),
    }

    struct Recorder {
        pulse: Result<(), ps2::ResetSkip>,
        prepared: bool,
        events: Vec<Event>,
    }

    impl Machine for Recorder {
        fn skipped(&mut self, method: Method, reason: Reason) {
            self.events.push(Event::Skipped(method, reason));
        }
        fn attempt(&mut self, method: Method) {
            self.events.push(Event::Attempt(method));
        }
        fn prepare_pulse(&mut self) -> Result<(), ps2::ResetSkip> {
            assert!(!self.prepared, "one 8042 wait per pulse");
            self.prepared = self.pulse.is_ok();
            self.pulse
        }
        fn write(&mut self, method: Method, port: u16, value: u8) {
            if port == KBC_COMMAND_PORT {
                // Every pulse follows its own successful wait.
                assert!(core::mem::take(&mut self.prepared));
            }
            self.events.push(Event::Write(method, port, value));
        }
        fn timed_out(&mut self, method: Method) {
            self.events.push(Event::Timeout(method));
        }
    }

    fn record(
        register: Option<Register>,
        first: Method,
        pulse: Result<(), ps2::ResetSkip>,
    ) -> Vec<Event> {
        let mut recorder = Recorder {
            pulse,
            prepared: false,
            events: Vec::new(),
        };
        run(&mut recorder, register, first);
        assert!(!recorder.prepared, "a prepared pulse was not written");
        recorder.events
    }

    use Event::{Attempt, Skipped, Timeout, Write};
    use Method::{Cf9, Fadt, Kbc, TripleFault};
    let qemu = Register::from(FadtReset {
        revision: 3,
        port: 0xcf9,
        value: 0x0f,
    });
    let vmware = Register::from(FadtReset {
        revision: 4,
        port: 0xcf9,
        value: 0x06,
    });
    let vmware_kbc = Register::from(FadtReset {
        revision: 4,
        port: 0x64,
        value: 0xfe,
    });
    for register in [qemu, vmware, vmware_kbc] {
        assert_eq!(Register::decode(register.encode()), Some(register));
    }
    assert_eq!(Register::decode(0), None);

    // D2 and D5: the FADT step writes the register's value with bit 2 clear,
    // then the value; 0x64 pulses; cf9 writes 0x02 then 0x06.
    assert_eq!(
        plan(Fadt, Some(qemu)),
        Step::ResetControl {
            first: 0x0b,
            second: 0x0f
        }
    );
    assert_eq!(
        plan(Fadt, Some(vmware)),
        Step::ResetControl {
            first: 0x02,
            second: 0x06
        }
    );
    assert_eq!(plan(Fadt, Some(vmware_kbc)), Step::Pulse);
    assert_eq!(plan(Fadt, None), Step::Unavailable);
    for register in [None, Some(qemu), Some(vmware_kbc)] {
        assert_eq!(plan(Kbc, register), Step::Pulse);
        assert_eq!(
            plan(Cf9, register),
            Step::ResetControl {
                first: 0x02,
                second: 0x06
            }
        );
    }

    // The full chain with QEMU's register, as reset-suppressed runs it.
    assert_eq!(
        record(Some(qemu), Fadt, Ok(())),
        [
            Attempt(Fadt),
            Write(Fadt, 0xcf9, 0x0b),
            Write(Fadt, 0xcf9, 0x0f),
            Timeout(Fadt),
            Attempt(Kbc),
            Write(Kbc, 0x64, 0xfe),
            Timeout(Kbc),
            Attempt(Cf9),
            Write(Cf9, 0xcf9, 0x02),
            Write(Cf9, 0xcf9, 0x06),
            Timeout(Cf9),
            Attempt(TripleFault),
        ]
    );
    // VMware without acpi.exposeHardReset: two pulses, then 0xCF9.
    assert_eq!(
        record(Some(vmware_kbc), Fadt, Ok(()))[..3],
        [Attempt(Fadt), Write(Fadt, 0x64, 0xfe), Timeout(Fadt)]
    );
    // No register, and an absent controller (-NoKeyboard).
    assert_eq!(
        record(None, Fadt, Err(ps2::ResetSkip::Absent)),
        [
            Skipped(Fadt, Reason::Unavailable),
            Skipped(Kbc, Reason::Absent),
            Attempt(Cf9),
            Write(Cf9, 0xcf9, 0x02),
            Write(Cf9, 0xcf9, 0x06),
            Timeout(Cf9),
            Attempt(TripleFault),
        ]
    );
    // A busy controller skips both pulses without writing.
    assert_eq!(
        record(Some(vmware_kbc), Fadt, Err(ps2::ResetSkip::Busy))[..2],
        [Skipped(Fadt, Reason::Busy), Skipped(Kbc, Reason::Busy)]
    );
    // The test cases' first methods.
    assert_eq!(
        record(Some(qemu), Kbc, Err(ps2::ResetSkip::Absent))[..3],
        [
            Skipped(Fadt, Reason::TestSkip),
            Skipped(Kbc, Reason::Absent),
            Attempt(Cf9)
        ]
    );
    assert_eq!(
        record(Some(qemu), TripleFault, Ok(())),
        [
            Skipped(Fadt, Reason::TestSkip),
            Skipped(Kbc, Reason::TestSkip),
            Skipped(Cf9, Reason::TestSkip),
            Attempt(TripleFault),
        ]
    );

    // Every combination keeps the chain's shape: each of the three methods
    // is skipped or attempted once, in order; only an attempt writes, only to
    // 0xCF9 or 0x64, and it ends in a timeout; the triple fault comes last.
    for register in [None, Some(qemu), Some(vmware), Some(vmware_kbc)] {
        for first in [Fadt, Kbc, Cf9, TripleFault] {
            for pulse in [
                Ok(()),
                Err(ps2::ResetSkip::Absent),
                Err(ps2::ResetSkip::Busy),
            ] {
                let events = record(register, first, pulse);
                assert_eq!(events.last(), Some(&Attempt(TripleFault)));
                let mut index = 0;
                for method in [Fadt, Kbc, Cf9] {
                    match events[index] {
                        Skipped(skipped, reason) => {
                            assert_eq!(skipped, method);
                            assert_eq!(reason == Reason::TestSkip, method < first);
                            index += 1;
                        }
                        Attempt(attempted) => {
                            assert!(attempted == method && method >= first);
                            index += 1;
                            let writes = events[index..]
                                .iter()
                                .take_while(|event| matches!(event, Write(..)))
                                .count();
                            for event in &events[index..index + writes] {
                                let Write(writer, port, value) = *event else {
                                    unreachable!()
                                };
                                assert_eq!(writer, method);
                                match port {
                                    0x64 => assert_eq!((writes, value), (1, 0xfe)),
                                    0xcf9 => assert!(writes == 2 && value & 0xf2 == 0x02),
                                    _ => panic!("reset write to another port"),
                                }
                            }
                            if let Write(_, 0xcf9, value) = events[index] {
                                assert_eq!(value & RESET_CPU, 0, "reset bit before select");
                                let Write(_, _, second) = events[index + 1] else {
                                    unreachable!()
                                };
                                assert_ne!(second & RESET_CPU, 0, "no reset bit");
                            }
                            index += writes;
                            assert_eq!(events[index], Timeout(method));
                            index += 1;
                        }
                        other => panic!("unexpected reset event {other:?}"),
                    }
                }
                assert_eq!(index + 1, events.len());
            }
        }
    }

    // D4: status 0xFF is absent; a full input buffer waits until 500 ms and
    // is then busy; an empty buffer is ready, even on a read after expiry.
    const INPUT_FULL: u8 = 2;
    for waited in [0, 499, 500, 10_000] {
        assert_eq!(
            ps2::reset_choice(0xff, waited),
            Some(Err(ps2::ResetSkip::Absent))
        );
        assert_eq!(ps2::reset_choice(0x1c, waited), Some(Ok(())));
        assert_eq!(
            ps2::reset_choice(INPUT_FULL | 0x1c, waited),
            (waited >= 500).then_some(Err(ps2::ResetSkip::Busy))
        );
    }
    serial::println!("MIRROR_RESET_POLICY_OK");
}

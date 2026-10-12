//! Bounded application surfaces and routed input, independent of window policy.
//! Pixel pages are kernel-owned, never user-mapped, and explicitly reclaimed.

use crate::memory::{FrameAllocator, PAGE_SIZE};
use mirror_window_protocol as protocol;

pub const APP_COUNT: usize = protocol::MAX_APPS;
pub const WIDTH: usize = protocol::WIDTH;
pub const HEIGHT: usize = protocol::HEIGHT;
pub const EDGE: usize = protocol::TILE;
pub const TILE_PACKET: usize = 16 + EDGE * EDGE * 4;
pub const READ_PACKET: usize = 24 + EDGE * EDGE * 4;
pub const INFO_SIZE: usize = 40;
pub const EVENT_SIZE: usize = 32;
const PIXEL_BYTES: usize = WIDTH * HEIGHT * 4;
const PIXEL_PAGES: usize = PIXEL_BYTES / PAGE_SIZE as usize;
pub const SURFACE_PAGES: usize = 2 * PIXEL_PAGES;
const TILES: usize = WIDTH / EDGE * (HEIGHT / EDGE);
/// Routed records an app can hold, not counting pointer Resets.
const QUEUE_EVENTS: usize = 32;
/// Queued records plus the pointer Resets between them. As in the shell's
/// queue, a Reset is a marker, not an event: it never counts against
/// `QUEUE_EVENTS`, and two never sit side by side, so one slot more than twice
/// the capacity holds every queue (spec 0017a-D3).
const QUEUE_SLOTS: usize = 2 * QUEUE_EVENTS + 1;
const _: () = assert!(PIXEL_BYTES.is_multiple_of(PAGE_SIZE as usize));

struct Pixels {
    frames: [u64; PIXEL_PAGES],
    count: usize,
    direct_map: u64,
}

impl Pixels {
    /// # Safety
    /// Sole BSP, IF=0, unique allocator, and a persistent writable HHDM for RAM.
    unsafe fn allocate(frames: &mut FrameAllocator<'_>, direct_map: u64) -> Result<Self, i64> {
        let mut pixels = Self {
            frames: [0; PIXEL_PAGES],
            count: 0,
            direct_map,
        };
        for index in 0..PIXEL_PAGES {
            let Some(frame) = frames.allocate() else {
                // SAFETY: These unexposed pages have no aliases except the HHDM.
                unsafe { pixels.destroy(frames) };
                return Err(-12);
            };
            pixels.frames[index] = frame;
            pixels.count += 1;
            let Some(end) = direct_map
                .checked_add(frame)
                .and_then(|start| start.checked_add(PAGE_SIZE - 1))
            else {
                // SAFETY: No owned page has been exposed to users or devices.
                unsafe { pixels.destroy(frames) };
                return Err(-12);
            };
            // SAFETY: This freshly allocated page is exclusively owned, fully
            // mapped by the caller's HHDM and checked against address overflow.
            unsafe { ((end - PAGE_SIZE + 1) as *mut u8).write_bytes(0, PAGE_SIZE as usize) };
        }
        Ok(pixels)
    }

    fn write(&mut self, mut offset: usize, mut source: &[u8]) {
        assert!(offset <= PIXEL_BYTES && source.len() <= PIXEL_BYTES - offset);
        while !source.is_empty() {
            let page = offset / PAGE_SIZE as usize;
            let within = offset % PAGE_SIZE as usize;
            let length = source.len().min(PAGE_SIZE as usize - within);
            let target = self.direct_map + self.frames[page] + within as u64;
            // SAFETY: Construction owns every backing page for this lifetime.
            // Bounds are checked; the stack packet cannot alias owned pixels.
            unsafe { core::ptr::copy_nonoverlapping(source.as_ptr(), target as *mut u8, length) };
            source = &source[length..];
            offset += length;
        }
    }

    fn read(&self, mut offset: usize, mut destination: &mut [u8]) {
        assert!(offset <= PIXEL_BYTES && destination.len() <= PIXEL_BYTES - offset);
        while !destination.is_empty() {
            let page = offset / PAGE_SIZE as usize;
            let within = offset % PAGE_SIZE as usize;
            let length = destination.len().min(PAGE_SIZE as usize - within);
            let source = self.direct_map + self.frames[page] + within as u64;
            // SAFETY: The checked span lies in initialized owned pages. The
            // destination is a distinct kernel packet, never a user pointer.
            unsafe {
                core::ptr::copy_nonoverlapping(
                    source as *const u8,
                    destination.as_mut_ptr(),
                    length,
                )
            };
            destination = &mut destination[length..];
            offset += length;
        }
    }

    /// # Safety
    /// IF=0 on the sole BSP; no live read/write operation or user aliases.
    unsafe fn destroy(&mut self, frames: &mut FrameAllocator<'_>) {
        for &frame in &self.frames[..self.count] {
            // SAFETY: These pages have never been mapped outside the dormant
            // HHDM or exposed to DMA, and the caller excluded all live accesses.
            unsafe { frames.deallocate(frame) }.expect("release application pixel page");
        }
        self.count = 0;
    }
}

pub struct Application {
    pub pid: u64,
    pub kind: u32,
    pub state: u32,
    pub detail: u64,
    pub revision: u64,
    front: Pixels,
    back: Pixels,
    coverage: [u64; TILES.div_ceil(64)],
    covered: usize,
    events: [[u8; EVENT_SIZE]; QUEUE_SLOTS],
    head: usize,
    length: usize,
    /// Queued records other than Reset markers; at most `QUEUE_EVENTS`.
    real: usize,
}

impl Application {
    /// # Safety
    /// Same unique allocator and persistent HHDM contract as Pixels::allocate.
    pub unsafe fn new(
        pid: u64,
        kind: u32,
        frames: &mut FrameAllocator<'_>,
        direct_map: u64,
    ) -> Result<Self, i64> {
        // SAFETY: The scheduler supplies the unique allocator with IF clear.
        let mut front = unsafe { Pixels::allocate(frames, direct_map) }?;
        // SAFETY: This second buffer acquires distinct pages from that allocator.
        let back = match unsafe { Pixels::allocate(frames, direct_map) } {
            Ok(back) => back,
            Err(error) => {
                // SAFETY: The first buffer was never published or mapped.
                unsafe { front.destroy(frames) };
                return Err(error);
            }
        };
        let mut application = Self {
            pid,
            kind,
            state: 1,
            detail: 0,
            revision: 0,
            front,
            back,
            coverage: [0; TILES.div_ceil(64)],
            covered: 0,
            events: [[0; EVENT_SIZE]; QUEUE_SLOTS],
            head: 0,
            length: 0,
            real: 0,
        };
        application.push(reset_event());
        Ok(application)
    }

    pub fn info(&self) -> [u8; INFO_SIZE] {
        let mut info = [0; INFO_SIZE];
        info[..8].copy_from_slice(&self.pid.to_le_bytes());
        info[8..16].copy_from_slice(&self.revision.to_le_bytes());
        for (bytes, value) in info[16..32].as_chunks_mut::<4>().0.iter_mut().zip([
            self.state,
            WIDTH as u32,
            HEIGHT as u32,
            self.kind,
        ]) {
            bytes.copy_from_slice(&value.to_le_bytes());
        }
        info[32..].copy_from_slice(&self.detail.to_le_bytes());
        info
    }

    pub fn surface_info() -> [u8; 24] {
        let mut info = [0; 24];
        for (bytes, value) in info.as_chunks_mut::<4>().0.iter_mut().zip([
            1_u32,
            WIDTH as u32,
            HEIGHT as u32,
            1,
            EDGE as u32,
            0,
        ]) {
            bytes.copy_from_slice(&value.to_le_bytes());
        }
        info
    }

    pub fn present(&mut self, packet: &[u8]) -> Result<(), i64> {
        if self.state != 1 {
            return Err(-32);
        }
        if packet.len() != TILE_PACKET {
            return Err(-22);
        }
        let x = word(packet, 0) as usize;
        let y = word(packet, 4) as usize;
        if word(packet, 8) != EDGE as u32
            || word(packet, 12) != EDGE as u32
            || !x.is_multiple_of(EDGE)
            || !y.is_multiple_of(EDGE)
            || x > WIDTH - EDGE
            || y > HEIGHT - EDGE
        {
            return Err(-22);
        }
        for row in 0..EDGE {
            self.back.write(
                ((y + row) * WIDTH + x) * 4,
                &packet[16 + row * EDGE * 4..16 + (row + 1) * EDGE * 4],
            );
        }
        let tile = y / EDGE * (WIDTH / EDGE) + x / EDGE;
        let mask = 1_u64 << (tile % 64);
        if self.coverage[tile / 64] & mask == 0 {
            self.coverage[tile / 64] |= mask;
            self.covered += 1;
        }
        Ok(())
    }

    pub fn commit(&mut self) -> Result<u64, i64> {
        if self.state != 1 {
            return Err(-32);
        }
        if self.covered != TILES {
            return Err(-11);
        }
        let revision = self
            .revision
            .checked_add(1)
            .filter(|value| *value <= i64::MAX as u64)
            .ok_or(-75)?;
        core::mem::swap(&mut self.front, &mut self.back);
        self.revision = revision;
        self.coverage.fill(0);
        self.covered = 0;
        Ok(revision)
    }

    pub fn read(&self, packet: &mut [u8]) -> Result<(), i64> {
        if !(28..=READ_PACKET).contains(&packet.len()) {
            return Err(-22);
        }
        let revision = u64::from_le_bytes(packet[..8].try_into().unwrap());
        let x = word(packet, 8) as usize;
        let y = word(packet, 12) as usize;
        let width = word(packet, 16) as usize;
        let height = word(packet, 20) as usize;
        if !(1..=EDGE).contains(&width)
            || !(1..=EDGE).contains(&height)
            || x > WIDTH - width
            || y > HEIGHT - height
            || packet.len() != 24 + width * height * 4
        {
            return Err(-22);
        }
        if revision != self.revision {
            return Err(-11);
        }
        for row in 0..height {
            self.front.read(
                ((y + row) * WIDTH + x) * 4,
                &mut packet[24 + row * width * 4..24 + (row + 1) * width * 4],
            );
        }
        Ok(())
    }

    /// Queue one validated routed record, or push back with -11 while 32
    /// records other than Resets are queued. Nothing queued is ever dropped
    /// here: the shell keeps a refused record and applies the only loss policy.
    /// A pointer Reset is a marker that takes no capacity, and one right after
    /// another adds nothing, so a pointer loss never costs a key (0017a-D3).
    pub fn enqueue(&mut self, event: [u8; EVENT_SIZE]) -> Result<(), i64> {
        if self.state != 1 {
            return Err(-32);
        }
        if protocol::Event::decode(&event) == Ok(protocol::Event::Reset) {
            if self.length == 0 || self.events[self.tail()] != reset_event() {
                self.push(reset_event());
            }
            return Ok(());
        }
        if self.real == QUEUE_EVENTS {
            return Err(-11);
        }
        self.push(event);
        self.real += 1;
        Ok(())
    }

    fn tail(&self) -> usize {
        (self.head + self.length - 1) % QUEUE_SLOTS
    }

    fn push(&mut self, event: [u8; EVENT_SIZE]) {
        assert!(self.length < QUEUE_SLOTS);
        self.events[(self.head + self.length) % QUEUE_SLOTS] = event;
        self.length += 1;
    }

    pub fn peek(&self) -> Option<[u8; EVENT_SIZE]> {
        (self.length != 0).then(|| self.events[self.head])
    }

    pub fn consume(&mut self) {
        assert_ne!(self.length, 0);
        if self.events[self.head] != reset_event() {
            self.real -= 1;
        }
        self.events[self.head] = [0; EVENT_SIZE];
        self.head = (self.head + 1) % QUEUE_SLOTS;
        self.length -= 1;
    }

    pub fn finish(&mut self, state: u32, detail: u64) {
        self.state = state;
        self.detail = detail;
        self.events.fill([0; EVENT_SIZE]);
        self.head = 0;
        self.length = 0;
        self.real = 0;
    }

    /// # Safety
    /// The scheduler has revoked active session pointers and stopped the owner.
    pub unsafe fn destroy(&mut self, frames: &mut FrameAllocator<'_>) {
        // SAFETY: Pixel storage was never user-mapped; the caller ended access.
        unsafe {
            self.front.destroy(frames);
            self.back.destroy(frames);
        }
    }
}

pub struct Session {
    pub applications: [Option<Application>; APP_COUNT],
    pub smoke: bool,
    pub launches: usize,
    pub closes: usize,
    pub faults: usize,
    pub preemptions: usize,
}

impl Session {
    pub fn new(smoke: bool) -> Self {
        Self {
            applications: [None, None],
            smoke,
            launches: 0,
            closes: 0,
            faults: 0,
            preemptions: 0,
        }
    }

    pub fn get(&self, pid: u64) -> Result<&Application, i64> {
        self.applications
            .iter()
            .flatten()
            .find(|application| application.pid == pid)
            .ok_or(-9)
    }

    pub fn get_mut(&mut self, pid: u64) -> Result<&mut Application, i64> {
        self.applications
            .iter_mut()
            .flatten()
            .find(|application| application.pid == pid)
            .ok_or(-9)
    }

    /// The window index of the app with this PID.
    pub fn index(&self, pid: u64) -> Result<usize, i64> {
        self.applications
            .iter()
            .position(|entry| {
                entry
                    .as_ref()
                    .is_some_and(|application| application.pid == pid)
            })
            .ok_or(-9)
    }
}

fn word(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
}

fn reset_event() -> [u8; EVENT_SIZE] {
    protocol::Event::Reset.encode().unwrap()
}

pub fn validate_event(event: &[u8; EVENT_SIZE], smoke: bool) -> Result<(), i64> {
    let event = protocol::Event::decode(event).map_err(|_| -22)?;
    if !smoke
        && matches!(
            event,
            protocol::Event::TestFault | protocol::Event::TestBusy
        )
    {
        Err(-22)
    } else {
        Ok(())
    }
}

#[cfg(feature = "boot-smoke")]
pub fn self_test(frames: &mut FrameAllocator<'_>, direct_map: u64) {
    let before = frames.stats().unwrap().allocated_frames;
    // SAFETY: Called by the sole BSP scheduler with IF=0 and the kernel root
    // active, before any window resource is published or user can execute.
    let mut application = unsafe { Application::new(1, 0, frames, direct_map) }.unwrap();
    assert_eq!(
        frames.stats().unwrap().allocated_frames,
        before + SURFACE_PAGES
    );
    assert_eq!(application.commit(), Err(-11));
    let mut tile = [9; TILE_PACKET];
    tile[..8].fill(0);
    tile[8..12].copy_from_slice(&(EDGE as u32).to_le_bytes());
    tile[12..16].copy_from_slice(&(EDGE as u32).to_le_bytes());
    application.present(&tile).unwrap();
    application.present(&tile).unwrap();
    assert_eq!(application.covered, 1, "duplicate upload counted twice");
    let mut read = [0xa5; 28];
    read[..24].fill(0);
    read[16..20].copy_from_slice(&1_u32.to_le_bytes());
    read[20..24].copy_from_slice(&1_u32.to_le_bytes());
    application.read(&mut read).unwrap();
    assert_eq!(
        &read[24..],
        &[0; 4],
        "uncommitted back-buffer bytes exposed"
    );
    for y in (0..HEIGHT).step_by(EDGE) {
        for x in (0..WIDTH).step_by(EDGE) {
            tile[..4].copy_from_slice(&(x as u32).to_le_bytes());
            tile[4..8].copy_from_slice(&(y as u32).to_le_bytes());
            application.present(&tile).unwrap();
        }
    }
    assert_eq!(application.commit(), Ok(1));
    let stale = read;
    assert_eq!(application.read(&mut read), Err(-11));
    assert_eq!(read, stale, "stale read modified packet");
    read[..8].copy_from_slice(&1_u64.to_le_bytes());
    application.read(&mut read).unwrap();
    assert_eq!(&read[24..], &[9; 4]);
    assert_eq!(application.commit(), Err(-11), "coverage survived commit");
    tile[..8].fill(0);
    tile[16..].fill(22);
    application.present(&tile).unwrap();
    application.read(&mut read).unwrap();
    assert_eq!(
        &read[24..],
        &[9; 4],
        "partial next frame changed front buffer"
    );
    tile[..4].copy_from_slice(&1_u32.to_le_bytes());
    assert_eq!(application.present(&tile), Err(-22));
    tile[..4].copy_from_slice(&u32::MAX.to_le_bytes());
    assert_eq!(application.present(&tile), Err(-22));
    assert_eq!(application.covered, 1);
    while application.peek().is_some() {
        application.consume();
    }
    let key = |ascii| {
        protocol::Event::Key {
            flags: 1,
            kind: 1,
            ascii,
        }
        .encode()
        .unwrap()
    };
    // Pointer Resets take no capacity and a full queue pushes back, keeping
    // every record (codex's 0017a reviews): 32 keys, each after a Reset, and a
    // final Reset fill all 65 slots; a second Reset in a row adds nothing, and
    // a 33rd key is refused until a key, not a Reset, is consumed.
    for index in 0..QUEUE_EVENTS as u8 {
        application.enqueue(reset_event()).unwrap();
        application.enqueue(key(b'A' + index)).unwrap();
    }
    application.enqueue(reset_event()).unwrap();
    application.enqueue(reset_event()).unwrap();
    assert_eq!(application.length, QUEUE_SLOTS);
    assert_eq!(application.enqueue(key(b'z')), Err(-11));
    for index in 0..QUEUE_EVENTS as u8 {
        assert_eq!(application.peek(), Some(reset_event()), "lost a Reset");
        application.consume();
        if index == 0 {
            assert_eq!(application.enqueue(key(b'z')), Err(-11));
        }
        assert_eq!(application.peek(), Some(key(b'A' + index)), "lost a key");
        application.consume();
        if index == 0 {
            application.enqueue(key(b'z')).unwrap();
        }
    }
    assert_eq!(application.peek(), Some(reset_event()));
    application.consume();
    assert_eq!(application.peek(), Some(key(b'z')));
    application.consume();
    assert_eq!((application.peek(), application.real), (None, 0));
    let key = key(b'x');
    let mut invalid = key;
    invalid[16] = 1;
    assert_eq!(validate_event(&invalid, true), Err(-22));
    assert_eq!(
        validate_event(&protocol::Event::TestFault.encode().unwrap(), false),
        Err(-22)
    );
    application.finish(3, 6);
    assert_eq!(application.enqueue(key), Err(-32));
    // SAFETY: The test never published this object or user-mapped its pages.
    unsafe { application.destroy(frames) };
    assert_eq!(frames.stats().unwrap().allocated_frames, before);
    crate::serial::println!("MIRROR_WINDOW_TESTS_OK");
}

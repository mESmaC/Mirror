//! The bounded RFB 3.8 subset shared by the 0015c host lane.
//!
//! Wire layouts follow RFC 6143 (https://www.rfc-editor.org/rfc/rfc6143).
//! There are no authentication mechanisms other than security None. Callers
//! must use the reference VM's loopback-only VNC listener.

use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

use crate::bounds::{CONNECT_ATTEMPT, CONNECT_ATTEMPTS, CountBudget, Deadline, KeyAction};

const VERSION: &[u8; 12] = b"RFB 003.008\n";
const MAX_DIMENSION: u16 = 4096;
const MAX_PIXELS: usize = 4096 * 4096;
const MAX_RECTANGLES: usize = 4096;
const MAX_MESSAGE_BYTES: usize = 128 * 1024 * 1024;
const MAX_STRING_BYTES: usize = 1024 * 1024;
const MAX_IGNORED_MESSAGES: usize = 4096;
// 32 bits per pixel, depth 24, little endian, true color, RGB888.
const PIXEL_FORMAT: [u8; 16] = [32, 24, 0, 1, 0, 255, 0, 255, 0, 255, 16, 8, 0, 0, 0, 0];

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Frame {
    pub width: u16,
    pub height: u16,
    pub pixels: Vec<[u8; 3]>,
}

impl Frame {
    pub fn new(width: u16, height: u16) -> io::Result<Self> {
        let count = pixel_count(width, height)?;
        Ok(Self {
            width,
            height,
            pixels: filled_vec(count, [0; 3])?,
        })
    }

    fn try_copy(&self) -> io::Result<Self> {
        self.validate()?;
        Ok(Self {
            width: self.width,
            height: self.height,
            pixels: copied_vec(&self.pixels)?,
        })
    }

    pub fn validate(&self) -> io::Result<()> {
        if self.pixels.len() != pixel_count(self.width, self.height)? {
            return Err(invalid("framebuffer length does not match dimensions"));
        }
        Ok(())
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct Update {
    pub size_changes: Vec<(u16, u16)>,
    pub raw_rectangles: usize,
    /// Every pixel has been received since the latest full request or resize.
    pub complete: bool,
}

/// Cached oracle geometry is valid only while this generation is unchanged.
/// Even a DesktopSize message with unchanged dimensions invalidates it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Geometry {
    pub width: u16,
    pub height: u16,
    pub generation: u64,
}

/// Keep one of these for the entire case, including any permitted reconnect.
/// This limits attempts; it does not authorize a reconnect policy.
pub struct Connector {
    case: Deadline,
    attempts: CountBudget,
}

impl Connector {
    pub fn new(case: Deadline) -> Self {
        Self {
            case,
            attempts: CountBudget::new(CONNECT_ATTEMPTS),
        }
    }

    pub fn connect(&mut self, address: SocketAddr) -> io::Result<Client> {
        if !address.ip().is_loopback() {
            return Err(argument("RFB security None requires a loopback listener"));
        }
        let deadline = self.case.child(CONNECT_ATTEMPT)?;
        self.attempts.consume()?;
        Client::connect_until(address, deadline)
    }

    pub fn attempts(&self) -> usize {
        self.attempts.used()
    }
}

pub struct Client {
    stream: TcpStream,
    frame: Frame,
    initial_size: (u16, u16),
    coverage: Vec<u8>,
    uncovered: usize,
    // Retaining partial messages makes an input timeout safe even when it
    // interrupts a Raw rectangle, so the caller can release the held key.
    incoming: Vec<u8>,
    usable: bool,
    pending: bool,
    geometry_generation: u64,
}

impl Client {
    pub fn connect(address: SocketAddr, timeout: Duration) -> io::Result<Self> {
        Self::connect_until(address, Deadline::new(timeout)?)
    }

    fn connect_until(address: SocketAddr, deadline: Deadline) -> io::Result<Self> {
        if !address.ip().is_loopback() {
            return Err(argument("RFB security None requires a loopback listener"));
        }
        let deadline = deadline.child(CONNECT_ATTEMPT)?;
        let stream = TcpStream::connect_timeout(&address, deadline.remaining()?)?;
        stream.set_nodelay(true)?;
        let frame = handshake(&mut TimedStream {
            stream: &stream,
            deadline,
        })?;
        deadline.remaining()?;
        let initial_size = (frame.width, frame.height);
        let uncovered = frame.pixels.len();
        let coverage = filled_vec(uncovered, 0)?;
        deadline.remaining()?;
        Ok(Self {
            stream,
            frame,
            initial_size,
            coverage,
            uncovered,
            incoming: Vec::new(),
            usable: true,
            pending: false,
            geometry_generation: 0,
        })
    }

    pub fn frame(&self) -> &Frame {
        &self.frame
    }

    pub fn initial_size(&self) -> (u16, u16) {
        self.initial_size
    }

    pub fn geometry(&self) -> Geometry {
        Geometry {
            width: self.frame.width,
            height: self.frame.height,
            generation: self.geometry_generation,
        }
    }

    pub fn request_pending(&self) -> bool {
        self.pending
    }

    /// An oracle may inspect only a completely covered, settled framebuffer.
    pub fn complete_frame(&self) -> Option<&Frame> {
        (self.usable && !self.pending && self.uncovered == 0).then_some(&self.frame)
    }

    /// Request the entire current framebuffer. A pending request survives
    /// timeouts; a duplicate is rejected before any bytes are sent.
    pub fn request_update(&mut self, incremental: bool, timeout: Duration) -> io::Result<()> {
        self.request_update_until(incremental, Deadline::new(timeout)?)
    }

    pub fn request_update_until(
        &mut self,
        incremental: bool,
        deadline: Deadline,
    ) -> io::Result<()> {
        self.check_usable()?;
        if self.pending {
            return Err(argument("RFB framebuffer request is already pending"));
        }
        let mut request = [0; 10];
        request[0] = 3;
        request[1] = u8::from(incremental);
        request[6..8].copy_from_slice(&self.frame.width.to_be_bytes());
        request[8..10].copy_from_slice(&self.frame.height.to_be_bytes());
        self.send_until(&request, deadline)?;
        self.pending = true;
        if !incremental {
            self.coverage.fill(0);
            self.uncovered = self.coverage.len();
        }
        Ok(())
    }

    /// Receive one complete framebuffer update. A timeout leaves both the last
    /// complete frame and any partial wire message intact. Other wire errors
    /// retire this connection; a caller must reconnect rather than resync it.
    pub fn read_update(&mut self, timeout: Duration) -> io::Result<Update> {
        self.read_until(Deadline::new(timeout)?)
    }

    pub fn read_until(&mut self, deadline: Deadline) -> io::Result<Update> {
        self.check_usable()?;
        if !self.pending {
            return Err(argument("RFB update requires an outstanding request"));
        }
        let result = self.read_update_until(deadline);
        if let Err(error) = &result
            && error.kind() != io::ErrorKind::TimedOut
        {
            self.usable = false;
        }
        result
    }

    /// Readiness polling always asks for a full frame when no request remains.
    /// Pass the same phase deadline on every poll so progress cannot renew it.
    pub fn poll_full(&mut self, deadline: Deadline) -> io::Result<Update> {
        if !self.pending {
            self.request_update_until(false, deadline)?;
        }
        self.read_until(deadline)
    }

    fn read_update_until(&mut self, deadline: Deadline) -> io::Result<Update> {
        let mut ignored = 0;
        loop {
            deadline.remaining()?;
            if let Some(message) = parse_message(&self.incoming, &self.frame)? {
                match message {
                    Message::Update { consumed, rects } => {
                        let (frame, coverage, uncovered, update) = apply_update(
                            &self.frame,
                            &self.coverage,
                            self.uncovered,
                            &self.incoming,
                            &rects,
                            deadline,
                        )?;
                        deadline.remaining()?;
                        let generation = self
                            .geometry_generation
                            .checked_add(update.size_changes.len() as u64)
                            .ok_or_else(|| invalid("RFB geometry generation overflow"))?;
                        self.frame = frame;
                        self.coverage = coverage;
                        self.uncovered = uncovered;
                        self.incoming.drain(..consumed);
                        self.pending = false;
                        self.geometry_generation = generation;
                        return Ok(update);
                    }
                    Message::Ignored { consumed } => {
                        self.incoming.drain(..consumed);
                        ignored += 1;
                        if ignored > MAX_IGNORED_MESSAGES {
                            return Err(invalid("too many non-frame RFB messages"));
                        }
                    }
                }
                continue;
            }
            if self.incoming.len() == MAX_MESSAGE_BYTES {
                return Err(invalid("RFB message exceeds byte limit"));
            }
            let mut bytes = [0; 64 * 1024];
            let limit = bytes.len().min(MAX_MESSAGE_BYTES - self.incoming.len());
            let count = TimedStream {
                stream: &self.stream,
                deadline,
            }
            .read(&mut bytes[..limit])?;
            if count == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "RFB server closed before a complete update",
                ));
            }
            self.incoming.try_reserve(count).map_err(allocation_error)?;
            self.incoming.extend_from_slice(&bytes[..count]);
        }
    }

    pub fn key_event(&mut self, keysym: u32, down: bool, timeout: Duration) -> io::Result<()> {
        let mut event = [0; 8];
        event[0] = 4;
        event[1] = u8::from(down);
        event[4..].copy_from_slice(&keysym.to_be_bytes());
        self.send(&event, timeout)
    }

    pub fn key_pair(&mut self, action: &mut KeyAction<'_>, keysym: u32) -> io::Result<()> {
        crate::bounds::send_key_pair(action, keysym, std::time::Instant::now, |key, down, cap| {
            self.key_event(key, down, cap)
        })
    }

    pub fn pointer_event(
        &mut self,
        x: u16,
        y: u16,
        buttons: u8,
        timeout: Duration,
    ) -> io::Result<()> {
        if x >= self.frame.width || y >= self.frame.height {
            return Err(argument("RFB pointer is outside the framebuffer"));
        }
        // The transport supports the three ordinary buttons only. The lane's
        // 0015d policy further permits only its single buttons-up priming move.
        if buttons & !0x07 != 0 {
            return Err(argument("unsupported RFB pointer button mask"));
        }
        let mut event = [5, buttons, 0, 0, 0, 0];
        event[2..4].copy_from_slice(&x.to_be_bytes());
        event[4..6].copy_from_slice(&y.to_be_bytes());
        self.send(&event, timeout)
    }

    fn check_usable(&self) -> io::Result<()> {
        if self.usable {
            Ok(())
        } else {
            Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "RFB connection retired after a wire error",
            ))
        }
    }

    fn send(&mut self, message: &[u8], timeout: Duration) -> io::Result<()> {
        self.send_until(message, Deadline::new(timeout)?)
    }

    fn send_until(&mut self, message: &[u8], deadline: Deadline) -> io::Result<()> {
        self.check_usable()?;
        let result = TimedStream {
            stream: &self.stream,
            deadline,
        }
        .write_all(message);
        // A partially written client message cannot be safely retried.
        if result.is_err() {
            self.usable = false;
        }
        result
    }
}

struct TimedStream<'a> {
    stream: &'a TcpStream,
    deadline: Deadline,
}

impl Read for TimedStream<'_> {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        loop {
            self.stream
                .set_read_timeout(Some(self.deadline.remaining()?))?;
            match self.stream.read(bytes) {
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                result => return result.map_err(normalize_timeout),
            }
        }
    }
}

impl Write for TimedStream<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        loop {
            self.stream
                .set_write_timeout(Some(self.deadline.remaining()?))?;
            match self.stream.write(bytes) {
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                result => return result.map_err(normalize_timeout),
            }
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        self.deadline.remaining()?;
        self.stream.flush()
    }
}

fn normalize_timeout(error: io::Error) -> io::Error {
    if error.kind() == io::ErrorKind::WouldBlock {
        io::Error::new(io::ErrorKind::TimedOut, "RFB operation timed out")
    } else {
        error
    }
}

fn handshake(stream: &mut (impl Read + Write)) -> io::Result<Frame> {
    let mut version = [0; 12];
    stream.read_exact(&mut version)?;
    if &version != VERSION {
        return Err(invalid("RFB server must offer protocol 3.8"));
    }
    stream.write_all(VERSION)?;
    let mut count = [0];
    stream.read_exact(&mut count)?;
    if count[0] == 0 {
        discard_string(stream)?;
        return Err(invalid("RFB server rejected the connection"));
    }
    let mut security = [0; 255];
    stream.read_exact(&mut security[..usize::from(count[0])])?;
    if !security[..usize::from(count[0])].contains(&1) {
        return Err(invalid("RFB server does not offer security None"));
    }
    stream.write_all(&[1])?;
    let mut result = [0; 4];
    stream.read_exact(&mut result)?;
    if u32::from_be_bytes(result) != 0 {
        discard_string(stream)?;
        return Err(invalid("RFB security negotiation failed"));
    }
    // Shared session: observing a baseline never disconnects another viewer.
    stream.write_all(&[1])?;
    let mut init = [0; 20];
    stream.read_exact(&mut init)?;
    let width = be_u16(&init[0..2]);
    let height = be_u16(&init[2..4]);
    pixel_count(width, height)?;
    validate_pixel_format(&init[4..20])?;
    discard_string(stream)?;
    let frame = Frame::new(width, height)?;
    let mut format = [0; 20];
    format[4..].copy_from_slice(&PIXEL_FORMAT);
    stream.write_all(&format)?;
    // SetEncodings: Raw (0), DesktopSize (-223), and nothing else.
    let mut encodings = [2, 0, 0, 2, 0, 0, 0, 0, 0, 0, 0, 0];
    encodings[8..12].copy_from_slice(&(-223_i32).to_be_bytes());
    stream.write_all(&encodings)?;
    Ok(frame)
}

fn discard_string(stream: &mut impl Read) -> io::Result<()> {
    let mut length = [0; 4];
    stream.read_exact(&mut length)?;
    let length = u32::from_be_bytes(length) as usize;
    if length > MAX_STRING_BYTES {
        return Err(invalid("RFB string exceeds byte limit"));
    }
    let mut scratch = [0; 4096];
    let mut remaining = length;
    while remaining > 0 {
        let count = remaining.min(scratch.len());
        stream.read_exact(&mut scratch[..count])?;
        remaining -= count;
    }
    Ok(())
}

fn validate_pixel_format(format: &[u8]) -> io::Result<()> {
    let bits = format[0];
    let depth = format[1];
    if !matches!(bits, 8 | 16 | 32) || depth == 0 || depth > bits || format[2] > 1 || format[3] != 1
    {
        return Err(invalid("invalid or unsupported RFB pixel format"));
    }
    let mut occupied = 0_u32;
    let mut channel_bits = 0;
    for channel in 0..3 {
        let maximum = u32::from(be_u16(&format[4 + channel * 2..6 + channel * 2]));
        let shift = u32::from(format[10 + channel]);
        let width = maximum.count_ones();
        if maximum == 0 || maximum & (maximum + 1) != 0 || shift + width > u32::from(bits) {
            return Err(invalid("invalid RFB color maximum or shift"));
        }
        let mask = maximum << shift;
        if occupied & mask != 0 {
            return Err(invalid("overlapping RFB color channels"));
        }
        occupied |= mask;
        channel_bits += width;
    }
    if channel_bits > u32::from(depth) {
        return Err(invalid("RFB color channels exceed pixel depth"));
    }
    Ok(())
}

#[derive(Debug)]
enum Rectangle {
    Raw {
        x: u16,
        y: u16,
        width: u16,
        height: u16,
        offset: usize,
    },
    DesktopSize {
        width: u16,
        height: u16,
    },
}

#[derive(Debug)]
enum Message {
    Update {
        consumed: usize,
        rects: Vec<Rectangle>,
    },
    Ignored {
        consumed: usize,
    },
}

/// Only validate framing here. Pixels are committed after the entire message
/// passes, so a malformed final rectangle cannot leave a partly changed frame.
fn parse_message(bytes: &[u8], frame: &Frame) -> io::Result<Option<Message>> {
    let Some(kind) = bytes.first() else {
        return Ok(None);
    };
    match kind {
        0 => parse_update(bytes, frame),
        2 => Ok(Some(Message::Ignored { consumed: 1 })), // Bell.
        3 => {
            if bytes.len() < 8 {
                return Ok(None);
            }
            let length = u32::from_be_bytes(bytes[4..8].try_into().unwrap()) as usize;
            // This also rejects the extended clipboard's signed-length form,
            // which we did not advertise and therefore must not interpret.
            if length > MAX_STRING_BYTES {
                return Err(invalid("RFB clipboard exceeds byte limit"));
            }
            Ok((bytes.len() >= length + 8).then_some(Message::Ignored {
                consumed: length + 8,
            }))
        }
        _ => Err(invalid("unsupported RFB server message")),
    }
}

fn parse_update(bytes: &[u8], frame: &Frame) -> io::Result<Option<Message>> {
    if bytes.len() < 4 {
        return Ok(None);
    }
    let count = usize::from(be_u16(&bytes[2..4]));
    if count > MAX_RECTANGLES {
        return Err(invalid("RFB update exceeds rectangle limit"));
    }
    let mut rects = Vec::new();
    rects.try_reserve(count).map_err(allocation_error)?;
    let mut offset = 4_usize;
    for index in 0..count {
        let header_end = offset
            .checked_add(12)
            .filter(|end| *end <= MAX_MESSAGE_BYTES)
            .ok_or_else(|| invalid("RFB update exceeds byte limit"))?;
        if bytes.len() < header_end {
            return Ok(None);
        }
        let header = &bytes[offset..header_end];
        let x = be_u16(&header[0..2]);
        let y = be_u16(&header[2..4]);
        let width = be_u16(&header[4..6]);
        let height = be_u16(&header[6..8]);
        let encoding = i32::from_be_bytes(header[8..12].try_into().unwrap());
        offset = header_end;
        match encoding {
            0 => {
                if width == 0
                    || height == 0
                    || u32::from(x) + u32::from(width) > u32::from(frame.width)
                    || u32::from(y) + u32::from(height) > u32::from(frame.height)
                {
                    return Err(invalid("RFB Raw rectangle is outside the framebuffer"));
                }
                let payload = usize::from(width)
                    .checked_mul(usize::from(height))
                    .and_then(|pixels| pixels.checked_mul(4))
                    .ok_or_else(|| invalid("RFB Raw rectangle length overflow"))?;
                let end = offset
                    .checked_add(payload)
                    .filter(|end| *end <= MAX_MESSAGE_BYTES)
                    .ok_or_else(|| invalid("RFB update exceeds byte limit"))?;
                if bytes.len() < end {
                    return Ok(None);
                }
                rects.push(Rectangle::Raw {
                    x,
                    y,
                    width,
                    height,
                    offset,
                });
                offset = end;
            }
            -223 => {
                pixel_count(width, height)?;
                if index + 1 != count {
                    return Err(invalid("RFB DesktopSize must be the final rectangle"));
                }
                // RFC 6143 explicitly ignores DesktopSize's x/y fields.
                rects.push(Rectangle::DesktopSize { width, height });
            }
            _ => return Err(invalid("unsupported RFB rectangle encoding")),
        }
    }
    Ok(Some(Message::Update {
        consumed: offset,
        rects,
    }))
}

fn apply_update(
    original: &Frame,
    old_coverage: &[u8],
    old_uncovered: usize,
    bytes: &[u8],
    rects: &[Rectangle],
    deadline: Deadline,
) -> io::Result<(Frame, Vec<u8>, usize, Update)> {
    let mut frame = original.try_copy()?;
    let mut coverage = copied_vec(old_coverage)?;
    let mut uncovered = old_uncovered;
    let mut update = Update {
        size_changes: Vec::new(),
        raw_rectangles: 0,
        complete: false,
    };
    for rect in rects {
        deadline.remaining()?;
        match *rect {
            Rectangle::Raw {
                x,
                y,
                width,
                height,
                offset,
            } => {
                let row_bytes = usize::from(width) * 4;
                for row in 0..usize::from(height) {
                    deadline.remaining()?;
                    let start = offset + row * row_bytes;
                    let destination =
                        (usize::from(y) + row) * usize::from(frame.width) + usize::from(x);
                    for (column, pixel) in bytes[start..start + row_bytes]
                        .as_chunks::<4>()
                        .0
                        .iter()
                        .enumerate()
                    {
                        let index = destination + column;
                        frame.pixels[index] = [pixel[2], pixel[1], pixel[0]];
                        if coverage[index] == 0 {
                            coverage[index] = 1;
                            uncovered -= 1;
                        }
                    }
                }
                update.raw_rectangles += 1;
            }
            Rectangle::DesktopSize { width, height } => {
                frame = Frame::new(width, height)?;
                uncovered = frame.pixels.len();
                coverage = filled_vec(uncovered, 0)?;
                update.size_changes.push((width, height));
            }
        }
    }
    update.complete = uncovered == 0;
    Ok((frame, coverage, uncovered, update))
}

fn pixel_count(width: u16, height: u16) -> io::Result<usize> {
    if width == 0 || height == 0 || width > MAX_DIMENSION || height > MAX_DIMENSION {
        return Err(invalid("RFB framebuffer dimensions are outside 1..=4096"));
    }
    usize::from(width)
        .checked_mul(usize::from(height))
        .filter(|count| *count <= MAX_PIXELS)
        .ok_or_else(|| invalid("RFB framebuffer exceeds pixel limit"))
}

fn filled_vec<T: Clone>(length: usize, value: T) -> io::Result<Vec<T>> {
    let mut output = Vec::new();
    output.try_reserve_exact(length).map_err(allocation_error)?;
    output.resize(length, value);
    Ok(output)
}

fn copied_vec<T: Clone>(source: &[T]) -> io::Result<Vec<T>> {
    let mut output = Vec::new();
    output
        .try_reserve_exact(source.len())
        .map_err(allocation_error)?;
    output.extend_from_slice(source);
    Ok(output)
}

fn allocation_error(_: std::collections::TryReserveError) -> io::Error {
    io::Error::other("RFB allocation could not be reserved")
}

fn be_u16(bytes: &[u8]) -> u16 {
    u16::from_be_bytes([bytes[0], bytes[1]])
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn argument(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use std::net::TcpListener;

    const TIMEOUT: Duration = Duration::from_secs(3);

    // These are synthetic RFC/spec-derived fixtures. They are not recordings
    // from VMware, and do not satisfy spec 0018's real-capture acceptance item.
    #[derive(Default)]
    struct Fixture {
        input: Cursor<Vec<u8>>,
        output: Vec<u8>,
    }

    impl Read for Fixture {
        fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
            self.input.read(output)
        }
    }

    impl Write for Fixture {
        fn write(&mut self, input: &[u8]) -> io::Result<usize> {
            self.output.extend_from_slice(input);
            Ok(input.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn server_init(width: u16, height: u16) -> Vec<u8> {
        let mut bytes = Vec::from(VERSION.as_slice());
        bytes.extend_from_slice(&[1, 1, 0, 0, 0, 0]);
        bytes.extend_from_slice(&width.to_be_bytes());
        bytes.extend_from_slice(&height.to_be_bytes());
        bytes.extend_from_slice(&PIXEL_FORMAT);
        bytes.extend_from_slice(&9_u32.to_be_bytes());
        bytes.extend_from_slice(b"synthetic");
        bytes
    }

    fn fixture(bytes: Vec<u8>) -> Fixture {
        Fixture {
            input: Cursor::new(bytes),
            ..Fixture::default()
        }
    }

    fn rectangle(x: u16, y: u16, width: u16, height: u16, encoding: i32) -> Vec<u8> {
        let mut bytes = Vec::new();
        for field in [x, y, width, height] {
            bytes.extend_from_slice(&field.to_be_bytes());
        }
        bytes.extend_from_slice(&encoding.to_be_bytes());
        bytes
    }

    fn raw(x: u16, y: u16, width: u16, height: u16, pixel: [u8; 4]) -> Vec<u8> {
        let mut bytes = rectangle(x, y, width, height, 0);
        for _ in 0..usize::from(width) * usize::from(height) {
            bytes.extend_from_slice(&pixel);
        }
        bytes
    }

    fn update(rectangles: &[Vec<u8>]) -> Vec<u8> {
        let mut bytes = vec![0, 0];
        bytes.extend_from_slice(&(rectangles.len() as u16).to_be_bytes());
        for rectangle in rectangles {
            bytes.extend_from_slice(rectangle);
        }
        bytes
    }

    fn apply(
        frame: &Frame,
        coverage: &[u8],
        uncovered: usize,
        bytes: &[u8],
    ) -> io::Result<(Frame, Vec<u8>, usize, Update)> {
        let Some(Message::Update { rects, .. }) = parse_message(bytes, frame)? else {
            panic!("expected a complete update fixture");
        };
        apply_update(
            frame,
            coverage,
            uncovered,
            bytes,
            &rects,
            Deadline::new(TIMEOUT).unwrap(),
        )
    }

    fn connected_pair(width: u16, height: u16) -> (Client, TcpStream) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let stream = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (peer, _) = listener.accept().unwrap();
        peer.set_read_timeout(Some(TIMEOUT)).unwrap();
        peer.set_write_timeout(Some(TIMEOUT)).unwrap();
        let frame = Frame::new(width, height).unwrap();
        let uncovered = frame.pixels.len();
        (
            Client {
                stream,
                frame,
                initial_size: (width, height),
                coverage: vec![0; uncovered],
                uncovered,
                incoming: Vec::new(),
                usable: true,
                pending: false,
                geometry_generation: 0,
            },
            peer,
        )
    }

    fn requested_pair(width: u16, height: u16) -> (Client, TcpStream) {
        let (mut client, mut peer) = connected_pair(width, height);
        client.request_update(false, TIMEOUT).unwrap();
        let mut request = [0; 10];
        peer.read_exact(&mut request).unwrap();
        (client, peer)
    }

    #[test]
    fn synthetic_none_handshake_advertises_only_selected_format_and_encodings() {
        let mut fixture = fixture(server_init(1920, 1080));
        let frame = handshake(&mut fixture).unwrap();
        assert_eq!((frame.width, frame.height), (1920, 1080));
        let mut expected = Vec::from(VERSION.as_slice());
        expected.extend_from_slice(&[1, 1, 0, 0, 0, 0]);
        expected.extend_from_slice(&PIXEL_FORMAT);
        expected.extend_from_slice(&[2, 0, 0, 2, 0, 0, 0, 0]);
        expected.extend_from_slice(&(-223_i32).to_be_bytes());
        assert_eq!(fixture.output, expected);
    }

    #[test]
    fn handshake_rejects_other_versions_authentication_and_failure_results() {
        let mut older = server_init(1, 1);
        older[10] = b'3';
        assert!(handshake(&mut fixture(older)).is_err());
        let mut auth = server_init(1, 1);
        auth[13] = 2;
        assert!(handshake(&mut fixture(auth)).is_err());
        for mut failure in [vec![0], vec![1, 1, 0, 0, 0, 1]] {
            failure.extend_from_slice(&6_u32.to_be_bytes());
            failure.extend_from_slice(b"secret");
            let mut bytes = Vec::from(VERSION.as_slice());
            bytes.extend_from_slice(&failure);
            let error = handshake(&mut fixture(bytes)).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidData);
            assert!(!error.to_string().contains("secret"));
        }
    }

    #[test]
    fn handshake_rejects_dimensions_strings_and_truncation() {
        for (width, height) in [(0, 1), (1, 0), (4097, 1), (1, u16::MAX)] {
            assert!(handshake(&mut fixture(server_init(width, height))).is_err());
        }
        let complete = server_init(1, 1);
        for cut in 0..complete.len() {
            assert!(handshake(&mut fixture(complete[..cut].to_vec())).is_err());
        }
        let mut long_name = complete;
        long_name[38..42].copy_from_slice(&((MAX_STRING_BYTES + 1) as u32).to_be_bytes());
        assert!(handshake(&mut fixture(long_name)).is_err());
    }

    #[test]
    fn pixel_formats_reject_overflow_overlap_invalid_maxima_and_depth() {
        validate_pixel_format(&PIXEL_FORMAT).unwrap();
        let rgb565 = [16, 16, 1, 1, 0, 31, 0, 63, 0, 31, 11, 5, 0, 0, 0, 0];
        validate_pixel_format(&rgb565).unwrap();
        for (index, value) in [
            (0, 24),
            (1, 23),
            (2, 2),
            (3, 0),
            (5, 254),
            (10, 32),
            (11, 16),
        ] {
            let mut malformed = PIXEL_FORMAT;
            malformed[index] = value;
            assert!(validate_pixel_format(&malformed).is_err());
        }
    }

    #[test]
    fn raw_rows_decode_bgr_and_coverage_counts_each_pixel_once() {
        let frame = Frame::new(2, 2).unwrap();
        let top = update(&[
            raw(0, 0, 2, 1, [3, 2, 1, 99]),
            raw(1, 0, 1, 1, [6, 5, 4, 0]),
        ]);
        let (frame, coverage, uncovered, result) = apply(&frame, &[0; 4], 4, &top).unwrap();
        assert_eq!(frame.pixels, [[1, 2, 3], [4, 5, 6], [0; 3], [0; 3]]);
        assert_eq!(uncovered, 2);
        assert_eq!(result.raw_rectangles, 2);
        assert!(!result.complete);
        let bottom = update(&[raw(0, 1, 2, 1, [9, 8, 7, 0])]);
        let (_, _, uncovered, result) = apply(&frame, &coverage, uncovered, &bottom).unwrap();
        assert_eq!(uncovered, 0);
        assert!(result.complete);
    }

    #[test]
    fn desktop_size_invalidates_old_pixels_and_requires_new_coverage() {
        let original = Frame {
            width: 2,
            height: 1,
            pixels: vec![[9; 3]; 2],
        };
        // Coordinates are intentionally nonzero: RFC 6143 says to ignore them.
        let resize = update(&[rectangle(7, 8, 1, 2, -223)]);
        let (frame, coverage, uncovered, result) = apply(&original, &[1; 2], 0, &resize).unwrap();
        assert_eq!((frame.width, frame.height), (1, 2));
        assert_eq!(frame.pixels, [[0; 3]; 2]);
        assert_eq!(result.size_changes, [(1, 2)]);
        assert!(!result.complete);
        let full = update(&[raw(0, 0, 1, 2, [1, 2, 3, 0])]);
        let (_, _, _, result) = apply(&frame, &coverage, uncovered, &full).unwrap();
        assert!(result.complete);
    }

    #[test]
    fn zero_rectangles_do_not_invent_coverage() {
        let frame = Frame::new(1, 1).unwrap();
        let (_, _, _, result) = apply(&frame, &[0], 1, &[0, 0, 0, 0]).unwrap();
        assert!(!result.complete);
        let (_, _, _, result) = apply(&frame, &[1], 0, &[0, 0, 0, 0]).unwrap();
        assert!(result.complete);
    }

    #[test]
    fn rectangles_reject_bad_bounds_unsupported_encoding_and_bad_resize_order() {
        let frame = Frame::new(2, 2).unwrap();
        for rect in [
            rectangle(1, 0, 2, 1, 0),
            rectangle(0, 1, 1, 2, 0),
            rectangle(u16::MAX, 0, 1, 1, 0),
            rectangle(0, 0, 0, 1, 0),
            rectangle(0, 0, 1, 1, 1),
            rectangle(0, 0, 0, 2, -223),
            rectangle(0, 0, 4097, 2, -223),
        ] {
            assert!(parse_message(&update(&[rect]), &frame).is_err());
        }
        assert!(parse_message(&[0, 0, 0xff, 0xff], &frame).is_err());
        let wrong_order = update(&[rectangle(0, 0, 2, 2, -223), raw(0, 0, 1, 1, [0; 4])]);
        assert!(parse_message(&wrong_order, &frame).is_err());
    }

    #[test]
    fn every_truncated_raw_prefix_is_incomplete() {
        let frame = Frame::new(2, 2).unwrap();
        let bytes = update(&[raw(0, 0, 2, 2, [1, 2, 3, 0])]);
        for cut in 0..bytes.len() {
            assert!(parse_message(&bytes[..cut], &frame).unwrap().is_none());
        }
        assert!(parse_message(&bytes, &frame).unwrap().is_some());
    }

    #[test]
    fn clipboard_is_bounded_and_unsupported_messages_fail() {
        let frame = Frame::new(1, 1).unwrap();
        assert!(matches!(
            parse_message(&[2], &frame).unwrap(),
            Some(Message::Ignored { consumed: 1 })
        ));
        let clipboard = [3, 0, 0, 0, 0, 0, 0, 3, b'a', b'b', b'c'];
        for cut in 1..clipboard.len() {
            assert!(parse_message(&clipboard[..cut], &frame).unwrap().is_none());
        }
        assert!(matches!(
            parse_message(&clipboard, &frame).unwrap(),
            Some(Message::Ignored { consumed: 11 })
        ));
        assert!(parse_message(&[3, 0, 0, 0, 0xff, 0xff, 0xff, 0xff], &frame).is_err());
        for kind in [1, 4, 255] {
            assert!(parse_message(&[kind], &frame).is_err());
        }
    }

    #[test]
    fn events_and_requests_have_exact_wire_encoding() {
        let (mut client, mut peer) = connected_pair(1920, 1080);
        client.key_event(0xff08, true, TIMEOUT).unwrap();
        client.key_event(0xff08, false, TIMEOUT).unwrap();
        client.pointer_event(1919, 1079, 1, TIMEOUT).unwrap();
        client.pointer_event(0, 0, 0, TIMEOUT).unwrap();
        client.request_update(true, TIMEOUT).unwrap();
        let mut actual = [0; 38];
        peer.read_exact(&mut actual).unwrap();
        assert_eq!(
            actual,
            [
                4, 1, 0, 0, 0, 0, 255, 8, 4, 0, 0, 0, 0, 0, 255, 8, 5, 1, 7, 127, 4, 55, 5, 0, 0,
                0, 0, 0, 3, 1, 0, 0, 0, 0, 7, 128, 4, 56,
            ]
        );
        assert!(client.pointer_event(1920, 0, 0, TIMEOUT).is_err());
        assert!(client.pointer_event(0, 1080, 0, TIMEOUT).is_err());
        assert!(client.pointer_event(0, 0, 8, TIMEOUT).is_err());
        assert_eq!(client.initial_size(), (1920, 1080));
    }

    #[test]
    fn timeout_preserves_partial_update_and_allows_key_release() {
        let (mut client, mut peer) = requested_pair(2, 1);
        let bytes = update(&[raw(0, 0, 2, 1, [1, 2, 3, 0])]);
        peer.write_all(&bytes[..18]).unwrap();
        let error = client.read_update(Duration::from_millis(20)).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert_eq!(client.frame().pixels, [[0; 3]; 2]);
        assert!(client.request_pending());
        assert!(client.complete_frame().is_none());
        assert_eq!(
            client.request_update(false, TIMEOUT).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        client.key_event(u32::from(b'a'), false, TIMEOUT).unwrap();
        let mut released = [0; 8];
        peer.read_exact(&mut released).unwrap();
        assert_eq!(released, [4, 0, 0, 0, 0, 0, 0, b'a']);
        peer.write_all(&bytes[18..]).unwrap();
        assert!(client.read_update(TIMEOUT).unwrap().complete);
        assert_eq!(client.frame().pixels, [[3, 2, 1]; 2]);
        assert!(!client.request_pending());
        assert!(client.complete_frame().is_some());
    }

    #[test]
    fn handshake_deadline_is_total_even_when_server_keeps_sending() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut peer, _) = listener.accept().unwrap();
            peer.set_write_timeout(Some(TIMEOUT)).unwrap();
            // A per-read timeout would accept this slow, otherwise valid
            // handshake. The total deadline must expire partway through it.
            for byte in server_init(1, 1) {
                if peer.write_all(&[byte]).is_err() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
        });
        let result = Client::connect(address, Duration::from_millis(40));
        server.join().unwrap();
        assert_eq!(result.err().unwrap().kind(), io::ErrorKind::TimedOut);
    }

    #[test]
    fn resize_discards_raw_pixels_earlier_in_the_same_update() {
        let frame = Frame::new(2, 1).unwrap();
        let bytes = update(&[raw(0, 0, 2, 1, [1, 2, 3, 0]), rectangle(0, 0, 2, 1, -223)]);
        let (frame, coverage, uncovered, result) = apply(&frame, &[0; 2], 2, &bytes).unwrap();
        assert_eq!(result.raw_rectangles, 1);
        assert_eq!(result.size_changes, [(2, 1)]);
        assert_eq!(frame.pixels, [[0; 3]; 2]);
        assert_eq!(coverage, [0; 2]);
        assert_eq!(uncovered, 2);
        assert!(!result.complete);
    }

    #[test]
    fn malformed_update_is_atomic_and_retires_connection() {
        let (mut client, mut peer) = requested_pair(1, 1);
        peer.write_all(&update(&[
            raw(0, 0, 1, 1, [1; 4]),
            rectangle(1, 0, 1, 1, 0),
        ]))
        .unwrap();
        assert_eq!(
            client.read_update(TIMEOUT).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        assert_eq!(client.frame().pixels, [[0; 3]]);
        assert_eq!(
            client.read_update(TIMEOUT).unwrap_err().kind(),
            io::ErrorKind::NotConnected
        );
    }

    #[test]
    fn full_request_resets_coverage_after_a_complete_frame() {
        let (mut client, mut peer) = requested_pair(1, 1);
        peer.write_all(&update(&[raw(0, 0, 1, 1, [1; 4])])).unwrap();
        assert!(client.read_update(TIMEOUT).unwrap().complete);
        client.request_update(false, TIMEOUT).unwrap();
        let mut request = [0; 10];
        peer.read_exact(&mut request).unwrap();
        assert_eq!(request, [3, 0, 0, 0, 0, 0, 0, 1, 0, 1]);
        peer.write_all(&[0, 0, 0, 0]).unwrap();
        assert!(!client.read_update(TIMEOUT).unwrap().complete);
    }

    #[test]
    fn eof_and_non_frame_messages_are_handled_without_hanging() {
        let (mut client, mut peer) = requested_pair(1, 1);
        peer.write_all(&[2, 3, 0, 0, 0, 0, 0, 0, 1, b'x']).unwrap();
        peer.write_all(&update(&[raw(0, 0, 1, 1, [1; 4])])).unwrap();
        assert!(client.read_update(TIMEOUT).unwrap().complete);
        client.request_update(true, TIMEOUT).unwrap();
        let mut request = [0; 10];
        peer.read_exact(&mut request).unwrap();
        peer.write_all(&[0, 0, 0]).unwrap();
        drop(peer);
        assert_eq!(
            client.read_update(TIMEOUT).unwrap_err().kind(),
            io::ErrorKind::UnexpectedEof
        );
    }

    #[test]
    fn resize_invalidates_geometry_generation_and_requires_a_new_complete_frame() {
        let (mut client, mut peer) = requested_pair(1, 1);
        peer.write_all(&update(&[raw(0, 0, 1, 1, [1; 4])])).unwrap();
        client.read_update(TIMEOUT).unwrap();
        let geometry = client.geometry();
        assert!(client.complete_frame().is_some());
        client.request_update(true, TIMEOUT).unwrap();
        // Same dimensions must invalidate cached oracle geometry too.
        peer.write_all(&update(&[rectangle(0, 0, 1, 1, -223)]))
            .unwrap();
        assert!(!client.read_update(TIMEOUT).unwrap().complete);
        assert_ne!(geometry, client.geometry());
        assert_eq!(client.geometry().generation, 1);
        assert!(client.complete_frame().is_none());
        client.request_update(false, TIMEOUT).unwrap();
        peer.write_all(&update(&[raw(0, 0, 1, 1, [2; 4])])).unwrap();
        assert!(client.read_update(TIMEOUT).unwrap().complete);
        assert_eq!(client.complete_frame().unwrap().pixels, [[2; 3]]);
    }

    #[test]
    fn repeated_readiness_timeout_does_not_send_a_second_request() {
        let (mut client, mut peer) = connected_pair(1, 1);
        let error = client
            .poll_full(Deadline::new(Duration::from_millis(20)).unwrap())
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        let mut request = [0; 10];
        peer.read_exact(&mut request).unwrap();
        assert_eq!(request, [3, 0, 0, 0, 0, 0, 0, 1, 0, 1]);
        let error = client
            .poll_full(Deadline::new(Duration::from_millis(20)).unwrap())
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        peer.set_read_timeout(Some(Duration::from_millis(20)))
            .unwrap();
        let mut byte = [0];
        assert!(matches!(
            peer.read(&mut byte).unwrap_err().kind(),
            io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
        ));
        peer.write_all(&update(&[raw(0, 0, 1, 1, [5; 4])])).unwrap();
        assert!(
            client
                .poll_full(Deadline::new(TIMEOUT).unwrap())
                .unwrap()
                .complete
        );
        assert!(!client.request_pending());
    }

    #[test]
    fn no_request_means_no_update_oracle_and_connection_budget_is_case_wide() {
        let (mut client, _) = connected_pair(1, 1);
        assert!(client.complete_frame().is_none());
        assert_eq!(
            client.read_update(TIMEOUT).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        let mut connector = Connector::new(Deadline::new(Duration::from_secs(30)).unwrap());
        // Each local peer closes during negotiation: no VMware dependency.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            for _ in 0..CONNECT_ATTEMPTS {
                drop(listener.accept().unwrap().0);
            }
        });
        for _ in 0..CONNECT_ATTEMPTS {
            assert!(connector.connect(address).is_err());
        }
        assert_eq!(connector.attempts(), CONNECT_ATTEMPTS);
        assert!(
            connector
                .connect(address)
                .err()
                .unwrap()
                .to_string()
                .contains("count cap")
        );
        server.join().unwrap();
        assert!(Client::connect("192.0.2.1:5901".parse().unwrap(), TIMEOUT).is_err());
    }

    #[test]
    fn aggregate_rectangle_bytes_reject_before_unbounded_allocation() {
        // Two 4096x4096 raw payloads plus headers exceed 128 MiB. Build only
        // the first payload and second header; rejection precedes the second.
        let frame = Frame::new(4096, 4096).unwrap();
        let mut bytes = vec![0, 0, 0, 2];
        bytes.extend_from_slice(&rectangle(0, 0, 4096, 4096, 0));
        bytes.resize(bytes.len() + 4096 * 4096 * 4, 0);
        bytes.extend_from_slice(&rectangle(0, 0, 4096, 4096, 0));
        assert_eq!(
            parse_message(&bytes, &frame).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }
}

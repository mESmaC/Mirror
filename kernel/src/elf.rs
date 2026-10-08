//! Bounded, allocation-free loader metadata for native static ELF64 programs.
//!
//! This is deliberately a narrow ABI: little-endian x86-64 ET_EXEC images with
//! page-aligned, readable, non-overlapping PT_LOAD segments. Section headers are
//! not used to load a program. No interpreter, dynamic linker, or TLS is present.
//! Layout definitions: https://refspecs.linuxfoundation.org/elf/gabi4+/ch4.eheader.html
//! and https://refspecs.linuxfoundation.org/elf/gabi4+/ch5.pheader.html.

const HEADER_SIZE: usize = 64;
const PROGRAM_HEADER_SIZE: usize = 56;
const MAX_PROGRAM_HEADERS: usize = 16;
const PAGE_SIZE: u64 = 4096;
const USER_LIMIT: u64 = 0x7ffe_f000;
const MAX_MAPPED_BYTES: u64 = 2 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ElfError {
    Truncated,
    BadMagic,
    UnsupportedFormat,
    InvalidHeader,
    TooManyProgramHeaders,
    UnsupportedSegment,
    InvalidSegment,
    InvalidAlignment,
    UnreadableSegment,
    WritableExecutable,
    AddressOutOfRange,
    ImageTooLarge,
    OverlappingSegments,
    InvalidEntry,
    NoLoadSegments,
}

#[derive(Clone, Copy, Debug)]
pub struct Segment {
    pub virtual_address: u64,
    pub file_offset: usize,
    pub file_size: usize,
    pub memory_size: u64,
    pub writable: bool,
    pub executable: bool,
}

impl Segment {
    const EMPTY: Self = Self {
        virtual_address: 0,
        file_offset: 0,
        file_size: 0,
        memory_size: 0,
        writable: false,
        executable: false,
    };

    fn mapped_end(self) -> u64 {
        // parse() validates both additions before storing a segment.
        (self.virtual_address + self.memory_size + PAGE_SIZE - 1) & !(PAGE_SIZE - 1)
    }
}

#[derive(Debug)]
pub struct Image {
    pub entry: u64,
    segments: [Segment; MAX_PROGRAM_HEADERS],
    count: usize,
}

impl Image {
    pub fn segments(&self) -> &[Segment] {
        &self.segments[..self.count]
    }
}

fn read<const N: usize>(bytes: &[u8], offset: usize) -> Result<[u8; N], ElfError> {
    let end = offset.checked_add(N).ok_or(ElfError::Truncated)?;
    bytes
        .get(offset..end)
        .ok_or(ElfError::Truncated)?
        .try_into()
        .map_err(|_| ElfError::Truncated)
}

fn u16_at(bytes: &[u8], offset: usize) -> Result<u16, ElfError> {
    Ok(u16::from_le_bytes(read(bytes, offset)?))
}

fn u32_at(bytes: &[u8], offset: usize) -> Result<u32, ElfError> {
    Ok(u32::from_le_bytes(read(bytes, offset)?))
}

fn u64_at(bytes: &[u8], offset: usize) -> Result<u64, ElfError> {
    Ok(u64::from_le_bytes(read(bytes, offset)?))
}

pub fn parse(bytes: &[u8]) -> Result<Image, ElfError> {
    if bytes.len() < HEADER_SIZE {
        return Err(ElfError::Truncated);
    }
    if &bytes[..4] != b"\x7fELF" {
        return Err(ElfError::BadMagic);
    }
    // ELFCLASS64, ELFDATA2LSB, current version, System V ABI, ABI version zero.
    if bytes[4..9] != [2, 1, 1, 0, 0]
        || u16_at(bytes, 16)? != 2
        || u16_at(bytes, 18)? != 62
        || u32_at(bytes, 20)? != 1
    {
        return Err(ElfError::UnsupportedFormat);
    }
    if u32_at(bytes, 48)? != 0
        || u16_at(bytes, 52)? as usize != HEADER_SIZE
        || u16_at(bytes, 54)? as usize != PROGRAM_HEADER_SIZE
    {
        return Err(ElfError::InvalidHeader);
    }

    let program_count = u16_at(bytes, 56)? as usize;
    if program_count == 0 {
        return Err(ElfError::NoLoadSegments);
    }
    if program_count > MAX_PROGRAM_HEADERS {
        return Err(ElfError::TooManyProgramHeaders);
    }
    let program_offset = usize::try_from(u64_at(bytes, 32)?).map_err(|_| ElfError::Truncated)?;
    if program_offset < HEADER_SIZE || program_offset & 7 != 0 {
        return Err(ElfError::InvalidHeader);
    }
    let table_size = program_count
        .checked_mul(PROGRAM_HEADER_SIZE)
        .ok_or(ElfError::Truncated)?;
    let table_end = program_offset
        .checked_add(table_size)
        .ok_or(ElfError::Truncated)?;
    let table = bytes
        .get(program_offset..table_end)
        .ok_or(ElfError::Truncated)?;

    let mut image = Image {
        entry: u64_at(bytes, 24)?,
        segments: [Segment::EMPTY; MAX_PROGRAM_HEADERS],
        count: 0,
    };
    let mut mapped_bytes = 0u64;
    for header in table.chunks_exact(PROGRAM_HEADER_SIZE) {
        let segment_type = u32_at(header, 0)?;
        match segment_type {
            // PT_DYNAMIC, PT_INTERP, PT_TLS need runtime support we do not have.
            2 | 3 | 7 => return Err(ElfError::UnsupportedSegment),
            // Refuse an explicit executable-stack request rather than silently
            // loading an image whose stack requirements cannot be honored.
            0x6474_e551 if u32_at(header, 4)? & 1 != 0 => {
                return Err(ElfError::WritableExecutable);
            }
            1 => {}
            // Other metadata does not create mappings or alter permissions.
            _ => continue,
        }

        let flags = u32_at(header, 4)?;
        let offset = u64_at(header, 8)?;
        let virtual_address = u64_at(header, 16)?;
        let file_size = u64_at(header, 32)?;
        let memory_size = u64_at(header, 40)?;
        let alignment = u64_at(header, 48)?;
        if file_size > memory_size {
            return Err(ElfError::InvalidSegment);
        }
        if memory_size == 0 {
            continue;
        }
        if flags & !7 != 0 {
            return Err(ElfError::InvalidSegment);
        }
        if flags & 4 == 0 {
            return Err(ElfError::UnreadableSegment);
        }
        if flags & 3 == 3 {
            return Err(ElfError::WritableExecutable);
        }
        // The ELF ABI permits p_align 0 or 1 to mean no extra alignment.
        // Mirror independently requires aligned file and virtual page starts.
        if offset & (PAGE_SIZE - 1) != 0
            || virtual_address & (PAGE_SIZE - 1) != 0
            || (alignment > 1
                && (!alignment.is_power_of_two()
                    || offset % alignment != virtual_address % alignment))
        {
            return Err(ElfError::InvalidAlignment);
        }
        let memory_end = virtual_address
            .checked_add(memory_size)
            .ok_or(ElfError::AddressOutOfRange)?;
        let mapped_end = memory_end
            .checked_add(PAGE_SIZE - 1)
            .ok_or(ElfError::AddressOutOfRange)?
            & !(PAGE_SIZE - 1);
        if virtual_address < PAGE_SIZE || mapped_end > USER_LIMIT {
            return Err(ElfError::AddressOutOfRange);
        }
        let file_offset = usize::try_from(offset).map_err(|_| ElfError::Truncated)?;
        let file_size = usize::try_from(file_size).map_err(|_| ElfError::Truncated)?;
        let file_end = file_offset
            .checked_add(file_size)
            .ok_or(ElfError::Truncated)?;
        if file_end > bytes.len() {
            return Err(ElfError::Truncated);
        }
        mapped_bytes = mapped_bytes
            .checked_add(mapped_end - virtual_address)
            .ok_or(ElfError::ImageTooLarge)?;
        if mapped_bytes > MAX_MAPPED_BYTES {
            return Err(ElfError::ImageTooLarge);
        }
        for previous in image.segments() {
            if virtual_address < previous.mapped_end() && previous.virtual_address < mapped_end {
                return Err(ElfError::OverlappingSegments);
            }
        }
        image.segments[image.count] = Segment {
            virtual_address,
            file_offset,
            file_size,
            memory_size,
            writable: flags & 2 != 0,
            executable: flags & 1 != 0,
        };
        image.count += 1;
    }
    if image.count == 0 {
        return Err(ElfError::NoLoadSegments);
    }
    if !image.segments().iter().any(|segment| {
        segment.executable
            && image.entry >= segment.virtual_address
            && image.entry < segment.virtual_address + segment.memory_size
    }) {
        return Err(ElfError::InvalidEntry);
    }
    Ok(image)
}

#[cfg(feature = "boot-smoke")]
pub fn self_test() {
    fn put16(bytes: &mut [u8], offset: usize, value: u16) {
        bytes[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
    }
    fn put32(bytes: &mut [u8], offset: usize, value: u32) {
        bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
    }
    fn put64(bytes: &mut [u8], offset: usize, value: u64) {
        bytes[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
    }
    fn reject(template: &[u8; 512], error: ElfError, mutate: fn(&mut [u8; 512])) {
        let mut bytes = *template;
        mutate(&mut bytes);
        assert_eq!(parse(&bytes).unwrap_err(), error);
    }

    // One RX load segment backed by the entire small fixture, with a real
    // in-segment entry. A second header fits when testing combined mappings.
    let mut bytes = [0u8; 512];
    bytes[..9].copy_from_slice(&[0x7f, b'E', b'L', b'F', 2, 1, 1, 0, 0]);
    put16(&mut bytes, 16, 2);
    put16(&mut bytes, 18, 62);
    put32(&mut bytes, 20, 1);
    put64(&mut bytes, 24, 0x400100);
    put64(&mut bytes, 32, 64);
    put16(&mut bytes, 52, 64);
    put16(&mut bytes, 54, 56);
    put16(&mut bytes, 56, 1);
    put32(&mut bytes, 64, 1);
    put32(&mut bytes, 68, 5);
    put64(&mut bytes, 80, 0x400000);
    put64(&mut bytes, 96, 512);
    put64(&mut bytes, 104, 512);
    put64(&mut bytes, 112, 4096);

    let parsed = parse(&bytes).expect("valid native ELF fixture");
    assert_eq!(parsed.entry, 0x400100);
    assert_eq!(parsed.segments().len(), 1);
    assert_eq!(parsed.segments()[0].file_size, 512);
    assert_eq!(parsed.segments()[0].memory_size, 512);
    assert!(!parsed.segments()[0].writable);
    assert!(parsed.segments()[0].executable);
    assert_eq!(parse(&bytes[..63]).unwrap_err(), ElfError::Truncated);

    reject(&bytes, ElfError::BadMagic, |b| b[0] = 0);
    reject(&bytes, ElfError::UnsupportedFormat, |b| b[4] = 1);
    reject(&bytes, ElfError::UnsupportedFormat, |b| b[5] = 2);
    reject(&bytes, ElfError::UnsupportedFormat, |b| put16(b, 16, 3));
    reject(&bytes, ElfError::UnsupportedFormat, |b| put16(b, 18, 3));
    reject(&bytes, ElfError::InvalidHeader, |b| put32(b, 48, 1));
    reject(&bytes, ElfError::InvalidHeader, |b| put16(b, 52, 63));
    reject(&bytes, ElfError::InvalidHeader, |b| put16(b, 54, 55));
    reject(&bytes, ElfError::TooManyProgramHeaders, |b| {
        put16(b, 56, 17)
    });
    reject(&bytes, ElfError::NoLoadSegments, |b| put16(b, 56, 0));
    reject(&bytes, ElfError::Truncated, |b| put64(b, 32, u64::MAX - 7));
    reject(&bytes, ElfError::Truncated, |b| put64(b, 32, 512));
    reject(&bytes, ElfError::InvalidHeader, |b| put64(b, 32, 65));
    reject(&bytes, ElfError::Truncated, |b| put64(b, 72, 4096));
    reject(&bytes, ElfError::Truncated, |b| {
        put64(b, 96, 513);
        put64(b, 104, 513);
    });
    reject(&bytes, ElfError::InvalidSegment, |b| put64(b, 104, 511));
    reject(&bytes, ElfError::WritableExecutable, |b| put32(b, 68, 7));
    reject(&bytes, ElfError::UnreadableSegment, |b| put32(b, 68, 1));
    reject(&bytes, ElfError::InvalidSegment, |b| put32(b, 68, 13));
    reject(&bytes, ElfError::InvalidAlignment, |b| put64(b, 72, 1));
    reject(&bytes, ElfError::InvalidAlignment, |b| {
        put64(b, 80, 0x400001)
    });
    reject(&bytes, ElfError::InvalidAlignment, |b| put64(b, 112, 3));
    reject(&bytes, ElfError::AddressOutOfRange, |b| put64(b, 80, 0));
    reject(&bytes, ElfError::AddressOutOfRange, |b| {
        put64(b, 80, USER_LIMIT)
    });
    reject(&bytes, ElfError::AddressOutOfRange, |b| {
        put64(b, 104, u64::MAX)
    });
    reject(&bytes, ElfError::ImageTooLarge, |b| {
        put64(b, 104, MAX_MAPPED_BYTES + 1)
    });
    reject(&bytes, ElfError::InvalidEntry, |b| put64(b, 24, 0x400200));
    reject(&bytes, ElfError::InvalidEntry, |b| put32(b, 68, 6));
    for segment_type in [2, 3, 7] {
        let mut unsupported = bytes;
        put32(&mut unsupported, 64, segment_type);
        assert_eq!(
            parse(&unsupported).unwrap_err(),
            ElfError::UnsupportedSegment
        );
    }

    // The first segment ends one byte into its second page; a second segment
    // starting there must be rejected even though its byte range could differ.
    reject(&bytes, ElfError::OverlappingSegments, |b| {
        put16(b, 56, 2);
        put64(b, 104, 4097);
        b.copy_within(64..120, 120);
        put64(b, 136, 0x401000);
        put64(b, 160, 512);
    });
    reject(&bytes, ElfError::ImageTooLarge, |b| {
        put16(b, 56, 2);
        put64(b, 104, MAX_MAPPED_BYTES);
        b.copy_within(64..120, 120);
        put64(b, 136, 0x600000);
        put64(b, 160, 1);
        put64(b, 152, 0);
    });

    // File bytes may be followed by zero-filled BSS. Empty LOADs are ignored.
    put64(&mut bytes, 104, 1024);
    assert_eq!(parse(&bytes).unwrap().segments()[0].memory_size, 1024);
    put16(&mut bytes, 56, 2);
    put32(&mut bytes, 120, 1);
    assert_eq!(parse(&bytes).unwrap().segments().len(), 1);
    crate::serial::println!("MIRROR_ELF_TESTS_OK");
}

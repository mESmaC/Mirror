//! Bounded PPM frames and explicit, recorded pixel comparisons.

use crate::rfb::Frame;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::Path;
use std::str::FromStr;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rect {
    pub x: u16,
    pub y: u16,
    pub width: u16,
    pub height: u16,
}

impl Rect {
    pub fn validate(self, frame: &Frame) -> io::Result<()> {
        frame.validate()?;
        if self.width == 0
            || self.height == 0
            || u32::from(self.x) + u32::from(self.width) > u32::from(frame.width)
            || u32::from(self.y) + u32::from(self.height) > u32::from(frame.height)
        {
            return Err(invalid("rectangle is empty or outside the framebuffer"));
        }
        Ok(())
    }

    fn contains(self, x: u16, y: u16) -> bool {
        x >= self.x
            && y >= self.y
            && u32::from(x) < u32::from(self.x) + u32::from(self.width)
            && u32::from(y) < u32::from(self.y) + u32::from(self.height)
    }
}

impl std::fmt::Display for Rect {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(out, "{},{},{},{}", self.x, self.y, self.width, self.height)
    }
}

impl FromStr for Rect {
    type Err = io::Error;
    fn from_str(text: &str) -> io::Result<Self> {
        let mut fields = text.split(',');
        let mut values = [0_u16; 4];
        for value in &mut values {
            let field = fields
                .next()
                .ok_or_else(|| invalid("rectangle needs four fields"))?;
            if field.is_empty() || field.len() > 5 || !field.bytes().all(|b| b.is_ascii_digit()) {
                return Err(invalid("rectangle field must be an unsigned decimal u16"));
            }
            *value = field
                .parse()
                .map_err(|_| invalid("rectangle field exceeds u16"))?;
        }
        if fields.next().is_some() || values[2] == 0 || values[3] == 0 {
            return Err(invalid("rectangle must be nonempty x,y,width,height"));
        }
        Ok(Self {
            x: values[0],
            y: values[1],
            width: values[2],
            height: values[3],
        })
    }
}

pub fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

/// Write a new artifact, removing only this new file if its write fails.
pub fn write_new(path: &Path, write: impl FnOnce(&mut File) -> io::Result<()>) -> io::Result<()> {
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    let result = write(&mut file).and_then(|()| file.sync_all());
    drop(file);
    if result.is_err() {
        let _ = fs::remove_file(path);
    }
    result
}

pub fn write_ppm(path: &Path, frame: &Frame) -> io::Result<()> {
    frame.validate()?;
    write_new(path, |file| {
        let mut writer = io::BufWriter::new(file);
        write!(writer, "P6\n{} {}\n255\n", frame.width, frame.height)?;
        for pixel in &frame.pixels {
            writer.write_all(pixel)?;
        }
        writer.flush()
    })
}

pub fn read_ppm(path: &Path) -> io::Result<Frame> {
    const MAX_BYTES: usize = 4096 * 4096 * 3 + 32768 + 2;
    let mut bytes = Vec::new();
    File::open(path)?
        .take((MAX_BYTES + 1) as u64)
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_BYTES {
        return Err(invalid("PPM file exceeds the size bound"));
    }
    parse_ppm(&bytes)
}

fn parse_ppm(bytes: &[u8]) -> io::Result<Frame> {
    let mut cursor = 0;
    let mut tokens = Vec::new();
    while tokens.len() < 4 {
        loop {
            while bytes.get(cursor).is_some_and(u8::is_ascii_whitespace) {
                cursor += 1;
                if cursor > 32768 {
                    return Err(invalid("oversized PPM header"));
                }
            }
            if bytes.get(cursor) == Some(&b'#') {
                while bytes.get(cursor).is_some_and(|b| *b != b'\n') {
                    cursor += 1;
                    if cursor > 32768 {
                        return Err(invalid("oversized PPM header"));
                    }
                }
            } else {
                break;
            }
        }
        let start = cursor;
        while bytes.get(cursor).is_some_and(|b| !b.is_ascii_whitespace()) {
            cursor += 1;
            if cursor > 32768 {
                return Err(invalid("oversized PPM header"));
            }
        }
        if cursor == start || cursor > 32768 {
            return Err(invalid("invalid or oversized PPM header"));
        }
        tokens.push(
            std::str::from_utf8(&bytes[start..cursor])
                .map_err(|_| invalid("PPM header is not ASCII"))?,
        );
    }
    if tokens[0] != "P6" || tokens[3] != "255" {
        return Err(invalid("expected P6 RGB8 PPM"));
    }
    let width: u16 = tokens[1]
        .parse()
        .map_err(|_| invalid("invalid PPM width"))?;
    let height: u16 = tokens[2]
        .parse()
        .map_err(|_| invalid("invalid PPM height"))?;
    match bytes.get(cursor) {
        Some(b'\r') if bytes.get(cursor + 1) == Some(&b'\n') => cursor += 2,
        Some(b) if b.is_ascii_whitespace() => cursor += 1,
        _ => return Err(invalid("missing PPM raster separator")),
    }
    let mut frame = Frame::new(width, height)?;
    let raster = &bytes[cursor..];
    if raster.len() != frame.pixels.len() * 3 {
        return Err(invalid("truncated or trailing PPM data"));
    }
    for (pixel, rgb) in frame.pixels.iter_mut().zip(raster.as_chunks::<3>().0) {
        pixel.copy_from_slice(rgb);
    }
    Ok(frame)
}

#[derive(Debug, PartialEq, Eq)]
pub struct Difference {
    pub compared: u64,
    pub changed: u64,
}

/// Count pixels with any RGB channel differing by more than `tolerance`.
/// Masks exclude pixels, and all geometry must be inside both frames.
pub fn difference(
    reference: &Frame,
    observed: &Frame,
    rect: Rect,
    masks: &[Rect],
    tolerance: u8,
) -> io::Result<Difference> {
    reference.validate()?;
    observed.validate()?;
    if (reference.width, reference.height) != (observed.width, observed.height)
        || reference.pixels.len() != observed.pixels.len()
        || reference.pixels.len() != usize::from(reference.width) * usize::from(reference.height)
    {
        return Err(invalid("frame dimensions do not match"));
    }
    if masks.len() > 256 {
        return Err(invalid("at most 256 masks are supported"));
    }
    rect.validate(reference)?;
    for mask in masks {
        mask.validate(reference)?;
    }
    let mut result = Difference {
        compared: 0,
        changed: 0,
    };
    for y in rect.y..rect.y + rect.height {
        for x in rect.x..rect.x + rect.width {
            if masks.iter().any(|mask| mask.contains(x, y)) {
                continue;
            }
            let index = usize::from(y) * usize::from(reference.width) + usize::from(x);
            result.compared += 1;
            if reference.pixels[index]
                .iter()
                .zip(observed.pixels[index])
                .any(|(a, b)| a.abs_diff(b) > tolerance)
            {
                result.changed += 1;
            }
        }
    }
    if result.compared == 0 {
        return Err(invalid("masks exclude the entire comparison"));
    }
    Ok(result)
}

pub fn full_rect(frame: &Frame) -> Rect {
    Rect {
        x: 0,
        y: 0,
        width: frame.width,
        height: frame.height,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ppm_keeps_whitespace_and_hash_pixel_bytes() {
        let frame = parse_ppm(b"P6\n# example\n1 1\n255\n\n #").unwrap();
        assert_eq!(frame.pixels, vec![[10, 32, 35]]);
        assert!(parse_ppm(b"P6\n1 1\n255\n12").is_err());
        assert!(parse_ppm(b"P6\n1 1\n255\n1234").is_err());
        assert!(parse_ppm(b"P6\n65535 65535\n255\n").is_err());
    }

    #[test]
    fn masks_and_threshold_are_pixel_based() {
        let reference = Frame::new(2, 2).unwrap();
        let mut observed = Frame::new(2, 2).unwrap();
        observed.pixels[0] = [2, 3, 4];
        observed.pixels[3] = [10, 10, 10];
        let mask = Rect {
            x: 1,
            y: 1,
            width: 1,
            height: 1,
        };
        assert_eq!(
            difference(&reference, &observed, full_rect(&reference), &[mask], 3).unwrap(),
            Difference {
                compared: 3,
                changed: 1
            }
        );
        assert_eq!(
            difference(&reference, &observed, full_rect(&reference), &[mask], 4)
                .unwrap()
                .changed,
            0
        );
        assert!(
            difference(
                &reference,
                &observed,
                full_rect(&reference),
                &[full_rect(&reference)],
                0
            )
            .is_err()
        );
    }

    #[test]
    fn rejects_invalid_geometry() {
        assert!("1,2,0,4".parse::<Rect>().is_err());
        assert!("1,2,3,4,5".parse::<Rect>().is_err());
        let frame = Frame::new(2, 2).unwrap();
        assert!("1,1,2,2".parse::<Rect>().unwrap().validate(&frame).is_err());
        assert!("+1,1,1,1".parse::<Rect>().is_err());
        assert!("1,1,1,0000001".parse::<Rect>().is_err());
        let invalid = Frame {
            width: 0,
            height: 1,
            pixels: vec![],
        };
        assert!(difference(&invalid, &invalid, full_rect(&invalid), &[], 0).is_err());
        let invalid = Frame {
            width: 1,
            height: 1,
            pixels: vec![],
        };
        assert!(
            "0,0,1,1"
                .parse::<Rect>()
                .unwrap()
                .validate(&invalid)
                .is_err()
        );
    }

    #[test]
    fn oversized_header_is_rejected_without_consuming_the_raster_as_header() {
        assert!(parse_ppm(&vec![b' '; 32769]).is_err());
        let mut comment = Vec::from(b"P6\n#".as_slice());
        comment.resize(32769, b'x');
        assert!(parse_ppm(&comment).is_err());
    }
}

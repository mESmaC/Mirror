//! Streaming SHA-256 for package and evidence identity (FIPS 180-4).
use std::io::{self, Read};
use std::path::Path;
use std::time::Instant;

const K: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

struct State {
    words: [u32; 8],
    block: [u8; 64],
    used: usize,
    bytes: u64,
}
impl State {
    fn new() -> Self {
        Self {
            words: [
                0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
                0x5be0cd19,
            ],
            block: [0; 64],
            used: 0,
            bytes: 0,
        }
    }
    fn compress(&mut self) {
        let mut w = [0u32; 64];
        for (i, chunk) in self.block.as_chunks::<4>().0.iter().enumerate() {
            w[i] = u32::from_be_bytes(*chunk);
        }
        for i in 16..64 {
            let x = w[i - 15];
            let y = w[i - 2];
            let s0 = x.rotate_right(7) ^ x.rotate_right(18) ^ (x >> 3);
            let s1 = y.rotate_right(17) ^ y.rotate_right(19) ^ (y >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = self.words;
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let t1 = h
                .wrapping_add(s1)
                .wrapping_add((e & f) ^ (!e & g))
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let t2 = s0.wrapping_add((a & b) ^ (a & c) ^ (b & c));
            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        for (word, value) in self.words.iter_mut().zip([a, b, c, d, e, f, g, h]) {
            *word = word.wrapping_add(value);
        }
    }
    fn update(&mut self, mut data: &[u8]) {
        self.bytes = self
            .bytes
            .checked_add(data.len() as u64)
            .expect("SHA256 input length overflow");
        while !data.is_empty() {
            let take = (64 - self.used).min(data.len());
            self.block[self.used..self.used + take].copy_from_slice(&data[..take]);
            self.used += take;
            data = &data[take..];
            if self.used == 64 {
                self.compress();
                self.used = 0;
            }
        }
    }
    fn finish(mut self) -> String {
        let bits = self
            .bytes
            .checked_mul(8)
            .expect("SHA256 bit length overflow");
        self.update(&[0x80]);
        while self.used != 56 {
            self.update(&[0]);
        }
        self.update(&bits.to_be_bytes());
        self.words.iter().map(|w| format!("{w:08x}")).collect()
    }
}

pub fn bytes(data: &[u8]) -> String {
    let mut s = State::new();
    s.update(data);
    s.finish()
}
pub fn reader(mut input: impl Read) -> io::Result<(u64, String)> {
    reader_checked(&mut input, || Ok(()))
}

/// Check an absolute deadline between every fixed-size read and compression.
/// Regular file reads are synchronous OS operations; progress never renews the
/// CPU/read-loop budget, and a completed late read is rejected before hashing.
pub fn reader_until(input: impl Read, deadline: Instant) -> io::Result<(u64, String)> {
    reader_checked(input, || {
        if Instant::now() >= deadline {
            Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "hash deadline expired",
            ))
        } else {
            Ok(())
        }
    })
}

fn reader_checked(
    mut input: impl Read,
    mut check: impl FnMut() -> io::Result<()>,
) -> io::Result<(u64, String)> {
    let mut s = State::new();
    let mut chunk = [0; 65536];
    loop {
        check()?;
        let n = input.read(&mut chunk)?;
        check()?;
        if n == 0 {
            break;
        }
        s.update(&chunk[..n]);
    }
    let size = s.bytes;
    let hash = s.finish();
    check()?;
    Ok((size, hash))
}
pub fn file(path: &Path) -> io::Result<(u64, String)> {
    reader(std::fs::File::open(path)?)
}

pub fn file_until(path: &Path, deadline: Instant) -> io::Result<(u64, String)> {
    reader_until(std::fs::File::open(path)?, deadline)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn total_deadline_interrupts_a_progressing_stream() {
        let mut checks = 0;
        let mut source = std::io::Cursor::new(vec![1; 3 * 65536]);
        let error = reader_checked(&mut source, || {
            checks += 1;
            if checks == 4 {
                Err(io::Error::new(io::ErrorKind::TimedOut, "fixture deadline"))
            } else {
                Ok(())
            }
        })
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert_eq!(source.position(), 2 * 65536);
        assert_eq!(
            reader_until(&b"abc"[..], Instant::now())
                .unwrap_err()
                .kind(),
            io::ErrorKind::TimedOut
        );
    }
    #[test]
    fn published_vectors_and_chunk_boundaries() {
        assert_eq!(
            bytes(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            bytes(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            bytes(&vec![b'a'; 1_000_000]),
            "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0"
        );
        for n in [55, 56, 63, 64, 65, 1000] {
            let data = vec![23; n];
            let mut s = State::new();
            for b in &data {
                s.update(&[*b]);
            }
            assert_eq!(s.finish(), bytes(&data));
        }
    }
}

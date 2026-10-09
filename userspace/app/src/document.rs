//! A bounded, process-local ASCII document. Caret movement never changes text.

pub const CAPACITY: usize = 96;

pub struct Document {
    bytes: [u8; CAPACITY],
    length: usize,
    caret: usize,
}

impl Document {
    pub const fn new() -> Self {
        Self {
            bytes: [0; CAPACITY],
            length: 0,
            caret: 0,
        }
    }

    pub fn text(&self) -> &[u8] {
        &self.bytes[..self.length]
    }

    pub fn caret(&self) -> usize {
        self.caret
    }

    pub fn place(&mut self, position: usize) -> bool {
        let previous = self.caret;
        self.caret = position.min(self.length);
        self.caret != previous
    }

    /// Returns (repaint, text changed). Key kinds match the native input ABI.
    pub fn key(&mut self, kind: u32, ascii: u8) -> (bool, bool) {
        match kind {
            1 if (32..=126).contains(&ascii) && self.length < CAPACITY => {
                self.bytes
                    .copy_within(self.caret..self.length, self.caret + 1);
                self.bytes[self.caret] = ascii;
                self.caret += 1;
                self.length += 1;
                (true, true)
            }
            3 if self.caret != 0 => {
                self.bytes
                    .copy_within(self.caret..self.length, self.caret - 1);
                self.caret -= 1;
                self.length -= 1;
                self.bytes[self.length] = 0;
                (true, true)
            }
            6 => (self.place(self.caret.saturating_sub(1)), false),
            7 => (self.place(self.caret + 1), false),
            8 => (self.place(0), false),
            9 => (self.place(self.length), false),
            10 if self.caret != self.length => {
                self.bytes
                    .copy_within(self.caret + 1..self.length, self.caret);
                self.length -= 1;
                self.bytes[self.length] = 0;
                (true, true)
            }
            _ => (false, false),
        }
    }
}

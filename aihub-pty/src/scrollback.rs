/// Fixed-capacity byte ring for PTY output history.
pub struct Scrollback {
    buf: Vec<u8>,
    cap: usize,
    start: usize,
    len: usize,
}

impl Scrollback {
    pub fn new(cap: usize) -> Self {
        Self {
            buf: vec![0; cap],
            cap,
            start: 0,
            len: 0,
        }
    }

    pub fn append(&mut self, data: &[u8]) {
        for &byte in data {
            if self.len < self.cap {
                let idx = (self.start + self.len) % self.cap;
                self.buf[idx] = byte;
                self.len += 1;
            } else {
                self.buf[self.start] = byte;
                self.start = (self.start + 1) % self.cap;
            }
        }
    }

    pub fn snapshot(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.len);
        for i in 0..self.len {
            let idx = (self.start + i) % self.cap;
            out.push(self.buf[idx]);
        }
        out
    }
}

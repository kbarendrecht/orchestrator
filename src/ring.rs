use std::collections::VecDeque;

/// Bounded byte buffer holding the tail of a pty's output.
///
/// The SPA is stateless and disposable (§1): closing the browser kills nothing
/// and reopening replays from here. Buffers are in-memory only and are not
/// persisted across a daemon restart — session *records* are (§2).
pub struct RingBuffer {
    buf: VecDeque<u8>,
    cap: usize,
}

impl RingBuffer {
    pub fn new(cap: usize) -> Self {
        RingBuffer {
            buf: VecDeque::with_capacity(cap.min(64 * 1024)),
            cap,
        }
    }

    pub fn push(&mut self, bytes: &[u8]) {
        // A single write larger than the whole buffer keeps only its tail.
        if bytes.len() >= self.cap {
            self.buf.clear();
            self.buf.extend(&bytes[bytes.len() - self.cap..]);
            return;
        }
        let overflow = (self.buf.len() + bytes.len()).saturating_sub(self.cap);
        self.buf.drain(..overflow);
        self.buf.extend(bytes);
    }

    pub fn snapshot(&self) -> Vec<u8> {
        // Two memcpys, not a byte at a time: this runs on every attach and resync.
        let (a, b) = self.buf.as_slices();
        let mut out = Vec::with_capacity(a.len() + b.len());
        out.extend_from_slice(a);
        out.extend_from_slice(b);
        out
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.buf.len()
    }

    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_the_tail_when_it_overflows() {
        let mut r = RingBuffer::new(4);
        r.push(b"abc");
        r.push(b"de");
        assert_eq!(r.snapshot(), b"bcde");
    }

    #[test]
    fn a_write_larger_than_the_buffer_keeps_only_its_tail() {
        let mut r = RingBuffer::new(3);
        r.push(b"abcdefg");
        assert_eq!(r.snapshot(), b"efg");
    }

    #[test]
    fn stays_empty_until_written() {
        let r = RingBuffer::new(8);
        assert!(r.is_empty());
        assert_eq!(r.len(), 0);
    }
}

// Bounded capture of shell output (#442).
//
// A command's output is drained to the end no matter how large it is: the
// accumulator keeps a fixed head window and a rolling tail in memory and drops
// the middle, so a chatty command can never fill the pty/pipe, block, and time
// out with its output lost.

use std::collections::VecDeque;

/// Raw bytes kept from the start of the stream.
pub(crate) const RAW_HEAD_BYTES: usize = 64 * 1024;
/// Raw bytes kept from the end of the stream. Larger than the tail window the
/// model sees because it must also hold the PTY wrapper's shell-state block
/// (`pwd` + `env`), which is split off before rendering.
pub(crate) const RAW_TAIL_BYTES: usize = 1024 * 1024;

/// Collects a byte stream into a head window plus a rolling tail.
pub(crate) struct OutputAccumulator {
    head: Vec<u8>,
    tail: VecDeque<u8>,
    head_cap: usize,
    tail_cap: usize,
    omitted: u64,
}

/// What an [`OutputAccumulator`] retained once the stream ended.
#[derive(Debug)]
pub(crate) enum Captured {
    /// Nothing was dropped; these are all the bytes.
    Complete(Vec<u8>),
    /// The middle `omitted` bytes were dropped between `head` and `tail`.
    Truncated {
        head: Vec<u8>,
        tail: Vec<u8>,
        omitted: u64,
    },
}

impl OutputAccumulator {
    pub(crate) fn new(head_cap: usize, tail_cap: usize) -> Self {
        Self {
            head: Vec::new(),
            tail: VecDeque::new(),
            head_cap,
            tail_cap,
            omitted: 0,
        }
    }

    pub(crate) fn push(&mut self, bytes: &[u8]) {
        let room = self.head_cap.saturating_sub(self.head.len()).min(bytes.len());
        let (head, rest) = bytes.split_at(room);
        self.head.extend_from_slice(head);
        self.tail.extend(rest);
        let excess = self.tail.len().saturating_sub(self.tail_cap);
        if excess > 0 {
            self.tail.drain(..excess);
            self.omitted += excess as u64;
        }
    }

    pub(crate) fn finish(self) -> Captured {
        let Self {
            mut head,
            tail,
            omitted,
            ..
        } = self;
        if omitted == 0 {
            head.extend(tail);
            Captured::Complete(head)
        } else {
            Captured::Truncated {
                head,
                tail: tail.into(),
                omitted,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_stream_is_complete() {
        let mut acc = OutputAccumulator::new(4, 4);
        acc.push(b"abc");
        acc.push(b"def");
        match acc.finish() {
            Captured::Complete(bytes) => assert_eq!(bytes, b"abcdef"),
            other => panic!("expected Complete, got {other:?}"),
        }
    }

    #[test]
    fn large_stream_keeps_head_and_rolling_tail() {
        let mut acc = OutputAccumulator::new(4, 4);
        for chunk in [&b"0123"[..], b"4567", b"89ab", b"cdef"] {
            acc.push(chunk);
        }
        match acc.finish() {
            Captured::Truncated {
                head,
                tail,
                omitted,
            } => {
                assert_eq!(head, b"0123");
                assert_eq!(tail, b"cdef");
                assert_eq!(omitted, 8);
            }
            other => panic!("expected Truncated, got {other:?}"),
        }
    }
}

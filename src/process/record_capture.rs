//! Bounded capture for opt-in NUL-delimited listings. The process driver keeps
//! draining after capture stops, so a large Git listing cannot deadlock on a
//! full stdout pipe or turn a useful prefix into an output-limit failure.

pub(super) struct NulPrefix {
    bytes: Vec<u8>,
    records: usize,
    record_limit: usize,
    byte_limit: usize,
    truncated: bool,
}

impl NulPrefix {
    pub(super) fn new(record_limit: usize, byte_limit: usize) -> Self {
        Self {
            bytes: Vec::new(),
            records: 0,
            record_limit,
            byte_limit,
            truncated: false,
        }
    }

    pub(super) fn push(&mut self, mut chunk: &[u8]) {
        while !chunk.is_empty() && !self.truncated {
            if self.records >= self.record_limit || self.bytes.len() >= self.byte_limit {
                self.truncated = true;
                return;
            }
            let available = self.byte_limit - self.bytes.len();
            let end = chunk
                .iter()
                .position(|byte| *byte == 0)
                .map_or(chunk.len(), |index| index + 1);
            let take = end.min(available);
            self.bytes.extend_from_slice(&chunk[..take]);
            if chunk[take - 1] == 0 {
                self.records += 1;
            }
            chunk = &chunk[take..];
        }
    }

    pub(super) fn finish(self) -> (Vec<u8>, bool) {
        (self.bytes, self.truncated)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_accepts_every_chunk_boundary_and_drains_past_the_record_cap() {
        let input = b"one\0two\0three\0";
        for split in 0..=input.len() {
            let mut capture = NulPrefix::new(2, 100);
            capture.push(&input[..split]);
            capture.push(&input[split..]);
            let (bytes, truncated) = capture.finish();
            assert_eq!(bytes, b"one\0two\0");
            assert!(truncated);
        }
    }

    #[test]
    fn exact_limits_are_not_truncated_until_more_bytes_arrive() {
        let mut capture = NulPrefix::new(1, 4);
        capture.push(b"one\0");
        assert_eq!(capture.finish(), (b"one\0".to_vec(), false));
        let mut capture = NulPrefix::new(1, 4);
        capture.push(b"one\0");
        capture.push(b"x");
        assert_eq!(capture.finish(), (b"one\0".to_vec(), true));
        let mut capture = NulPrefix::new(0, 100);
        capture.push(b"");
        assert_eq!(capture.finish(), (vec![], false));
    }

    #[test]
    fn an_unterminated_record_cannot_allocate_beyond_the_byte_limit() {
        let mut capture = NulPrefix::new(10_000, 11);
        for _ in 0..20_000 {
            capture.push(b"a very long path without a nul");
        }
        let (bytes, truncated) = capture.finish();
        assert_eq!(bytes.len(), 11);
        assert!(truncated);
        let mut capture = NulPrefix::new(10_000, 11);
        capture.push(b"complete\0bad");
        assert_eq!(capture.finish(), (b"complete\0ba".to_vec(), true));
    }
}

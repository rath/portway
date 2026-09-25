//! The console's event backlog: every event it has seen, already serialized,
//! by sequence number. A page that reconnects asks for what came after the
//! last one it has; one that scrolls back asks for what came before.

use std::collections::VecDeque;
use std::sync::Arc;

/// Events kept for late or reconnecting pages: the TUI's event pane holds as
/// many.
pub const CAPACITY: usize = 10_000;

/// Asked for events the ring has already evicted.
#[derive(Debug, PartialEq, Eq)]
pub struct Gap;

pub struct Ring {
    items: VecDeque<(u64, Arc<str>)>,
    capacity: usize,
}

impl Ring {
    pub fn new(capacity: usize) -> Self {
        Ring {
            items: VecDeque::with_capacity(capacity.min(1024)),
            capacity,
        }
    }

    /// Sequence numbers only grow.
    pub fn push(&mut self, seq: u64, json: Arc<str>) {
        debug_assert!(self.items.back().is_none_or(|(last, _)| *last < seq));
        if self.items.len() == self.capacity {
            self.items.pop_front();
        }
        self.items.push_back((seq, json));
    }

    /// The newest sequence number, 0 before the first event.
    pub fn newest(&self) -> u64 {
        self.items.back().map_or(0, |(seq, _)| *seq)
    }

    /// The oldest sequence number still held, 0 before the first event.
    pub fn oldest(&self) -> u64 {
        self.items.front().map_or(0, |(seq, _)| *seq)
    }

    /// The last `count` events, oldest first.
    pub fn tail(&self, count: usize) -> Vec<Arc<str>> {
        let skip = self.items.len().saturating_sub(count);
        self.items
            .iter()
            .skip(skip)
            .map(|(_, json)| Arc::clone(json))
            .collect()
    }

    /// Every event after `seq`, or `Gap` when some of them were evicted.
    pub fn after(&self, seq: u64) -> Result<Vec<(u64, Arc<str>)>, Gap> {
        let oldest = self.oldest();
        if oldest > 1 && seq + 1 < oldest {
            return Err(Gap);
        }
        let start = self.items.partition_point(|(held, _)| *held <= seq);
        Ok(self.items.iter().skip(start).cloned().collect())
    }

    /// Up to `limit` events before `seq`, oldest first.
    pub fn before(&self, seq: u64, limit: usize) -> Vec<Arc<str>> {
        let end = self.items.partition_point(|(held, _)| *held < seq);
        self.items
            .range(end.saturating_sub(limit)..end)
            .map(|(_, json)| Arc::clone(json))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn filled(count: u64, capacity: usize) -> Ring {
        let mut ring = Ring::new(capacity);
        for seq in 1..=count {
            ring.push(seq, Arc::from(seq.to_string()));
        }
        ring
    }

    fn texts(items: &[Arc<str>]) -> Vec<&str> {
        items.iter().map(|item| &**item).collect()
    }

    #[test]
    fn a_reader_resumes_after_what_it_has() {
        let ring = filled(5, 10);
        assert_eq!((ring.oldest(), ring.newest()), (1, 5));
        let after: Vec<u64> = ring.after(3).unwrap().iter().map(|(s, _)| *s).collect();
        assert_eq!(after, vec![4, 5]);
        assert_eq!(ring.after(0).unwrap().len(), 5);
        assert!(ring.after(5).unwrap().is_empty());
        assert_eq!(texts(&ring.tail(2)), ["4", "5"]);
        assert_eq!(texts(&ring.before(4, 2)), ["2", "3"]);
        assert_eq!(texts(&ring.before(2, 10)), ["1"]);
    }

    #[test]
    fn an_evicted_stretch_is_a_gap() {
        let ring = filled(15, 10);
        assert_eq!(ring.oldest(), 6);
        assert_eq!(ring.after(4), Err(Gap));
        assert_eq!(ring.after(5).unwrap().len(), 10);
        assert_eq!(ring.after(0), Err(Gap));
        assert_eq!(Ring::new(4).after(0), Ok(Vec::new()));
    }
}

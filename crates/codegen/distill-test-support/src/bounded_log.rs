//! Arrival ordered entries that evict oldest first past a cap, so a double's memory is bounded.

use std::collections::VecDeque;

pub(crate) struct BoundedLog<T> {
    entries: VecDeque<T>,
    cap: usize,
    evicted: usize,
}

impl<T> BoundedLog<T> {
    pub(crate) fn new(cap: usize) -> BoundedLog<T> {
        BoundedLog {
            entries: VecDeque::new(),
            cap,
            evicted: 0,
        }
    }

    pub(crate) fn push(&mut self, entry: T) {
        self.entries.push_back(entry);
        self.evict_overflow();
    }

    pub(crate) fn extend(&mut self, entries: impl IntoIterator<Item = T>) {
        self.entries.extend(entries);
        self.evict_overflow();
    }

    fn evict_overflow(&mut self) {
        let overflow = self.entries.len().saturating_sub(self.cap);
        self.entries.drain(..overflow);
        self.evicted += overflow;
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = &T> {
        self.entries.iter()
    }

    /// Every entry ever pushed, retained or evicted, so a baseline taken before a wait still
    /// counts arrivals once the log is full.
    pub(crate) fn total(&self) -> usize {
        self.entries.len() + self.evicted
    }

    /// The entries pushed after `total()` read `baseline`, as many of them as are still retained.
    pub(crate) fn arrived_since(&self, baseline: usize) -> Vec<T>
    where
        T: Clone,
    {
        let fresh = self.total().saturating_sub(baseline);
        let skip = self.entries.len().saturating_sub(fresh);
        self.entries.iter().skip(skip).cloned().collect()
    }

    pub(crate) fn evicted(&self) -> usize {
        self.evicted
    }
}

impl<T: Clone> BoundedLog<T> {
    pub(crate) fn to_vec(&self) -> Vec<T> {
        self.entries.iter().cloned().collect()
    }
}

#[cfg(test)]
#[path = "bounded_log_tests.rs"]
mod tests;

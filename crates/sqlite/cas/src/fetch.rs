//! Fetch sizing: how much to ask the store for when a read misses the cache.
//!
//! A database is read in blocks of a fixed size. When SQLite needs blocks the
//! cache lacks, a [`FetchPolicy`] chooses the run of blocks to fetch in one
//! request: at least the missing ones, and perhaps more around or after them.
//! Policies only choose what to fetch, so they cannot change an answer, only
//! the number and size of requests.

use std::{
    collections::VecDeque,
    ops::Range,
    sync::{Arc, Mutex, PoisonError},
};

/// A read that needs blocks the cache lacks.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct Miss {
    /// The missing blocks the read needs, by index: a non-empty run.
    pub need: Range<u64>,
    /// The number of blocks in the database; `need` ends at or before it.
    pub blocks: u64,
    /// The size of a block in bytes; at least 1.
    pub block_size: u64,
}

impl Miss {
    /// A miss of the blocks `need` in a database of `blocks` blocks of `block_size` bytes.
    pub fn new(need: Range<u64>, blocks: u64, block_size: u64) -> Self {
        Self {
            need,
            blocks,
            block_size,
        }
    }

    /// The number of whole blocks in `bytes`, at least one.
    fn blocks_in(&self, bytes: u64) -> u64 {
        (bytes / self.block_size.max(1)).max(1)
    }
}

/// Chooses the blocks to fetch in one request when a read misses the cache.
///
/// One policy serves every connection to a database, from any thread.
pub trait FetchPolicy: Send + Sync {
    /// The run of blocks to fetch for `miss`.
    ///
    /// The caller widens the answer to hold `miss.need` and cuts it to the
    /// database, so any answer is safe; blocks outside `miss.need` are a hint.
    fn plan(&self, miss: &Miss) -> Range<u64>;
}

impl<T: FetchPolicy + ?Sized> FetchPolicy for Arc<T> {
    fn plan(&self, miss: &Miss) -> Range<u64> {
        (**self).plan(miss)
    }
}

impl<T: FetchPolicy + ?Sized> FetchPolicy for Box<T> {
    fn plan(&self, miss: &Miss) -> Range<u64> {
        (**self).plan(miss)
    }
}

/// Fetches only the blocks a read needs.
///
/// With page-sized blocks this is one request per page SQLite reads: the
/// fewest bytes, and the most requests.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Exact;

impl FetchPolicy for Exact {
    fn plan(&self, miss: &Miss) -> Range<u64> {
        miss.need.clone()
    }
}

/// Fetches whole windows of a fixed size, aligned to multiples of that size.
///
/// A read anywhere in a window fetches all of it, so nearby pages cost one
/// request, and every read costs at least a window of bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Aligned {
    bytes: u64,
}

impl Aligned {
    /// Windows of `bytes` bytes, rounded down to whole blocks and up to one block.
    pub fn new(bytes: u64) -> Self {
        Self { bytes }
    }
}

impl FetchPolicy for Aligned {
    fn plan(&self, miss: &Miss) -> Range<u64> {
        let window = miss.blocks_in(self.bytes);
        let start = miss.need.start / window * window;
        let end = miss.need.end.div_ceil(window).saturating_mul(window);
        start..end
    }
}

/// Fetches ahead of sequential reads, in windows that double.
///
/// - A read that starts a new place in the database fetches only the blocks
///   it needs, as [`Exact`] does, so scattered lookups stay small.
/// - A read that continues where an earlier fetch ended, or skips at most
///   that fetch's length past it, fetches twice as much as that fetch did,
///   up to the largest window. A scan soon reads in large requests.
/// - Several interleaved scans are followed at once, such as two connections
///   scanning, or a join walking an index and a table.
#[derive(Debug)]
pub struct ReadAhead {
    /// The largest window, in bytes.
    max: u64,
    /// The most scans followed at once.
    limit: usize,
    /// The scans being followed, least recently continued first.
    heads: Mutex<VecDeque<Head>>,
}

/// A scan: where its last fetch ended and how many blocks it fetched.
#[derive(Clone, Copy, Debug)]
struct Head {
    next: u64,
    window: u64,
}

impl ReadAhead {
    /// The scans followed at once by [`ReadAhead::new`].
    pub const SCANS: usize = 8;

    /// Windows that grow to `max_bytes`, following up to [`ReadAhead::SCANS`] scans.
    pub fn new(max_bytes: u64) -> Self {
        Self::with_scans(max_bytes, Self::SCANS)
    }

    /// Windows that grow to `max_bytes`, following up to `scans` scans, at least one.
    pub fn with_scans(max_bytes: u64, scans: usize) -> Self {
        Self {
            max: max_bytes,
            limit: scans.max(1),
            heads: Mutex::default(),
        }
    }
}

impl Default for ReadAhead {
    /// Windows that grow to 1 MiB.
    fn default() -> Self {
        Self::new(1 << 20)
    }
}

impl FetchPolicy for ReadAhead {
    fn plan(&self, miss: &Miss) -> Range<u64> {
        let Range { start, end } = miss.need;
        let need = end.saturating_sub(start);
        let mut heads = self.heads.lock().unwrap_or_else(PoisonError::into_inner);
        let continued = heads
            .iter()
            .position(|head| head.next <= start && start - head.next <= head.window);
        let window = match continued.and_then(|at| heads.remove(at)) {
            Some(head) => head.window.saturating_mul(2).min(miss.blocks_in(self.max)),
            None => need,
        };
        let window = window.max(need);
        let end = start.saturating_add(window).min(miss.blocks).max(end);
        if heads.len() >= self.limit {
            heads.pop_front();
        }
        heads.push_back(Head { next: end, window });
        start..end
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn miss(need: Range<u64>) -> Miss {
        Miss::new(need, 10_000, 4096)
    }

    #[test]
    fn exact_fetches_what_is_missing() {
        assert_eq!(Exact.plan(&miss(7..9)), 7..9);
    }

    #[test]
    fn aligned_fetches_whole_windows() {
        let policy = Aligned::new(64 << 10);
        assert_eq!(policy.plan(&miss(0..1)), 0..16);
        assert_eq!(policy.plan(&miss(17..18)), 16..32);
        assert_eq!(policy.plan(&miss(15..17)), 0..32);
        // A window smaller than a block is one block; sizes round down to blocks.
        assert_eq!(Aligned::new(1).plan(&miss(5..6)), 5..6);
        assert_eq!(Aligned::new(3 * 4096 + 5).plan(&miss(4..5)), 3..6);
        // Sizes that overflow are cut by the caller, not wrapped.
        let huge = Aligned::new(u64::MAX).plan(&Miss::new(3..4, u64::MAX, 1));
        assert_eq!(huge, 0..u64::MAX);
        assert_eq!(Aligned::new(8).plan(&Miss::new(3..4, 9, 0)), 0..8);
    }

    #[test]
    fn read_ahead_doubles_along_a_scan_and_stops_at_its_largest_window() {
        let policy = ReadAhead::new(16 * 4096);
        let mut next = 100;
        let mut windows = Vec::new();
        for _ in 0..7 {
            let plan = policy.plan(&miss(next..next + 1));
            assert_eq!(plan.start, next);
            windows.push(plan.end - plan.start);
            next = plan.end;
        }
        assert_eq!(windows, [1, 2, 4, 8, 16, 16, 16]);
        // The end of the database cuts a window, and the scan ends there.
        let end = Miss::new(9_990..9_991, 10_000, 4096);
        assert_eq!(policy.plan(&end), 9_990..9_991);
        assert_eq!(
            policy.plan(&Miss::new(9_991..9_992, 10_000, 4096)),
            9_991..9_993
        );
        assert_eq!(
            policy.plan(&Miss::new(9_993..9_994, 10_000, 4096)),
            9_993..9_997
        );
        assert_eq!(
            policy.plan(&Miss::new(9_997..9_998, 10_000, 4096)),
            9_997..10_000
        );
    }

    #[test]
    fn read_ahead_keeps_scattered_reads_small() {
        let policy = ReadAhead::default();
        for start in [5_000, 20, 7_300, 1_000, 9_999, 3] {
            assert_eq!(policy.plan(&miss(start..start + 1)), start..start + 1);
        }
        // A read before a scan's end, or far past it, is not that scan.
        let policy = ReadAhead::default();
        assert_eq!(policy.plan(&miss(50..52)), 50..52);
        assert_eq!(policy.plan(&miss(49..50)), 49..50);
        assert_eq!(policy.plan(&miss(60..61)), 60..61);
    }

    #[test]
    fn read_ahead_follows_scans_that_skip_or_interleave() {
        let policy = ReadAhead::default();
        // Two scans take turns.
        assert_eq!(policy.plan(&miss(100..101)), 100..101);
        assert_eq!(policy.plan(&miss(900..901)), 900..901);
        assert_eq!(policy.plan(&miss(101..102)), 101..103);
        assert_eq!(policy.plan(&miss(901..902)), 901..903);
        assert_eq!(policy.plan(&miss(103..104)), 103..107);
        // A skip of no more than the last window continues the scan.
        assert_eq!(policy.plan(&miss(905..906)), 905..909);
        // A read wider than the window is fetched whole.
        assert_eq!(policy.plan(&miss(107..140)), 107..140);
    }

    #[test]
    fn read_ahead_forgets_the_oldest_scan() {
        let policy = ReadAhead::with_scans(1 << 20, 2);
        assert_eq!(policy.plan(&miss(100..101)), 100..101);
        assert_eq!(policy.plan(&miss(200..201)), 200..201);
        assert_eq!(policy.plan(&miss(300..301)), 300..301);
        // The scan at 100 was forgotten; the one at 200 was not.
        assert_eq!(policy.plan(&miss(101..102)), 101..102);
        assert_eq!(policy.plan(&miss(301..302)), 301..303);
        assert_eq!(ReadAhead::with_scans(1, 0).limit, 1);
    }
}

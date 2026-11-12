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
    /// The most blocks one request fetches, and never fewer than `need`.
    ///
    /// The cache sets it: see [`BlockCache::capacity`]. A longer plan is cut
    /// to this many blocks, so a policy that remembers what it fetched plans
    /// within it.
    ///
    /// [`BlockCache::capacity`]: crate::cache::BlockCache::capacity
    pub most: u64,
}

impl Miss {
    /// A miss of the blocks `need` in a database of `blocks` blocks of
    /// `block_size` bytes, with no limit on what one request fetches.
    pub fn new(need: Range<u64>, blocks: u64, block_size: u64) -> Self {
        Self {
            need,
            blocks,
            block_size,
            most: u64::MAX,
        }
    }

    /// The same miss, when one request fetches at most `blocks` blocks.
    pub fn at_most(mut self, blocks: u64) -> Self {
        self.most = blocks;
        self
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
    /// database and to `miss.most` blocks, so any answer is safe; blocks
    /// outside `miss.need` are a hint.
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

/// Fetches ahead of sequential reads, in windows that grow.
///
/// - A read at a new place fetches a first window from where it starts:
///   64 KiB unless [`ReadAhead::first`] says otherwise, and never less than
///   the read needs. Scattered lookups cost that much each.
/// - A read near an earlier fetch continues it: the next fetch starts where
///   that one ended and is four times as long, up to the largest window. A
///   scan soon reads in large requests.
/// - Near is within that fetch's length of either end of it, so scans are
///   followed towards the end of the database and towards its start. The
///   blocks between are fetched too: SQLite scans a table by jumping ahead
///   to a page of its tree and coming back.
/// - Several interleaved scans are followed at once, such as two connections
///   scanning, or a join walking an index and a table.
///
/// The sizes to choose depend on the store. A request costs its round trip,
/// which is the time of some number of bytes at the store's speed: 1.5 MB
/// for 15 ms at 100 MB/s. A first window that is a small part of that costs
/// little, and a largest window several times that leaves little to gain.
///
/// ```
/// use covalence_sqlite_cas::fetch::{FetchPolicy, Miss, ReadAhead};
///
/// // A database of 10,000 blocks of 4 KiB, read from block 100 on.
/// let miss = |block: u64| Miss::new(block..block + 1, 10_000, 4096);
/// let policy = ReadAhead::new(1 << 20).first(16 << 10);
/// assert_eq!(policy.plan(&miss(100)), 100..104);
/// assert_eq!(policy.plan(&miss(104)), 104..120);
/// assert_eq!(policy.plan(&miss(120)), 120..184);
/// assert_eq!(policy.plan(&miss(184)), 184..440);
/// assert_eq!(policy.plan(&miss(440)), 440..696);
/// // A read somewhere else starts small again.
/// assert_eq!(policy.plan(&miss(5_000)), 5_000..5_004);
/// ```
#[derive(Debug)]
pub struct ReadAhead {
    /// The first window, in bytes.
    first: u64,
    /// The largest window, in bytes.
    max: u64,
    /// The most scans followed at once.
    limit: usize,
    /// The last fetch of each scan followed, least recently continued first.
    followed: Mutex<VecDeque<Range<u64>>>,
}

impl ReadAhead {
    /// The first window unless [`ReadAhead::first`] sets another: 64 KiB.
    pub const FIRST: u64 = 64 << 10;

    /// The largest window of [`ReadAhead::default`]: 4 MiB.
    pub const MAX: u64 = 4 << 20;

    /// The scans followed at once unless [`ReadAhead::scans`] sets another number.
    pub const SCANS: usize = 8;

    /// What a window is multiplied by each time its scan continues.
    const GROWTH: u64 = 4;

    /// Windows that grow from [`ReadAhead::FIRST`] bytes to `max_bytes`.
    ///
    /// Sizes are rounded down to whole blocks and up to one block, and no
    /// window is larger than `max_bytes` unless a read needs more.
    pub fn new(max_bytes: u64) -> Self {
        Self {
            first: Self::FIRST,
            max: max_bytes,
            limit: Self::SCANS,
            followed: Mutex::default(),
        }
    }

    /// The same policy with a first window of `bytes` bytes.
    ///
    /// With 0 a read at a new place fetches only the blocks it needs, as
    /// [`Exact`] does.
    pub fn first(mut self, bytes: u64) -> Self {
        self.first = bytes;
        self
    }

    /// The same policy following up to `scans` scans at once, at least one.
    pub fn scans(mut self, scans: usize) -> Self {
        self.limit = scans.max(1);
        self
    }
}

impl Default for ReadAhead {
    /// Windows that grow from 64 KiB to 4 MiB.
    fn default() -> Self {
        Self::new(Self::MAX)
    }
}

impl FetchPolicy for ReadAhead {
    fn plan(&self, miss: &Miss) -> Range<u64> {
        let Range { start, end } = miss.need;
        let most = miss.blocks_in(self.max).min(miss.most);
        let len = |fetch: &Range<u64>| fetch.end.saturating_sub(fetch.start);
        let grown = |last: &Range<u64>| len(last).saturating_mul(Self::GROWTH).min(most);
        let mut followed = self.followed.lock().unwrap_or_else(PoisonError::into_inner);
        // The scan this read continues: one whose last fetch holds the read
        // or is within its own length of it, and the longest such fetch.
        let near = |last: &Range<u64>| {
            let reach = len(last);
            start <= last.end.saturating_add(reach) && last.start <= end.saturating_add(reach)
        };
        let nearest = followed.iter().enumerate().filter(|(_, last)| near(last));
        let continued = nearest.max_by_key(|(_, last)| len(last)).map(|(at, _)| at);
        let fetch = match continued.and_then(|at| followed.remove(at)) {
            // On from where the last fetch ended.
            Some(last) if last.end <= start => last.end..last.end.saturating_add(grown(&last)),
            // Blocks of the last fetch that the cache did not keep: as many again.
            Some(last) if last.start <= start => start..start.saturating_add(len(&last).min(most)),
            // Back from where the last fetch started.
            Some(last) => last.start.saturating_sub(grown(&last))..last.start,
            // A new place.
            None => start..start.saturating_add(miss.blocks_in(self.first).min(most)),
        };
        let fetch = fetch.start.min(start)..fetch.end.min(miss.blocks).max(end);
        if followed.len() >= self.limit {
            followed.pop_front();
        }
        followed.push_back(fetch.clone());
        fetch
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

    /// A policy whose scans start with one block.
    fn from_one_block() -> ReadAhead {
        ReadAhead::default().first(0)
    }

    #[test]
    fn read_ahead_grows_fourfold_along_a_scan_up_to_its_largest_window() {
        let policy = ReadAhead::new(64 * 4096).first(0);
        let mut next = 100;
        let mut windows = Vec::new();
        for _ in 0..6 {
            let plan = policy.plan(&miss(next..next + 1));
            assert_eq!(plan.start, next);
            windows.push(plan.end - plan.start);
            next = plan.end;
        }
        assert_eq!(windows, [1, 4, 16, 64, 64, 64]);
        // The end of the database cuts a window, and the scan ends there.
        let policy = from_one_block();
        assert_eq!(policy.plan(&miss(9_990..9_991)), 9_990..9_991);
        assert_eq!(policy.plan(&miss(9_991..9_992)), 9_991..9_995);
        assert_eq!(policy.plan(&miss(9_995..9_996)), 9_995..10_000);
    }

    #[test]
    fn read_ahead_starts_with_its_first_window() {
        // 64 KiB unless set, which is 16 of these blocks, and 4 MiB at most.
        let policy = ReadAhead::default();
        assert_eq!(policy.plan(&miss(100..101)), 100..116);
        assert_eq!(policy.plan(&miss(116..117)), 116..180);
        assert_eq!(policy.plan(&miss(180..181)), 180..436);
        assert_eq!(policy.plan(&miss(436..437)), 436..1_460);
        assert_eq!(policy.plan(&miss(1_460..1_461)), 1_460..2_484);
        // A read that needs more than the first window is fetched whole.
        let policy = ReadAhead::new(8 * 4096).first(2 * 4096);
        assert_eq!(policy.plan(&miss(100..101)), 100..102);
        assert_eq!(policy.plan(&miss(500..505)), 500..505);
        // A first window is no larger than the largest, and at least a block.
        let policy = ReadAhead::new(8 * 4096).first(u64::MAX);
        assert_eq!(policy.plan(&miss(100..101)), 100..108);
        let policy = ReadAhead::new(0).first(0);
        assert_eq!(policy.plan(&miss(100..101)), 100..101);
        assert_eq!(policy.plan(&miss(101..102)), 101..102);
    }

    #[test]
    fn read_ahead_keeps_scattered_reads_small() {
        let policy = from_one_block();
        for start in [5_000, 20, 7_300, 1_000, 9_999, 3] {
            assert_eq!(policy.plan(&miss(start..start + 1)), start..start + 1);
        }
        // A read further from a fetch than the fetch is long is a new place.
        let policy = from_one_block();
        assert_eq!(policy.plan(&miss(50..52)), 50..52);
        assert_eq!(policy.plan(&miss(55..56)), 55..56);
        assert_eq!(policy.plan(&miss(46..47)), 46..47);
    }

    #[test]
    fn read_ahead_follows_scans_towards_the_start() {
        let policy = ReadAhead::new(64 * 4096).first(0);
        let mut next = 400;
        let mut windows = Vec::new();
        for _ in 0..6 {
            let plan = policy.plan(&miss(next - 1..next));
            assert_eq!(plan.end, next);
            windows.push(plan.end - plan.start);
            next = plan.start;
        }
        assert_eq!(windows, [1, 4, 16, 64, 64, 64]);
        // The start of the database cuts a window.
        let policy = from_one_block();
        assert_eq!(policy.plan(&miss(9..10)), 9..10);
        assert_eq!(policy.plan(&miss(8..9)), 5..9);
        assert_eq!(policy.plan(&miss(4..5)), 0..5);
    }

    #[test]
    fn read_ahead_fetches_what_a_scan_skips_and_what_was_not_kept() {
        let policy = from_one_block();
        assert_eq!(policy.plan(&miss(100..101)), 100..101);
        assert_eq!(policy.plan(&miss(101..102)), 101..105);
        // A jump ahead by no more than the last fetch: the blocks between
        // come with it, for when the scan returns to them.
        assert_eq!(policy.plan(&miss(108..109)), 105..121);
        // So does a jump back.
        let policy = from_one_block();
        assert_eq!(policy.plan(&miss(500..504)), 500..504);
        assert_eq!(policy.plan(&miss(495..496)), 484..500);
        // A block of the last fetch is missing, as when a cache holds less
        // than a window: the same number of blocks again, from there.
        assert_eq!(policy.plan(&miss(490..491)), 490..506);
        assert_eq!(policy.plan(&miss(506..507)), 506..570);
        // A read across the end of the last fetch is one of those too.
        assert_eq!(policy.plan(&miss(568..572)), 568..632);
        // A read across its start goes back from there, and is held whole.
        assert_eq!(policy.plan(&miss(560..570)), 312..570);
    }

    #[test]
    fn read_ahead_follows_scans_that_skip_or_interleave() {
        let policy = from_one_block();
        // Two scans take turns.
        assert_eq!(policy.plan(&miss(100..101)), 100..101);
        assert_eq!(policy.plan(&miss(900..901)), 900..901);
        assert_eq!(policy.plan(&miss(101..102)), 101..105);
        assert_eq!(policy.plan(&miss(901..902)), 901..905);
        assert_eq!(policy.plan(&miss(105..106)), 105..121);
        // A skip of no more than the last window continues the scan.
        assert_eq!(policy.plan(&miss(907..908)), 905..921);
        // A read wider than the window is fetched whole.
        assert_eq!(policy.plan(&miss(121..400)), 121..400);
    }

    #[test]
    fn read_ahead_continues_the_longest_scan_near_a_read() {
        let policy = from_one_block();
        // A scan, and one block read a little ahead of it.
        assert_eq!(policy.plan(&miss(100..101)), 100..101);
        assert_eq!(policy.plan(&miss(110..111)), 110..111);
        assert_eq!(policy.plan(&miss(101..102)), 101..105);
        // A read near both is the scan's.
        assert_eq!(policy.plan(&miss(109..110)), 105..121);
    }

    #[test]
    fn read_ahead_forgets_the_oldest_scan() {
        let policy = from_one_block().scans(2);
        assert_eq!(policy.plan(&miss(100..101)), 100..101);
        assert_eq!(policy.plan(&miss(200..201)), 200..201);
        assert_eq!(policy.plan(&miss(300..301)), 300..301);
        // The scan at 100 was forgotten; the one at 300 was not.
        assert_eq!(policy.plan(&miss(101..102)), 101..102);
        assert_eq!(policy.plan(&miss(301..302)), 301..305);
        assert_eq!(ReadAhead::default().scans(0).limit, 1);
    }

    #[test]
    fn read_ahead_plans_within_the_most_blocks_of_a_request() {
        let miss = |need: Range<u64>| miss(need).at_most(8);
        let policy = ReadAhead::default();
        // The first window is 16 blocks, and a request at most 8.
        assert_eq!(policy.plan(&miss(100..101)), 100..108);
        assert_eq!(policy.plan(&miss(108..109)), 108..116);
        // A read that skips ahead is continued from the fetch that was made,
        // not from a longer one that was only planned.
        assert_eq!(policy.plan(&miss(120..121)), 116..124);
        assert_eq!(policy.plan(&miss(117..118)), 117..125);
        // Towards the start as well.
        assert_eq!(policy.plan(&miss(5_000..5_001)), 5_000..5_008);
        assert_eq!(policy.plan(&miss(4_999..5_000)), 4_992..5_000);
        // A read that needs more is planned whole.
        assert_eq!(policy.plan(&miss(7_000..7_020)), 7_000..7_020);
    }

    #[test]
    fn read_ahead_takes_any_miss() {
        // Sizes and positions at the limits neither wrap nor panic.
        let policy = ReadAhead::new(u64::MAX).first(u64::MAX);
        let all = Miss::new(u64::MAX - 1..u64::MAX, u64::MAX, 1);
        assert_eq!(policy.plan(&all), u64::MAX - 1..u64::MAX);
        assert_eq!(policy.plan(&Miss::new(0..1, u64::MAX, 1)), 0..u64::MAX);
        assert_eq!(policy.plan(&Miss::new(5..6, u64::MAX, 0)), 5..u64::MAX);
        // A miss that breaks its own rules gets an answer the caller can cut.
        let policy = ReadAhead::default();
        let empty = Range { start: 7, end: 7 };
        let inverted = Range { start: 9, end: 3 };
        assert_eq!(policy.plan(&Miss::new(empty, 10, 4096)), 7..10);
        assert_eq!(policy.plan(&Miss::new(inverted, 10, 4096)), 9..10);
        assert_eq!(policy.plan(&Miss::new(20..21, 10, 4096)), 20..21);
    }
}

//! Block caches: where fetched blocks are kept for later reads.
//!
//! SQLite keeps the pages it reads in a page cache of its own, one for each
//! connection (2 MB unless `PRAGMA cache_size` says otherwise). A block
//! cache sits below those: it holds the blocks that were fetched ahead of
//! need until SQLite asks for them, and it serves a page to every connection
//! whose own cache lacks it. A page that SQLite holds is not asked for again
//! soon, so one connection needs a block cache of only a few fetches: one
//! request fetches at most a quarter of a cache that is smaller than the
//! database. A larger cache saves the requests of other and later
//! connections.
//!
//! - A [`CachePolicy`] builds the cache of a database when it is opened:
//!   [`NoCache`], [`MemoryCache`], [`FileCache`], or any function from a
//!   [`Layout`] to a [`BlockCache`].
//! - [`ClockCache`] is the cache behind the last two. It keeps blocks in an
//!   [`Arena`], so a cache over other storage, such as a memory-mapped file,
//!   needs only an [`Arena`].

use std::{
    collections::HashMap,
    fs::File,
    io,
    path::PathBuf,
    sync::{
        PoisonError, RwLock,
        atomic::{AtomicBool, Ordering},
    },
};

use crate::pio;

/// The shape of a database read in blocks: its size and its block size.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Layout {
    size: u64,
    block_size: u64,
}

impl Layout {
    /// A database of `size` bytes read in blocks of `block_size` bytes, at least 1.
    pub fn new(size: u64, block_size: u64) -> Self {
        Self {
            size,
            block_size: block_size.max(1),
        }
    }

    /// The size of the database in bytes.
    pub fn size(&self) -> u64 {
        self.size
    }

    /// The size of a block in bytes: every block but the last has this many.
    pub fn block_size(&self) -> u64 {
        self.block_size
    }

    /// The number of blocks.
    pub fn blocks(&self) -> u64 {
        self.size.div_ceil(self.block_size)
    }

    /// The number of bytes in `block`: fewer than a block's size only in the
    /// last block, and 0 past it.
    pub fn block_len(&self, block: u64) -> u64 {
        let start = block.saturating_mul(self.block_size);
        self.size.saturating_sub(start).min(self.block_size)
    }

    /// The number of bytes in the largest block: a block's size, or all of a
    /// database that is smaller than one block.
    pub fn largest_block(&self) -> u64 {
        self.size.min(self.block_size)
    }
}

/// Keeps blocks of one database.
///
/// What a cache returns is trusted and not checked again. Blocks are offered
/// to it only after they pass the database's verification, so a cache that
/// keeps blocks where others can change them must protect them itself.
pub trait BlockCache: Send + Sync {
    /// Fill `buf` with the bytes of `block` from `offset`, if the cache holds
    /// the block.
    ///
    /// Returns `false` if it does not hold the block or cannot fill `buf`;
    /// `buf` may then hold anything.
    fn read(&self, block: u64, offset: usize, buf: &mut [u8]) -> bool;

    /// Offer all the bytes of `block` to the cache, which may keep them.
    fn insert(&self, block: u64, bytes: &[u8]);

    /// The most blocks the cache holds at once, or `None` if it has no such
    /// limit or cannot say.
    ///
    /// If this is fewer blocks than the database has, one request to the
    /// store fetches no more than a quarter of it, unless a single read needs
    /// more. Blocks fetched ahead of a few scans then fit in the cache
    /// together; larger fetches would push each other out before they were
    /// read. Fetch policies see that limit as
    /// [`Miss::most`](crate::fetch::Miss::most).
    fn capacity(&self) -> Option<u64> {
        None
    }
}

/// Builds the block cache of a database when it is opened.
///
/// Every function `Fn(Layout) -> io::Result<Box<dyn BlockCache>>` that is
/// `Send` and `Sync` is a `CachePolicy`.
pub trait CachePolicy: Send + Sync {
    /// A new, empty cache for a database of this layout.
    fn build(&self, layout: Layout) -> io::Result<Box<dyn BlockCache>>;
}

impl<F> CachePolicy for F
where
    F: Fn(Layout) -> io::Result<Box<dyn BlockCache>> + Send + Sync,
{
    fn build(&self, layout: Layout) -> io::Result<Box<dyn BlockCache>> {
        self(layout)
    }
}

/// Keeps nothing: every read SQLite makes is fetched.
///
/// Its [`capacity`](BlockCache::capacity) is 0, so every fetch policy fetches
/// only the blocks that are read, as [`Exact`](crate::fetch::Exact) does.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct NoCache;

impl BlockCache for NoCache {
    fn read(&self, _block: u64, _offset: usize, _buf: &mut [u8]) -> bool {
        false
    }
    fn insert(&self, _block: u64, _bytes: &[u8]) {}
    fn capacity(&self) -> Option<u64> {
        Some(0)
    }
}

impl CachePolicy for NoCache {
    fn build(&self, _layout: Layout) -> io::Result<Box<dyn BlockCache>> {
        Ok(Box::new(NoCache))
    }
}

/// Keeps blocks in memory, up to a number of bytes.
///
/// Memory is taken as blocks arrive, not when the database is opened. See
/// [`ClockCache`] for what is dropped when the cache is full.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MemoryCache {
    bytes: u64,
}

impl MemoryCache {
    /// A cache of at most `bytes` bytes of blocks. Less than the database's
    /// largest block keeps nothing.
    pub fn new(bytes: u64) -> Self {
        Self { bytes }
    }
}

impl CachePolicy for MemoryCache {
    fn build(&self, layout: Layout) -> io::Result<Box<dyn BlockCache>> {
        let block = usize::try_from(layout.largest_block()).map_err(io::Error::other)?;
        let arena = Heap {
            block,
            slots: Vec::new(),
        };
        Ok(Box::new(ClockCache::new(
            arena,
            layout,
            slots(self.bytes, layout),
        )))
    }
}

/// Keeps blocks in a temporary file, up to a number of bytes.
///
/// The file has no name: only this process can read it, and the system frees
/// it when the database and its connections are gone. It grows as blocks
/// arrive. See [`ClockCache`] for what is dropped when the cache is full.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileCache {
    bytes: u64,
    dir: Option<PathBuf>,
}

impl FileCache {
    /// A cache of at most `bytes` bytes of blocks, in the system's temporary
    /// directory. Less than the database's largest block keeps nothing.
    pub fn new(bytes: u64) -> Self {
        Self { bytes, dir: None }
    }

    /// The same cache with its file in the directory `dir`.
    pub fn in_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.dir = Some(dir.into());
        self
    }
}

impl CachePolicy for FileCache {
    fn build(&self, layout: Layout) -> io::Result<Box<dyn BlockCache>> {
        let file = match &self.dir {
            Some(dir) => tempfile::tempfile_in(dir)?,
            None => tempfile::tempfile()?,
        };
        Ok(Box::new(ClockCache::new(
            file,
            layout,
            slots(self.bytes, layout),
        )))
    }
}

/// The number of blocks `bytes` bytes hold: no more than the database has.
fn slots(bytes: u64, layout: Layout) -> usize {
    let slots = (bytes / layout.largest_block().max(1)).min(layout.blocks());
    usize::try_from(slots).unwrap_or(usize::MAX)
}

/// Storage for the blocks of a [`ClockCache`]: bytes read and written by position.
///
/// The cache reads from several threads at once, and writes from one thread
/// while no other reads or writes.
pub trait Arena: Send + Sync {
    /// Fill `buf` with the bytes at `at`, all of which were written before.
    fn read(&self, at: u64, buf: &mut [u8]) -> io::Result<()>;

    /// Write `bytes` at `at`.
    fn write(&mut self, at: u64, bytes: &[u8]) -> io::Result<()>;
}

impl Arena for File {
    fn read(&self, at: u64, buf: &mut [u8]) -> io::Result<()> {
        pio::read_exact_at(self, at, buf)
    }
    fn write(&mut self, at: u64, bytes: &[u8]) -> io::Result<()> {
        pio::write_all_at(self, at, bytes)
    }
}

/// Blocks in memory, each allocated when first written.
struct Heap {
    /// The size of a block.
    block: usize,
    /// One buffer of `block` bytes for each slot written so far.
    slots: Vec<Box<[u8]>>,
}

impl Heap {
    /// The slot and the offset in it of the position `at`.
    fn locate(&self, at: u64) -> io::Result<(usize, usize)> {
        let at = usize::try_from(at).map_err(io::Error::other)?;
        let block = self.block.max(1);
        Ok((at / block, at % block))
    }
}

impl Arena for Heap {
    fn read(&self, at: u64, buf: &mut [u8]) -> io::Result<()> {
        let (slot, offset) = self.locate(at)?;
        let bytes = self
            .slots
            .get(slot)
            .and_then(|slot| slot.get(offset..)?.get(..buf.len()))
            .ok_or(io::ErrorKind::UnexpectedEof)?;
        buf.copy_from_slice(bytes);
        Ok(())
    }

    fn write(&mut self, at: u64, bytes: &[u8]) -> io::Result<()> {
        let (slot, offset) = self.locate(at)?;
        // The cache fills slots in order, so this adds at most one.
        while self.slots.len() <= slot {
            self.slots.push(vec![0; self.block].into_boxed_slice());
        }
        self.slots[slot]
            .get_mut(offset..)
            .and_then(|slot| slot.get_mut(..bytes.len()))
            .ok_or(io::ErrorKind::WriteZero)?
            .copy_from_slice(bytes);
        Ok(())
    }
}

/// A cache of a fixed number of blocks in an [`Arena`].
///
/// - The block in slot `i` lives at `i * layout.largest_block()` in the
///   arena, so the arena holds up to `capacity * layout.largest_block()`
///   bytes.
/// - When full, a new block replaces the block that arrived first. A block
///   that was read since the cache last passed it is passed once more
///   instead (the
///   [clock algorithm](https://en.wikipedia.org/wiki/Page_replacement_algorithm#Clock)).
///   Blocks fetched ahead of a scan therefore stay until older blocks have
///   gone, and a block that is read time and again stays longest.
/// - A read or write the arena fails is a miss, or a block not kept.
/// - Reads share a lock and run together; inserting a block excludes them.
pub struct ClockCache<A> {
    layout: Layout,
    capacity: usize,
    state: RwLock<State<A>>,
}

/// What is in each slot of a [`ClockCache`].
struct State<A> {
    arena: A,
    /// The slot of each block held.
    slot_of: HashMap<u64, usize>,
    /// The block in each slot used so far, or `EMPTY`.
    block_of: Vec<u64>,
    /// Whether the block in each slot arrived or was read since the hand
    /// last passed it.
    marked: Vec<AtomicBool>,
    /// The next slot to consider dropping.
    hand: usize,
}

/// The block of a slot whose write failed.
const EMPTY: u64 = u64::MAX;

impl<A: Arena> ClockCache<A> {
    /// A cache of up to `capacity` blocks of a database of this `layout`, in `arena`.
    pub fn new(arena: A, layout: Layout, capacity: usize) -> Self {
        Self {
            layout,
            capacity,
            state: RwLock::new(State {
                arena,
                slot_of: HashMap::new(),
                block_of: Vec::new(),
                marked: Vec::new(),
                hand: 0,
            }),
        }
    }

    /// The position of `slot` in the arena.
    fn position(&self, slot: usize) -> u64 {
        (slot as u64).saturating_mul(self.layout.largest_block())
    }
}

impl<A: Arena> BlockCache for ClockCache<A> {
    fn read(&self, block: u64, offset: usize, buf: &mut [u8]) -> bool {
        let end = (offset as u64).saturating_add(buf.len() as u64);
        if end > self.layout.block_len(block) {
            return false;
        }
        let state = self.state.read().unwrap_or_else(PoisonError::into_inner);
        let Some(&slot) = state.slot_of.get(&block) else {
            return false;
        };
        let at = self.position(slot).saturating_add(offset as u64);
        if state.arena.read(at, buf).is_err() {
            return false;
        }
        state.marked[slot].store(true, Ordering::Relaxed);
        true
    }

    fn insert(&self, block: u64, bytes: &[u8]) {
        if self.capacity == 0 || bytes.is_empty() {
            return;
        }
        if bytes.len() as u64 != self.layout.block_len(block) {
            return;
        }
        let mut state = self.state.write().unwrap_or_else(PoisonError::into_inner);
        let state = &mut *state;
        if state.slot_of.contains_key(&block) {
            return;
        }
        let slot = if state.block_of.len() < self.capacity {
            state.block_of.push(EMPTY);
            state.marked.push(AtomicBool::new(false));
            state.block_of.len() - 1
        } else {
            // Each slot passed loses its mark, so the hand stops within two turns.
            loop {
                let slot = state.hand;
                state.hand = (slot + 1) % self.capacity;
                if !state.marked[slot].swap(false, Ordering::Relaxed) {
                    break slot;
                }
            }
        };
        state.slot_of.remove(&state.block_of[slot]);
        state.block_of[slot] = EMPTY;
        let kept = state.arena.write(self.position(slot), bytes).is_ok();
        if kept {
            state.block_of[slot] = block;
            state.slot_of.insert(block, slot);
        }
        // A new block is marked as one that was read is: it was fetched to be
        // read, and must not go before the blocks that were read already.
        state.marked[slot].store(kept, Ordering::Relaxed);
    }

    fn capacity(&self) -> Option<u64> {
        Some(self.capacity as u64)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, atomic::AtomicUsize};

    use super::*;

    /// The bytes of `block` in the test database: its number, repeated.
    fn block(layout: Layout, block: u64) -> Vec<u8> {
        vec![block as u8 + 1; layout.block_len(block) as usize]
    }

    /// The first byte of `block` if `cache` holds it.
    fn first(cache: &dyn BlockCache, block: u64) -> Option<u8> {
        let mut buf = [0];
        cache.read(block, 0, &mut buf).then_some(buf[0])
    }

    fn fill(cache: &dyn BlockCache, layout: Layout, blocks: impl IntoIterator<Item = u64>) {
        for index in blocks {
            cache.insert(index, &block(layout, index));
        }
    }

    fn held(cache: &dyn BlockCache, layout: Layout) -> Vec<u64> {
        (0..layout.blocks())
            .filter(|&index| first(cache, index) == Some(index as u8 + 1))
            .collect()
    }

    /// Ten blocks of 8 bytes and one of 3.
    const LAYOUT: Layout = Layout {
        size: 83,
        block_size: 8,
    };

    fn policies(bytes: u64) -> [Box<dyn CachePolicy>; 2] {
        [
            Box::new(MemoryCache::new(bytes)),
            Box::new(FileCache::new(bytes)),
        ]
    }

    #[test]
    fn layouts_count_blocks_and_their_bytes() {
        assert_eq!(LAYOUT.blocks(), 11);
        assert_eq!(LAYOUT.block_len(0), 8);
        assert_eq!(LAYOUT.block_len(10), 3);
        assert_eq!(LAYOUT.block_len(11), 0);
        assert_eq!(LAYOUT.block_len(u64::MAX), 0);
        assert_eq!(Layout::new(0, 4096).blocks(), 0);
        assert_eq!(Layout::new(4096, 4096).blocks(), 1);
        assert_eq!(Layout::new(4097, 4096).blocks(), 2);
        // A block size of zero is one byte, and the largest sizes do not overflow.
        assert_eq!(Layout::new(5, 0).block_size(), 1);
        let huge = Layout::new(u64::MAX, u64::MAX);
        assert_eq!(
            (huge.blocks(), huge.block_len(0), huge.block_len(1)),
            (1, u64::MAX, 0)
        );
        assert_eq!(Layout::new(u64::MAX, 1).blocks(), u64::MAX);
        // The largest block is a whole one, or all of a smaller database.
        assert_eq!(LAYOUT.largest_block(), 8);
        assert_eq!(Layout::new(5, 4096).largest_block(), 5);
        assert_eq!(Layout::new(0, 4096).largest_block(), 0);
    }

    #[test]
    fn blocks_read_back_whole_and_in_part() {
        for policy in policies(1 << 10) {
            let cache = policy.build(LAYOUT).unwrap();
            assert_eq!(first(&*cache, 3), None);
            fill(&*cache, LAYOUT, [3, 10]);
            let mut buf = [0; 8];
            assert!(cache.read(3, 0, &mut buf));
            assert_eq!(buf, [4; 8]);
            let mut buf = [0; 2];
            assert!(cache.read(3, 6, &mut buf));
            assert_eq!(buf, [4; 2]);
            assert!(cache.read(10, 1, &mut buf));
            assert_eq!(buf, [11; 2]);
            // Reads past a block's end, of other blocks or of nothing held all miss.
            assert!(!cache.read(3, 7, &mut buf));
            assert!(!cache.read(10, 2, &mut buf));
            assert!(!cache.read(3, usize::MAX, &mut buf));
            assert!(!cache.read(4, 0, &mut buf));
            assert!(!cache.read(11, 0, &mut buf));
            assert!(cache.read(3, 8, &mut []));
        }
    }

    #[test]
    fn blocks_of_the_wrong_size_are_not_kept() {
        for policy in policies(1 << 10) {
            let cache = policy.build(LAYOUT).unwrap();
            cache.insert(2, &[9; 7]);
            cache.insert(2, &[9; 9]);
            cache.insert(2, &[]);
            cache.insert(10, &[9; 8]);
            cache.insert(11, &[9; 8]);
            cache.insert(u64::MAX, &[9; 8]);
            assert_eq!(held(&*cache, LAYOUT), [] as [u64; 0]);
            // The first bytes kept for a block stay: a database does not change.
            fill(&*cache, LAYOUT, [2]);
            cache.insert(2, &[9; 8]);
            assert_eq!(first(&*cache, 2), Some(3));
        }
    }

    #[test]
    fn a_full_cache_drops_the_oldest_block_not_read_since_it_was_passed() {
        for policy in policies(3 * 8 + 7) {
            let cache = policy.build(LAYOUT).unwrap();
            fill(&*cache, LAYOUT, 0..3);
            // The block that arrived first goes first.
            fill(&*cache, LAYOUT, [3]);
            assert_eq!(first(&*cache, 0), None);
            // Block 1 is next, but it is read again: block 2 goes in its place.
            assert_eq!(first(&*cache, 1), Some(2));
            fill(&*cache, LAYOUT, [4]);
            assert_eq!(held(&*cache, LAYOUT), [1, 3, 4]);
        }
    }

    #[test]
    fn blocks_fetched_ahead_outlast_older_blocks_that_were_read() {
        for policy in policies(4 * 8) {
            let cache = policy.build(LAYOUT).unwrap();
            // A scan has read four blocks, and two more are fetched ahead of it.
            fill(&*cache, LAYOUT, 0..4);
            assert_eq!(held(&*cache, LAYOUT), [0, 1, 2, 3]);
            fill(&*cache, LAYOUT, [4, 5]);
            // It reads the older blocks again, and two more arrive: the
            // blocks it has not reached yet are still there.
            assert_eq!((first(&*cache, 2), first(&*cache, 3)), (Some(3), Some(4)));
            fill(&*cache, LAYOUT, [6, 7]);
            assert_eq!(held(&*cache, LAYOUT), [4, 5, 6, 7]);
        }
    }

    #[test]
    fn budgets_are_cut_to_whole_blocks_and_to_the_database() {
        for policy in policies(7) {
            let cache = policy.build(LAYOUT).unwrap();
            fill(&*cache, LAYOUT, 0..11);
            assert_eq!(held(&*cache, LAYOUT), [] as [u64; 0]);
        }
        for policy in policies(u64::MAX) {
            let cache = policy.build(LAYOUT).unwrap();
            fill(&*cache, LAYOUT, (0..11).rev());
            assert_eq!(held(&*cache, LAYOUT), (0..11).collect::<Vec<_>>());
        }
        assert_eq!(slots(u64::MAX, Layout::new(u64::MAX, 1)), usize::MAX);
        // A database smaller than one block takes only its own bytes.
        let small = Layout::new(5, 1 << 30);
        assert_eq!(
            (slots(4, small), slots(5, small), slots(u64::MAX, small)),
            (0, 1, 1)
        );
        assert_eq!(slots(u64::MAX, Layout::new(0, 4096)), 0);
        for policy in policies(5) {
            let cache = policy.build(small).unwrap();
            cache.insert(0, b"small");
            let mut buf = [0; 4];
            assert!(cache.read(0, 1, &mut buf));
            assert_eq!(&buf, b"mall");
        }
        let cache = NoCache.build(LAYOUT).unwrap();
        fill(&*cache, LAYOUT, 0..11);
        assert_eq!(held(&*cache, LAYOUT), [] as [u64; 0]);
    }

    #[test]
    fn caches_say_how_many_blocks_they_hold() {
        assert_eq!(NoCache.build(LAYOUT).unwrap().capacity(), Some(0));
        for (bytes, blocks) in [(0, 0), (7, 0), (8, 1), (31, 3), (88, 11), (u64::MAX, 11)] {
            for policy in policies(bytes) {
                assert_eq!(policy.build(LAYOUT).unwrap().capacity(), Some(blocks));
            }
        }
        let arena = Heap {
            block: 8,
            slots: Vec::new(),
        };
        assert_eq!(ClockCache::new(arena, LAYOUT, 5).capacity(), Some(5));
    }

    #[test]
    fn a_function_is_a_policy_and_the_file_can_be_placed() {
        let dir = tempfile::tempdir().unwrap();
        let cache = FileCache::new(64).in_dir(dir.path()).build(LAYOUT).unwrap();
        fill(&*cache, LAYOUT, 0..2);
        assert_eq!(held(&*cache, LAYOUT), [0, 1]);
        // The file has no name in the directory.
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
        let missing = dir.path().join("missing");
        assert!(FileCache::new(64).in_dir(missing).build(LAYOUT).is_err());

        let built = Arc::new(AtomicUsize::new(0));
        let count = built.clone();
        let policy = move |layout: Layout| {
            count.fetch_add(1, Ordering::Relaxed);
            MemoryCache::new(16).build(layout)
        };
        let cache = policy.build(LAYOUT).unwrap();
        fill(&*cache, LAYOUT, 0..2);
        assert_eq!(
            (held(&*cache, LAYOUT), built.load(Ordering::Relaxed)),
            (vec![0, 1], 1)
        );
    }

    /// An arena that fails when told to.
    struct Flaky {
        heap: Heap,
        fail: Arc<AtomicBool>,
    }

    impl Arena for Flaky {
        fn read(&self, at: u64, buf: &mut [u8]) -> io::Result<()> {
            if self.fail.load(Ordering::Relaxed) {
                return Err(io::ErrorKind::Other.into());
            }
            self.heap.read(at, buf)
        }
        fn write(&mut self, at: u64, bytes: &[u8]) -> io::Result<()> {
            if self.fail.load(Ordering::Relaxed) {
                return Err(io::ErrorKind::Other.into());
            }
            self.heap.write(at, bytes)
        }
    }

    #[test]
    fn a_failing_arena_misses_and_keeps_nothing_wrong() {
        let fail = Arc::new(AtomicBool::new(false));
        let heap = Heap {
            block: 8,
            slots: Vec::new(),
        };
        let arena = Flaky {
            heap,
            fail: fail.clone(),
        };
        let cache = ClockCache::new(arena, LAYOUT, 2);
        fill(&cache, LAYOUT, 0..2);
        fail.store(true, Ordering::Relaxed);
        assert_eq!(held(&cache, LAYOUT), [] as [u64; 0]);
        // A failed write drops the block that was in the slot and keeps neither.
        fill(&cache, LAYOUT, [5]);
        fail.store(false, Ordering::Relaxed);
        assert_eq!(held(&cache, LAYOUT), [1]);
        // The slot is used again.
        fill(&cache, LAYOUT, [6, 7]);
        assert_eq!(held(&cache, LAYOUT).len(), 2);
    }

    #[test]
    fn threads_never_read_another_blocks_bytes() {
        let layout = Layout::new(200 * 64, 64);
        for policy in policies(16 * 64) {
            let cache: Arc<dyn BlockCache> = Arc::from(policy.build(layout).unwrap());
            let threads: Vec<_> = (0..4u64)
                .map(|thread| {
                    let cache = cache.clone();
                    std::thread::spawn(move || {
                        let mut hits = 0;
                        for round in 0..2_000u64 {
                            let index = (round * 7 + thread * 13) % 200;
                            let mut buf = [0; 64];
                            if cache.read(index, 0, &mut buf) {
                                assert_eq!(buf, [index as u8 + 1; 64]);
                                hits += 1;
                            } else {
                                cache.insert(index, &block(layout, index));
                            }
                        }
                        hits
                    })
                })
                .collect();
            let hits: u64 = threads
                .into_iter()
                .map(|thread| thread.join().unwrap())
                .sum();
            assert!(hits > 0);
        }
    }
}

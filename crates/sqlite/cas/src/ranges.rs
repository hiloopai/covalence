//! A database read from its store by range, as SQLite asks for it.

use std::{
    ops::Range,
    sync::{
        Arc, PoisonError, RwLock,
        atomic::{AtomicU64, Ordering::Relaxed},
    },
};

use covalence_cas_trait::Cas;
use covalence_lib_cid::Cid;
use covalence_lib_error::snafu::{OptionExt, ensure};

use crate::{
    Error, MAX_BLOCK_SIZE,
    cache::{BlockCache, Layout},
    error::{LengthSnafu, MissingSnafu},
    fetch::{FetchPolicy, Miss},
    verify::RangeVerifier,
};

/// A database in a store, read in blocks through a cache.
pub(crate) struct Ranges {
    cas: Arc<dyn Cas>,
    cid: Cid,
    layout: Layout,
    /// Checks fetched blocks before they are used; `None` trusts the store.
    verifier: Option<Arc<dyn RangeVerifier>>,
    policy: RwLock<Arc<dyn FetchPolicy>>,
    cache: Box<dyn BlockCache>,
    fetches: AtomicU64,
    fetched_bytes: AtomicU64,
}

impl Ranges {
    /// The database `cid` of `cas`, of the size and blocks of `layout`.
    pub(crate) fn new(
        cas: Arc<dyn Cas>,
        cid: Cid,
        layout: Layout,
        verifier: Option<Arc<dyn RangeVerifier>>,
        policy: Arc<dyn FetchPolicy>,
        cache: Box<dyn BlockCache>,
    ) -> Self {
        Self {
            cas,
            cid,
            layout,
            verifier,
            policy: RwLock::new(policy),
            cache,
            fetches: AtomicU64::new(0),
            fetched_bytes: AtomicU64::new(0),
        }
    }

    /// The size of the database in bytes.
    pub(crate) fn size(&self) -> u64 {
        self.layout.size()
    }

    /// Use `policy` for the fetches that start from now on.
    pub(crate) fn set_policy(&self, policy: Arc<dyn FetchPolicy>) {
        *self.policy.write().unwrap_or_else(PoisonError::into_inner) = policy;
    }

    /// The requests made to the store, and the bytes it gave.
    pub(crate) fn fetched(&self) -> (u64, u64) {
        (self.fetches.load(Relaxed), self.fetched_bytes.load(Relaxed))
    }

    /// Fill `buf` with the bytes at `offset`, which the database must have.
    ///
    /// Blocks come from the cache until one is missing; one request to the
    /// store then gives the rest of the read.
    pub(crate) fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), Error> {
        let block_size = self.layout.block_size();
        let mut at = offset;
        let mut rest = buf;
        while !rest.is_empty() {
            let block = at / block_size;
            let within = at % block_size;
            let left = self.layout.block_len(block).saturating_sub(within);
            let take = left.min(rest.len() as u64) as usize;
            // No bytes left in the block: the read is past the database's end.
            if take == 0 || !self.cache.read(block, within as usize, &mut rest[..take]) {
                return self.fetch(at, rest);
            }
            at += take as u64;
            rest = &mut std::mem::take(&mut rest)[take..];
        }
        Ok(())
    }

    /// Fill `buf` with the bytes at `at` from one request to the store, and
    /// offer the blocks it gave to the cache.
    fn fetch(&self, at: u64, buf: &mut [u8]) -> Result<(), Error> {
        let (size, block_size) = (self.layout.size(), self.layout.block_size());
        let end = at.checked_add(buf.len() as u64).filter(|end| *end <= size);
        let end = end.context(LengthSnafu {
            offset: at,
            expected: buf.len() as u64,
            actual: size.saturating_sub(at),
        })?;
        let need = at / block_size..end.div_ceil(block_size);
        let policy = Arc::clone(&self.policy.read().unwrap_or_else(PoisonError::into_inner));
        let miss = Miss::new(need.clone(), self.layout.blocks(), block_size);
        let blocks = window(&need, policy.plan(&miss), &miss);
        let start = blocks.start * block_size;
        let len = (blocks.end.saturating_mul(block_size)).min(size) - start;

        self.fetches.fetch_add(1, Relaxed);
        // A verifier checks the bytes here, so the store need not; without
        // one they are taken as the store vouches for them.
        let bytes = match &self.verifier {
            Some(_) => self.cas.get_range_unchecked(&self.cid, start, len),
            None => self.cas.get_range(&self.cid, start, len),
        };
        let bytes = (bytes.map_err(crate::store)?).context(MissingSnafu { cid: self.cid })?;
        self.fetched_bytes.fetch_add(bytes.len() as u64, Relaxed);
        ensure!(
            bytes.len() as u64 == len,
            LengthSnafu {
                offset: start,
                expected: len,
                actual: bytes.len() as u64,
            }
        );
        if let Some(verifier) = &self.verifier {
            verifier
                .verify(blocks.start, &bytes)
                .map_err(|source| Error::Corrupt {
                    offset: start,
                    source,
                })?;
        }
        for (block, bytes) in (blocks.start..).zip(bytes.chunks(block_size as usize)) {
            self.cache.insert(block, bytes);
        }
        let from = (at - start) as usize;
        buf.copy_from_slice(&bytes[from..from + buf.len()]);
        Ok(())
    }
}

/// The blocks to fetch for `miss`: `plan`, widened to hold `need`, and cut to
/// the database and to the most bytes of one fetch. An empty plan is `need`.
fn window(need: &Range<u64>, plan: Range<u64>, miss: &Miss) -> Range<u64> {
    let plan = if plan.is_empty() { need.clone() } else { plan };
    let most = (MAX_BLOCK_SIZE / miss.block_size.max(1)).max(need.end - need.start);
    let end = (plan.end.max(need.end).min(miss.blocks)).min(need.start.saturating_add(most));
    let start = plan.start.min(need.start).max(end.saturating_sub(most));
    start..end
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_hold_the_need_and_stay_in_the_database_and_the_limit() {
        let miss = |need: Range<u64>| Miss::new(need, 1_000, 4096);
        let check = |need: Range<u64>, plan: Range<u64>, expected: Range<u64>| {
            assert_eq!(
                window(&need, plan.clone(), &miss(need.clone())),
                expected,
                "{plan:?}"
            );
        };
        // A plan that holds the need is kept; others are widened to hold it.
        check(10..12, 8..20, 8..20);
        check(10..12, 10..12, 10..12);
        check(10..12, 11..12, 10..12);
        check(10..12, 0..0, 10..12);
        check(10..12, 500..600, 10..600);
        check(10..12, 0..5, 0..12);
        // Plans are cut to the database, and inverted or huge ones do no harm.
        check(998..1_000, 990..5_000, 990..1_000);
        let inverted = Range { start: 20, end: 3 };
        check(10..12, inverted, 10..12);
        check(10..12, u64::MAX..u64::MAX, 10..12);
        check(10..12, 900..u64::MAX, 10..1_000);
        check(10..12, 0..u64::MAX, 0..1_000);

        // A fetch is at most `MAX_BLOCK_SIZE` bytes, taken from the need on.
        let most = MAX_BLOCK_SIZE / 4096;
        let large = Miss::new(most..most + 2, 10 * most, 4096);
        assert_eq!(window(&large.need, 0..u64::MAX, &large), most..2 * most);
        assert_eq!(window(&large.need, 0..most + 2, &large), 2..most + 2);
        // A read that needs more than the limit is still fetched whole.
        let wide = Miss::new(0..10, 10, MAX_BLOCK_SIZE);
        assert_eq!(window(&wide.need, 0..0, &wide), 0..10);
        let bytes = Miss::new(5..7, u64::MAX, 1);
        assert_eq!(
            window(&bytes.need, 0..u64::MAX, &bytes),
            5..5 + MAX_BLOCK_SIZE
        );
    }
}

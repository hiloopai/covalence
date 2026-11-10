//! The shared Lake build pool: one store per platform and toolchain, shared
//! by every repository, worktree and host.
//!
//! Keys under the bucket, for a scope `<os>-<arch>/<toolchain>`:
//!
//! - `lake/<scope>/cas/sha2-256/<digest>/bytes.zst`: artifact bytes under
//!   the SHA-256 CID of the raw bytes, as a Zstandard frame.
//! - `lake/<scope>/cas/blake3/<digest>/bytes.zst`: map nodes, in a [`KvCas`]
//!   with Zstandard; Prolly names its nodes by BLAKE3.
//! - `lake/<scope>/rev/<owner>/<repo>/<commit>/<package>`: the root CID of
//!   that package's [`Map`] at that commit, as text.
//!
//! A revision map is a Prolly map from a Lake input hash, as 16 lowercase hex
//! digits, to an [`Entry`]. An entry's `artifacts` are the only place Lake
//! artifact IDs meet CIDs: once Lake hashes artifacts with a CID-compatible
//! hash, that map becomes the identity and this tool can retire.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

use covalence_cas_kv::{Compression, KvCas};
use covalence_cas_prolly::Map;
use covalence_cas_trait::{Cas, GetExt};
use covalence_kv_trait::Kv;
use covalence_lib_cid::{Bdasl, Cid, Codec, Packed32, Sha256, storage};
use covalence_lib_dasl::Value;
use covalence_lib_data::Bytes;

use crate::{
    blobs::Blobs,
    error::{Error, Result, invalid},
    mapping,
    storage::parallel,
};

/// The version of this layout, mixed into every toolchain scope.
pub const FORMAT: &str = "cov-lake/v1";

/// The content-addressed store of a scope's map nodes.
pub fn store(kv: Arc<dyn Kv>, scope: &str) -> Result<KvCas> {
    mapping::segment_path(scope)?;
    KvCas::with_prefix(kv, &format!("lake/{scope}/cas"))
        .map(|cas| cas.with_compression(Compression::zstd(1)))
        .map_err(|_| invalid("invalid pool scope"))
}

/// The store of a scope's artifacts.
pub(crate) fn blobs(kv: Arc<dyn Kv>, scope: &str) -> Result<Blobs> {
    mapping::segment_path(scope)?;
    Ok(Blobs::new(kv, &format!("lake/{scope}/cas")))
}

/// The key of a package's revision pointer.
pub fn revision_key(scope: &str, repository: &str, commit: &str, package: &str) -> Result<String> {
    for name in [scope, repository, package] {
        mapping::segment_path(name)?;
    }
    if !mapping::hex(commit, 40) {
        return Err(invalid("expected a 40-digit Git revision"));
    }
    Ok(format!("lake/{scope}/rev/{repository}/{commit}/{package}"))
}

/// What one Lake input produced.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    /// Lake's output descriptor, exactly as its mapping files write it.
    pub output: String,
    /// Each artifact the descriptor names, by Lake file name, with its CID.
    pub artifacts: BTreeMap<String, Sha256>,
}

fn cid_error(operation: &'static str) -> impl FnOnce(covalence_lib_cid::Error) -> Error {
    move |source| Error::Cid {
        operation: operation.into(),
        source,
    }
}

fn map_error(operation: &'static str) -> impl FnOnce(covalence_cas_prolly::Error) -> Error {
    move |source| Error::Map {
        operation: operation.into(),
        source,
    }
}

fn cas_error(operation: &'static str) -> impl FnOnce(covalence_cas_trait::Error) -> Error {
    move |source| Error::Cas {
        operation: operation.into(),
        source,
    }
}

impl Entry {
    fn to_value(&self) -> Result<Value> {
        let artifacts = self
            .artifacts
            .iter()
            .map(|(name, cid)| {
                Ok((
                    name.clone(),
                    Value::Link(cid.cast().map_err(cid_error("link artifact"))?),
                ))
            })
            .collect::<Result<_>>()?;
        Ok(Value::Map(BTreeMap::from([
            ("output".into(), Value::String(self.output.clone())),
            ("artifacts".into(), Value::Map(artifacts)),
        ])))
    }

    fn from_value(value: Value) -> Result<Self> {
        let malformed = || invalid("malformed revision map entry");
        let Value::Map(mut fields) = value else {
            return Err(malformed());
        };
        let (Some(Value::String(output)), Some(Value::Map(links)), true) = (
            fields.remove("output"),
            fields.remove("artifacts"),
            fields.is_empty(),
        ) else {
            return Err(malformed());
        };
        let artifacts = links
            .into_iter()
            .map(|(name, link)| match link {
                Value::Link(cid) if mapping::artifact_name(&name) => cid
                    .cast::<storage::Sha256>()
                    .map(|cid| (name, cid))
                    .map_err(|_| malformed()),
                _ => Err(malformed()),
            })
            .collect::<Result<_>>()?;
        Ok(Self { output, artifacts })
    }
}

fn input(key: &[u8]) -> Result<String> {
    String::from_utf8(key.to_vec())
        .ok()
        .filter(|input| mapping::hex(input, 16))
        .ok_or_else(|| invalid("invalid revision map key"))
}

/// Store a revision map of `entries`, keyed by Lake input hash.
pub fn build(cas: Arc<dyn Cas>, entries: &BTreeMap<String, Entry>) -> Result<Map> {
    let entries = entries
        .iter()
        .map(|(input, entry)| {
            if !mapping::hex(input, 16) {
                return Err(invalid("invalid Lake input hash"));
            }
            Ok((Bytes::copy_from_slice(input.as_bytes()), entry.to_value()?))
        })
        .collect::<Result<Vec<_>>>()?;
    Map::build(cas, entries).map_err(map_error("build revision map"))
}

/// Every entry of a revision map.
pub fn entries(map: &Map) -> Result<BTreeMap<String, Entry>> {
    map.range(..)
        .map_err(map_error("read revision map"))?
        .map(|item| {
            let (key, value) = item.map_err(map_error("read revision map"))?;
            Ok((input(&key)?, Entry::from_value(value)?))
        })
        .collect()
}

/// What turns a host holding one revision map into one holding another.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Transfer {
    /// Entries the target adds or changes, by Lake input hash.
    pub entries: BTreeMap<String, Entry>,
    /// Artifacts those entries name that the entries they replace did not,
    /// by Lake file name.
    pub artifacts: BTreeMap<String, Sha256>,
}

/// The minimal transfer from the map at `from`, if any, to `to`: Prolly's
/// root-to-root diff, which skips the subtrees both maps share unread.
pub fn transfer(from: Option<&Map>, to: &Map) -> Result<Transfer> {
    let mut transfer = Transfer::default();
    let mut replaced = BTreeSet::new();
    match from {
        None => transfer.entries = entries(to)?,
        Some(from) => {
            for change in from.diff(to).map_err(map_error("diff revision maps"))? {
                if let Some(old) = change.old {
                    replaced.extend(Entry::from_value(old)?.artifacts.into_values());
                }
                if let Some(new) = change.new {
                    transfer
                        .entries
                        .insert(input(&change.key)?, Entry::from_value(new)?);
                }
            }
        }
    }
    for entry in transfer.entries.values() {
        for (name, cid) in &entry.artifacts {
            if !replaced.contains(cid) {
                transfer.artifacts.insert(name.clone(), *cid);
            }
        }
    }
    Ok(transfer)
}

/// Copy the nodes of the map at `root` that `to` lacks from `from`, children
/// before parents, and return how many were copied.
///
/// A store that receives nodes only through this function or a map build
/// holds whole subtrees, so a node `to` already has ends that branch of the
/// walk. Each tree level is one parallel round of requests. Artifacts, which
/// are raw links, are not copied.
pub fn copy_nodes(from: &dyn Cas, to: &dyn Cas, root: Cid<Bdasl>) -> Result<usize> {
    let node = |link: Cid<Bdasl>| {
        link.cast::<Packed32>()
            .map_err(cid_error("address map node"))
    };
    let mut levels = Vec::new();
    let mut frontier = vec![node(root)?];
    while !frontier.is_empty() {
        let fetched = parallel(&frontier, |_, cid| {
            if to.has(cid).map_err(cas_error("inspect map node"))? {
                return Ok(None);
            }
            let fact = from
                .get_fact(cid)
                .map_err(cas_error("read map node"))?
                .ok_or_else(|| invalid(format!("map node {cid} is missing")))?;
            let value = Value::from_fact(&fact).map_err(|source| Error::Dasl {
                operation: "decode map node".into(),
                source,
            })?;
            let children = value
                .links()
                .filter(|link| link.codec() == Codec::Drisl.code())
                .collect::<Vec<_>>();
            Ok(Some((fact.bytes().clone(), children)))
        })?;
        let mut next = BTreeSet::new();
        let mut level = Vec::new();
        for (bytes, children) in fetched.into_iter().flatten() {
            level.push(bytes);
            for child in children {
                next.insert(node(child)?);
            }
        }
        levels.push(level);
        frontier = next.into_iter().collect();
    }
    let mut copied = 0;
    for level in levels.into_iter().rev() {
        copied += level.len();
        parallel(&level, |_, bytes| {
            to.put(bytes.clone()).map_err(cas_error("store map node"))?;
            Ok(())
        })?;
    }
    Ok(copied)
}

#[cfg(test)]
mod tests {
    use super::*;
    use covalence_cas_memory::MemoryCas;
    use covalence_kv_memory::MemoryKv;

    fn entry(output: &str, artifact: &[u8]) -> Entry {
        Entry {
            output: format!("\"{output}\""),
            artifacts: BTreeMap::from([(output.to_owned(), Sha256::of(artifact))]),
        }
    }

    fn inputs(count: u64) -> BTreeMap<String, Entry> {
        (0..count)
            .map(|i| {
                (
                    format!("{i:016x}"),
                    entry(&format!("{i:016x}.ltar"), &i.to_le_bytes()),
                )
            })
            .collect()
    }

    #[test]
    fn entries_round_trip_and_reject_malformed_keys() {
        let cas = MemoryCas::default().into_dyn();
        let all = inputs(3000);
        let map = build(cas.clone(), &all).unwrap();
        assert_eq!(entries(&map).unwrap(), all);
        let bad = BTreeMap::from([("xyz".to_owned(), entry("000000000000000a.ltar", b"a"))]);
        assert!(build(cas, &bad).is_err());
    }

    #[test]
    fn transfer_holds_only_changed_entries_and_their_new_artifacts() {
        let cas = MemoryCas::default().into_dyn();
        let mut all = inputs(3000);
        let old = build(cas.clone(), &all).unwrap();
        assert_eq!(transfer(None, &old).unwrap().artifacts.len(), 3000);
        assert_eq!(transfer(Some(&old), &old).unwrap(), Transfer::default());
        let edited = entry("00000000000000ff.ltar", b"edited");
        all.insert(format!("{:016x}", 7), edited.clone());
        all.insert(
            format!("{:016x}", 9000),
            entry("0000000000009000.ltar", b"new"),
        );
        let new = build(cas, &all).unwrap();
        let delta = transfer(Some(&old), &new).unwrap();
        assert_eq!(delta.entries.len(), 2);
        assert_eq!(delta.entries[&format!("{:016x}", 7)], edited);
        assert_eq!(delta.artifacts.len(), 2);
        assert!(delta.artifacts.contains_key("00000000000000ff.ltar"));
    }

    #[test]
    fn copying_nodes_skips_shared_subtrees_and_never_copies_artifacts() {
        let local = MemoryCas::default().into_dyn();
        let remote: Arc<dyn Cas> =
            Arc::new(store(MemoryKv::default().into_dyn(), "linux-x86_64/t").unwrap());
        let mut all = inputs(5000);
        let old = build(local.clone(), &all).unwrap();
        let first = copy_nodes(&*local, &*remote, old.root()).unwrap();
        assert!(first > 2);
        assert_eq!(copy_nodes(&*local, &*remote, old.root()).unwrap(), 0);
        all.insert(
            format!("{:016x}", 1),
            entry("00000000000000ee.ltar", b"edited"),
        );
        let new = build(local.clone(), &all).unwrap();
        let changed = copy_nodes(&*local, &*remote, new.root()).unwrap();
        assert!(0 < changed && changed < first / 4, "{changed} of {first}");
        let fresh = MemoryCas::default().into_dyn();
        let total = copy_nodes(&*remote, &*fresh, new.root()).unwrap();
        assert!(total >= first);
        assert_eq!(entries(&Map::open(fresh, new.root())).unwrap(), all);
    }

    #[test]
    fn revision_keys_reject_escapes() {
        let commit = "a".repeat(40);
        let key = revision_key("linux-x86_64/t", "o/r", &commit, "pkg").unwrap();
        assert_eq!(key, format!("lake/linux-x86_64/t/rev/o/r/{commit}/pkg"));
        assert!(revision_key("linux-x86_64/t", "o/../r", &commit, "p").is_err());
        assert!(revision_key("linux-x86_64/t", "o/r", "HEAD", "p").is_err());
    }
}

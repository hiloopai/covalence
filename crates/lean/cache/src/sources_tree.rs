//! Publish a commit's source tree to the pool, beside its builds.
//!
//! Under `lake/src/` in the bucket, the layout `hiloop prove publish` reads:
//!
//! - `cas/blake3/<digest>/bytes`: blobs and trees, uncompressed, in a
//!   key-value CAS. A blob's CID is the raw BLAKE3 CID of the Git blob's bytes.
//! - `rev/<owner>/<repo>/<commit>`: the CID text of that commit's record, the
//!   DRISL map `{"tree": <link>, "targets": [<Lake target>, ...]}`.
//!
//! A tree is the DRISL map `{"files": {<path>: <link>}}` over every blob the
//! commit tracks. Writes go blobs, then tree, then record, then pointer, so a
//! pointer always names a complete tree.

use std::{
    collections::{BTreeMap, BTreeSet},
    io::{BufRead, BufReader, Read, Write},
    path::Path,
    process::{Command, Stdio},
    sync::Arc,
    thread,
};

use covalence_cas_kv::KvCas;
use covalence_cas_trait::{Cas, Get, Put};
use covalence_kv_trait::Kv;
use covalence_lib_cid::{Bdasl, Cid, Codec, HashFn, Packed32};
use covalence_lib_dasl::Value;
use covalence_lib_data::Bytes;

use crate::{
    error::{Error, IoResult, Result, invalid},
    mapping,
    project::Project,
    storage::parallel,
};

/// Blobs asked about, read and sent per round, so memory stays bounded.
const BATCH: usize = 256;
/// First path components a tree may not use.
const RESERVED: [&str; 3] = [".git", ".lake", ".beam"];

fn cas_error(operation: &'static str) -> impl FnOnce(covalence_cas_trait::Error) -> Error {
    move |source| Error::Cas {
        operation: operation.into(),
        source,
    }
}

fn dasl_error(operation: &'static str) -> impl FnOnce(covalence_lib_dasl::Error) -> Error {
    move |source| Error::Dasl {
        operation: operation.into(),
        source,
    }
}

fn link(cid: &Cid) -> Result<Cid<Bdasl>> {
    cid.cast().map_err(|source| Error::Cid {
        operation: "link a source blob".into(),
        source,
    })
}

/// The key of a commit's record pointer.
fn pointer(repository: &str, commit: &str) -> Result<String> {
    mapping::segment_path(repository)?;
    if repository.split('/').count() != 2 || !mapping::hex(commit, 40) {
        return Err(invalid(
            "source records are keyed by owner/name and a commit",
        ));
    }
    Ok(format!("lake/src/rev/{repository}/{commit}"))
}

fn safe(path: &str) -> bool {
    let parts = path.split('/').collect::<Vec<_>>();
    !path.contains(['\\', '\0'])
        && !path.starts_with('/')
        && !parts.iter().any(|part| matches!(*part, "" | "." | ".."))
        && !RESERVED.contains(&parts[0])
}

/// Every blob `commit` tracks, as path and Git object id.
fn tracked(root: &Path, commit: &str) -> Result<Vec<(String, String)>> {
    let listing = Command::new("git")
        .current_dir(root)
        .args(["ls-tree", "-r", "-z", commit])
        .stderr(Stdio::inherit())
        .output()
        .operation("start git ls-tree")?;
    if !listing.status.success() {
        return Err(Error::Process {
            program: "git ls-tree",
            status: listing.status,
        });
    }
    let mut entries = Vec::new();
    for record in listing.stdout.split(|&byte| byte == 0) {
        if record.is_empty() {
            continue;
        }
        let record = str::from_utf8(record).map_err(|_| invalid("a tracked path is not UTF-8"))?;
        let (meta, path) = record
            .split_once('\t')
            .ok_or_else(|| invalid("unexpected git ls-tree output"))?;
        let mut meta = meta.split(' ');
        let (Some(_mode), Some(kind), Some(object)) = (meta.next(), meta.next(), meta.next())
        else {
            return Err(invalid("unexpected git ls-tree output"));
        };
        if kind != "blob" {
            continue;
        }
        if !safe(path) {
            return Err(invalid(format!("unsafe tracked path {path:?}")));
        }
        entries.push((path.to_owned(), object.to_owned()));
    }
    Ok(entries)
}

/// Stream the contents of Git objects, in order, through one
/// `git cat-file --batch`, passing each to `each`.
fn read_objects(
    root: &Path,
    objects: &[String],
    mut each: impl FnMut(usize, Vec<u8>) -> Result<()>,
) -> Result<()> {
    if objects.is_empty() {
        return Ok(());
    }
    let mut child = Command::new("git")
        .current_dir(root)
        .args(["cat-file", "--batch"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .operation("start git cat-file")?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| invalid("no cat-file stdin"))?;
    let request = objects.iter().map(|o| format!("{o}\n")).collect::<String>();
    // Git blocks on a full stdout pipe while the request is still going in.
    let writer = thread::spawn(move || stdin.write_all(request.as_bytes()));
    let mut stdout = BufReader::new(
        child
            .stdout
            .take()
            .ok_or_else(|| invalid("no cat-file stdout"))?,
    );
    for (index, object) in objects.iter().enumerate() {
        let mut header = String::new();
        stdout
            .read_line(&mut header)
            .operation("read git cat-file")?;
        let fields = header.split_whitespace().collect::<Vec<_>>();
        let [name, _kind, size] = fields[..] else {
            return Err(invalid(format!("git cat-file did not return {object}")));
        };
        if name != object {
            return Err(invalid(format!("git cat-file did not return {object}")));
        }
        let size = size
            .parse::<usize>()
            .map_err(|_| invalid("bad git cat-file size"))?;
        let mut data = vec![0; size];
        stdout.read_exact(&mut data).operation("read git object")?;
        let mut newline = [0];
        stdout
            .read_exact(&mut newline)
            .operation("read git object")?;
        each(index, data)?;
    }
    writer
        .join()
        .map_err(|_| Error::Worker)?
        .operation("write git cat-file request")?;
    let status = child.wait().operation("wait for git cat-file")?;
    if !status.success() {
        return Err(Error::Process {
            program: "git cat-file",
            status,
        });
    }
    Ok(())
}

/// The CIDs of a published tree's files.
fn tree_cids(cas: &KvCas, tree: &Cid) -> Result<BTreeSet<Cid>> {
    let Some(bytes) = cas.get(tree).map_err(cas_error("read published tree"))? else {
        return Ok(BTreeSet::new());
    };
    let Value::Map(mut fields) = Value::decode(&bytes).map_err(dasl_error("decode tree"))? else {
        return Err(invalid("a published tree is not a map"));
    };
    let Some(Value::Map(files)) = fields.remove("files") else {
        return Err(invalid("a published tree has no files"));
    };
    files
        .into_values()
        .map(|value| match value {
            Value::Link(cid) => cid.cast::<Packed32>().map_err(|source| Error::Cid {
                operation: "read a published tree".into(),
                source,
            }),
            _ => Err(invalid("a published tree links something else")),
        })
        .collect()
}

/// The tree of the nearest of `commits` with a published record.
fn nearest_tree(
    kv: &dyn Kv,
    cas: &KvCas,
    repository: &str,
    commits: &[String],
) -> Result<Option<Cid>> {
    let records = parallel(commits, |_, commit| {
        kv.get(pointer(repository, commit)?.as_bytes())
            .map_err(|source| Error::Store {
                operation: "read source record pointer".into(),
                source,
            })
    })?;
    let Some(text) = records.into_iter().flatten().next() else {
        return Ok(None);
    };
    let record: Cid = str::from_utf8(&text)
        .ok()
        .and_then(|text| text.trim().parse().ok())
        .ok_or_else(|| invalid("invalid source record pointer"))?;
    let Some(bytes) = cas.get(&record).map_err(cas_error("read source record"))? else {
        return Err(invalid("a source record pointer names a missing record"));
    };
    let Value::Map(mut fields) = Value::decode(&bytes).map_err(dasl_error("decode record"))? else {
        return Err(invalid("a source record is not a map"));
    };
    match fields.remove("tree") {
        Some(Value::Link(tree)) => Ok(Some(tree.cast().map_err(|source| Error::Cid {
            operation: "read a source record".into(),
            source,
        })?)),
        _ => Err(invalid("a source record has no tree")),
    }
}

/// A tree's DRISL bytes and CID.
fn encode_tree(
    entries: &[(String, String)],
    by_object: &BTreeMap<String, Cid>,
) -> Result<(Vec<u8>, Cid<Bdasl>)> {
    let files = entries
        .iter()
        .map(|(path, object)| Ok((path.clone(), Value::Link(link(&by_object[object])?))))
        .collect::<Result<BTreeMap<_, _>>>()?;
    let tree = Value::Map(BTreeMap::from([("files".to_owned(), Value::Map(files))]));
    let bytes = tree.encode().map_err(dasl_error("encode tree"))?;
    let cid = Cid::<Bdasl>::of(Codec::Drisl, HashFn::Blake3, &bytes);
    Ok((bytes, cid))
}

/// A record's DRISL bytes and CID.
fn encode_record(tree: Cid<Bdasl>, targets: &[String]) -> Result<(Vec<u8>, Cid<Bdasl>)> {
    let record = Value::Map(BTreeMap::from([
        ("tree".to_owned(), Value::Link(tree)),
        (
            "targets".to_owned(),
            Value::List(targets.iter().cloned().map(Value::String).collect()),
        ),
    ]));
    let bytes = record.encode().map_err(dasl_error("encode record"))?;
    let cid = Cid::<Bdasl>::of(Codec::Drisl, HashFn::Blake3, &bytes);
    Ok((bytes, cid))
}

/// Each tracked path with its Git object id, and each object's CID.
type Snapshot = (Vec<(String, String)>, BTreeMap<String, Cid>);

/// Every tracked blob of `commit` by path, and each object's CID.
fn snapshot(root: &Path, commit: &str) -> Result<Snapshot> {
    let entries = tracked(root, commit)?;
    let objects = entries
        .iter()
        .map(|(_, object)| object.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    let mut cids = Vec::with_capacity(objects.len());
    read_objects(root, &objects, |_, data| {
        cids.push(Cid::from(covalence_lib_cid::Blake3::of(&data)));
        Ok(())
    })?;
    Ok((entries, objects.into_iter().zip(cids).collect()))
}

/// What a source publication did.
#[derive(Debug, covalence_lib_serde::Serialize)]
#[serde(crate = "covalence_lib_serde::serde")]
pub(crate) struct Published {
    pub commit: String,
    pub record: String,
    pub tree: String,
    pub files: usize,
    pub uploaded: usize,
}

/// Publish `commit`'s tree and the Lake `targets` its CI built.
pub(crate) fn publish(
    project: &Project,
    kv: &Arc<dyn Kv>,
    commit: &str,
    targets: &[String],
) -> Result<Published> {
    let repository = &project.repository;
    pointer(repository, commit)?;
    let cas = KvCas::with_prefix(kv.clone(), "lake/src/cas")
        .map_err(|_| invalid("invalid source CAS prefix"))?;
    let (entries, by_object) = snapshot(&project.root, commit)?;
    let ancestors = project.git(&["rev-list", "--first-parent", "--max-count=65", commit])?;
    let ancestors = ancestors
        .lines()
        .skip(1)
        .map(str::to_owned)
        .collect::<Vec<_>>();
    let held = match nearest_tree(&**kv, &cas, repository, &ancestors)? {
        Some(tree) => tree_cids(&cas, &tree)?,
        None => BTreeSet::new(),
    };
    let fresh = by_object
        .iter()
        .filter(|(_, cid)| !held.contains(cid))
        .map(|(object, cid)| (object.clone(), *cid))
        .collect::<Vec<_>>();
    let mut uploaded = 0;
    for batch in fresh.chunks(BATCH) {
        let present = parallel(batch, |_, (_, cid)| {
            cas.has(cid).map_err(cas_error("inspect source blob"))
        })?;
        let missing = batch
            .iter()
            .zip(present)
            .filter(|(_, present)| !present)
            .map(|((object, cid), _)| (object.clone(), *cid))
            .collect::<Vec<_>>();
        let names = missing
            .iter()
            .map(|(object, _)| object.clone())
            .collect::<Vec<_>>();
        let mut contents = Vec::with_capacity(missing.len());
        read_objects(&project.root, &names, |_, data| {
            contents.push(Bytes::from(data));
            Ok(())
        })?;
        let blobs = missing.iter().zip(contents).collect::<Vec<_>>();
        parallel(&blobs, |_, ((_, cid), data)| {
            let stored: Cid =
                Put::<Cid>::put(&cas, data.clone()).map_err(cas_error("upload source blob"))?;
            if stored != *cid {
                return Err(invalid(format!(
                    "a source blob read for {cid} hashes to {stored}"
                )));
            }
            Ok(())
        })?;
        uploaded += missing.len();
    }
    let (tree_bytes, tree_cid) = encode_tree(&entries, &by_object)?;
    Put::<Cid>::put(&cas, tree_bytes.into()).map_err(cas_error("upload tree"))?;
    let (record_bytes, record_cid) = encode_record(tree_cid, targets)?;
    Put::<Cid>::put(&cas, record_bytes.into()).map_err(cas_error("upload record"))?;
    kv.put(
        pointer(repository, commit)?.as_bytes(),
        Bytes::copy_from_slice(record_cid.to_string().as_bytes()),
    )
    .map_err(|source| Error::Store {
        operation: "publish source record pointer".into(),
        source,
    })?;
    Ok(Published {
        commit: commit.to_owned(),
        record: record_cid.to_string(),
        tree: tree_cid.to_string(),
        files: entries.len(),
        uploaded,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git(root: &Path, args: &[&str]) -> String {
        crate::project::output(
            "git",
            Command::new("git")
                .current_dir(root)
                .args(["-c", "user.name=T", "-c", "user.email=t@example.invalid"])
                .args(args),
        )
        .unwrap()
    }

    #[test]
    fn publication_writes_blobs_tree_record_then_pointer_and_skips_held_blobs() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().to_owned();
        git(&root, &["init", "-q"]);
        std::fs::create_dir(root.join("Dir")).unwrap();
        std::fs::write(root.join("A.lean"), "theorem a : True := trivial\n").unwrap();
        std::fs::write(root.join("Dir/B.lean"), "-- b\n").unwrap();
        git(&root, &["add", "."]);
        git(
            &root,
            &["-c", "commit.gpgsign=false", "commit", "-qm", "one"],
        );
        let first = git(&root, &["rev-parse", "HEAD"]);
        let project = Project {
            root: root.clone(),
            repository: "o/r".into(),
            record: root.join("record"),
        };
        let kv = covalence_kv_memory::MemoryKv::default().into_dyn();
        let targets = ["Demo".to_owned()];
        let published = publish(&project, &kv, &first, &targets).unwrap();
        assert_eq!((published.files, published.uploaded), (2, 2));
        let cas = KvCas::with_prefix(kv.clone(), "lake/src/cas").unwrap();
        let pointer = kv
            .get(format!("lake/src/rev/o/r/{first}").as_bytes())
            .unwrap()
            .unwrap();
        assert_eq!(str::from_utf8(&pointer).unwrap(), published.record);
        let record: Cid = published.record.parse().unwrap();
        let Value::Map(fields) = Value::decode(&cas.get(&record).unwrap().unwrap()).unwrap() else {
            panic!("record is not a map");
        };
        assert_eq!(
            fields["targets"],
            Value::List(vec![Value::String("Demo".into())])
        );
        let tree: Cid = published.tree.parse().unwrap();
        let held = tree_cids(&cas, &tree).unwrap();
        assert_eq!(held.len(), 2);
        assert!(held.iter().all(|cid| cas.has(cid).unwrap()));
        // A child commit uploads only the blob its parent's tree lacks.
        std::fs::write(root.join("Dir/B.lean"), "-- b, edited\n").unwrap();
        git(
            &root,
            &["-c", "commit.gpgsign=false", "commit", "-qam", "two"],
        );
        let second = git(&root, &["rev-parse", "HEAD"]);
        let child = publish(&project, &kv, &second, &targets).unwrap();
        assert_eq!((child.files, child.uploaded), (2, 1));
    }

    #[test]
    fn paths_and_pointers_follow_the_prove_layout() {
        for path in ["a.lean", "Dir/B.lean", ".github/ci.yml"] {
            assert!(safe(path), "{path}");
        }
        for path in ["", "/a", "a//b", "a/../b", ".lake/x", ".git/config", "a\\b"] {
            assert!(!safe(path), "{path}");
        }
        assert_eq!(
            pointer("o/r", &"a".repeat(40)).unwrap(),
            format!("lake/src/rev/o/r/{}", "a".repeat(40))
        );
        assert!(pointer("o", &"a".repeat(40)).is_err());
    }
}

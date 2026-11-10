//! Module archives unpacked in this process, into Lake's artifact cache.
//!
//! Lake restores a module whose cached output is only its `.ltar` by running
//! `leantar` once per module. For the archives an eager restore installs,
//! this tool instead downloads and decodes each one here, 64 at a time, writes
//! its files into Lake's artifact directory under the names its trace gives
//! them, and hands Lake the trace's outputs as the mapping. Lake then finds
//! every file in its cache and runs no archiver.

use std::{
    collections::BTreeMap,
    fs,
    io::{self, Write},
    path::Path,
    time::Instant,
};

use covalence_lib_cid::Sha256;
use serde_json::Value;

use crate::{
    blobs::Blobs,
    error::{Error, IoResult, Result, invalid, json},
    mapping,
    pool::Entry,
    storage,
};

/// The archive an entry's output is, if it is only one.
fn archive(entry: &Entry) -> Option<&Sha256> {
    let Ok(Value::String(name)) = serde_json::from_str(&entry.output) else {
        return None;
    };
    name.ends_with(".ltar")
        .then(|| entry.artifacts.get(&name))
        .flatten()
}

/// Lake's output descriptor for each of `entries`, by input hash: for each
/// archive, the outputs its trace names, now in `artifacts`; else the
/// entry's own.
pub(crate) fn unpack(
    blobs: &Blobs,
    artifacts: &Path,
    entries: &BTreeMap<String, Entry>,
) -> Result<BTreeMap<String, String>> {
    let started = Instant::now();
    let archives: Vec<_> = entries
        .iter()
        .filter_map(|(input, entry)| Some((input, archive(entry)?)))
        .collect();
    if !archives.is_empty() {
        fs::create_dir_all(artifacts).operation("create Lake artifact directory")?;
    }
    let outputs =
        storage::parallel_with(storage::ARCHIVE_WORKERS, &archives, |_, (input, cid)| {
            unpack_one(blobs, artifacts, input, cid)
        })?;
    if !archives.is_empty() {
        println!(
            "Lake cache: unpacked {} archives in {:.1} s",
            archives.len(),
            started.elapsed().as_secs_f64()
        );
    }
    let mut rows: BTreeMap<_, _> = entries
        .iter()
        .map(|(input, entry)| (input.clone(), entry.output.clone()))
        .collect();
    for ((input, _), output) in archives.into_iter().zip(outputs) {
        rows.insert(input.clone(), output);
    }
    Ok(rows)
}

fn unpack_one(blobs: &Blobs, artifacts: &Path, input: &str, cid: &Sha256) -> Result<String> {
    let mut bytes = Vec::new();
    blobs
        .get_to(cid, &mut bytes)?
        .ok_or_else(|| invalid(format!("{cid} is missing from the pool")))?;
    let hash = u64::from_str_radix(input, 16).map_err(|_| invalid("invalid Lake input hash"))?;
    let archive = covalence_lean_ltar::unpack(&bytes, hash).map_err(|source| Error::Archive {
        cid: cid.to_string(),
        source,
    })?;
    let stem = archive
        .trace
        .path
        .strip_suffix(".trace")
        .ok_or_else(|| invalid(format!("archive {cid} has no module trace")))?;
    let trace: Value = json(&archive.trace.bytes)?;
    let Some(Value::Object(mut outputs)) = trace.get("outputs").cloned() else {
        return Err(invalid(format!("archive {cid} names no module outputs")));
    };
    // Lake would resolve the archive too; the files are all it needs.
    outputs.remove("l");
    let mut names = Vec::new();
    for value in outputs.values() {
        match value {
            Value::String(name) => names.push(name),
            Value::Array(items) => {
                for item in items {
                    let Value::String(name) = item else {
                        return Err(invalid(format!("archive {cid} names an invalid output")));
                    };
                    names.push(name);
                }
            }
            // `m`, whether the module uses the module system.
            _ => {}
        }
    }
    for name in names {
        let extension = name
            .split_once('.')
            .filter(|_| mapping::artifact_name(name))
            .map(|(_, extension)| extension)
            .ok_or_else(|| invalid(format!("archive {cid} names an invalid output")))?;
        let path = format!("{stem}.{extension}");
        let file = archive
            .files
            .iter()
            .find(|file| file.path == path)
            .ok_or_else(|| invalid(format!("archive {cid} lacks its {extension} output")))?;
        save(&artifacts.join(name), &file.bytes)?;
    }
    serde_json::to_string(&Value::Object(outputs)).map_err(|source| Error::Json {
        operation: "write Lake output descriptor".into(),
        source,
    })
}

/// Writes a content-addressed file once, read-only as Lake leaves the
/// artifacts it downloads; an existing file has the same bytes.
fn save(path: &Path, bytes: &[u8]) -> Result<()> {
    if path.exists() {
        return Ok(());
    }
    let parent = path
        .parent()
        .ok_or_else(|| invalid("artifact has no parent"))?;
    let mut pending = tempfile::NamedTempFile::new_in(parent).operation("stage Lake artifact")?;
    pending.write_all(bytes).operation("write Lake artifact")?;
    let mut permissions = pending
        .as_file()
        .metadata()
        .operation("read Lake artifact permissions")?
        .permissions();
    permissions.set_readonly(true);
    pending
        .as_file()
        .set_permissions(permissions)
        .operation("mark Lake artifact read-only")?;
    match pending.persist_noclobber(path) {
        Ok(_) => Ok(()),
        Err(error) if error.error.kind() == io::ErrorKind::AlreadyExists => Ok(()),
        Err(error) => Err(error.error).operation("save Lake artifact"),
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use covalence_kv_memory::MemoryKv;
    use covalence_kv_trait::Kv;

    use super::*;
    use crate::blobs;

    fn fixture() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../ltar/tests/fixtures/module.ltar")
    }

    #[test]
    fn archives_become_their_outputs_in_the_artifact_directory() {
        let blobs = Blobs::new(MemoryKv::default().into_dyn(), "lake/test/cas");
        let (cid, _) = blobs::hash_file(&fixture()).unwrap();
        blobs.put_file(&cid, &fixture()).unwrap();
        let name = "89abcdef01234567.ltar".to_owned();
        let entries = BTreeMap::from([
            (
                "0123456789abcdef".to_owned(),
                Entry {
                    output: format!("\"{name}\""),
                    artifacts: BTreeMap::from([(name, cid)]),
                },
            ),
            (
                "1123456789abcdef".to_owned(),
                Entry {
                    output: "\"0d9deac573f269f3\"".into(),
                    artifacts: BTreeMap::new(),
                },
            ),
        ]);
        let artifacts = tempfile::tempdir().unwrap();
        let rows = unpack(&blobs, artifacts.path(), &entries).unwrap();
        assert_eq!(rows["1123456789abcdef"], "\"0d9deac573f269f3\"");
        let outputs: Value = serde_json::from_str(&rows["0123456789abcdef"]).unwrap();
        assert_eq!(
            outputs,
            serde_json::json!({
                "m": true,
                "o": ["1111111111111111.olean", "2222222222222222.olean.server",
                      "3333333333333333.olean.private"],
                "i": "4444444444444444.ilean",
                "rs": "5555555555555555.ir.sig",
                "r": "6666666666666666.ir",
                "c": "7777777777777777.c",
            })
        );
        let mut saved: Vec<_> = fs::read_dir(artifacts.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        saved.sort();
        assert_eq!(
            saved,
            [
                "1111111111111111.olean",
                "2222222222222222.olean.server",
                "3333333333333333.olean.private",
                "4444444444444444.ilean",
                "5555555555555555.ir.sig",
                "6666666666666666.ir",
                "7777777777777777.c",
            ]
        );
        let c = artifacts.path().join("7777777777777777.c");
        assert!(fs::metadata(&c).unwrap().permissions().readonly());
        assert_eq!(
            fs::read(&c).unwrap(),
            b"// generated\nint main(void) { return 0; }\n"
        );
        // A second restore keeps the files it finds.
        unpack(&blobs, artifacts.path(), &entries).unwrap();
    }
}

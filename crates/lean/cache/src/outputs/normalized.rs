//! Output equality revision 1: actual constituent bytes and module mode,
//! independent of archive packing, routing, trace stamps and diagnostics.

use std::collections::{BTreeMap, BTreeSet};

use covalence_lean_ltar::{Archive, Dir};
use covalence_lib_cid::Sha256;
use serde_json::Value;
use sha2::Digest;

use super::Entry;
use crate::error::{Error, Result, invalid, json};
use crate::mapping;

const ROLES: [(&str, &str); 5] = [
    ("i", "ilean"),
    ("rs", "ir.sig"),
    ("r", "ir"),
    ("c", "c"),
    ("b", "bc"),
];
const MODULE_KEYS: [&str; 8] = ["m", "o", "i", "rs", "r", "c", "b", "l"];

#[derive(Debug, PartialEq, Eq)]
pub(super) enum Normalized {
    Module {
        mode: bool,
        roles: BTreeMap<String, String>,
    },
    Generic(Value),
}

struct File {
    digest: String,
    archive: Option<Vec<u8>>,
}

fn digest(bytes: &[u8]) -> Result<String> {
    Ok(crate::blobs::cid(&sha2::Sha256::digest(bytes))?.to_string())
}

fn artifact(value: &Value) -> Result<String> {
    match value {
        Value::String(name) if mapping::artifact_name(name) => Ok(name.clone()),
        Value::Number(n) => n
            .as_u64()
            .map(|n| format!("{n:016x}.art"))
            .ok_or_else(|| invalid("artifact hash is not a UInt64")),
        _ => Err(invalid("output role is not an artifact descriptor")),
    }
}

// Lake's ModuleOutputDescrs uses the string-only ArtifactDescr decoder;
// numeric UInt64 references belong only to generic mapping descriptors.
fn module_artifact(value: &Value) -> Result<&str> {
    match value {
        Value::String(name) if mapping::artifact_name(name) => Ok(name),
        _ => Err(invalid("module role is not a string artifact descriptor")),
    }
}

/// Resolve precisely the artifact set selected by mapping::names. That
/// decoder selects the first filename for each UInt64 artifact hash.
pub(super) fn entry(
    entry: &Entry,
    input: &str,
    mut load: impl FnMut(&str) -> Result<Vec<u8>>,
) -> Result<Normalized> {
    let data = entry
        .record
        .get("data")
        .ok_or_else(|| invalid("output record has no data"))?;
    let names = mapping::names(data)?;
    if names.iter().collect::<BTreeSet<_>>() != entry.artifacts.keys().collect::<BTreeSet<_>>() {
        return Err(invalid("output descriptor and artifact list disagree"));
    }
    let mut files = BTreeMap::new();
    for name in names {
        let expected: Sha256 = entry.artifacts[&name]
            .parse()
            .map_err(|_| invalid("output names an invalid CID"))?;
        let bytes = load(&name)?;
        let digest = digest(&bytes)?;
        if digest != expected.to_string() {
            return Err(invalid(format!(
                "artifact {name} does not hash to {expected}"
            )));
        }
        files.insert(
            name[..16].to_owned(),
            File {
                digest,
                archive: name.ends_with(".ltar").then_some(bytes),
            },
        );
    }
    let resolve = |value: &Value| -> Result<&File> {
        let name = artifact(value)?;
        files
            .get(&name[..16])
            .ok_or_else(|| invalid(format!("unresolved artifact {name}")))
    };
    if data.as_str().is_some_and(|name| name.ends_with(".ltar")) {
        return archive(
            archive_bytes(resolve(data)?)?,
            data.as_str().unwrap_or("archive"),
            input,
        );
    }
    if data
        .as_object()
        .is_some_and(|object| MODULE_KEYS.iter().any(|key| object.contains_key(*key)))
    {
        let normalized = module(data, |_, value| Ok(resolve(value)?.digest.clone()))?;
        if let Some(value) = data.get("l") {
            let name = module_artifact(value)?;
            if !name.ends_with(".ltar") {
                return Err(invalid("module archive has no ltar extension"));
            }
            if archive(archive_bytes(resolve(value)?)?, name, input)? != normalized {
                return Err(invalid("module archive contradicts its expanded outputs"));
            }
        }
        return Ok(normalized);
    }
    Ok(Normalized::Generic(generic(data, &resolve)?))
}

/// Generic descriptors follow only mapping::collect: null and booleans are
/// metadata, numbers and strings are artifact references, containers recurse.
/// Preserve object keys and array positions; there is no module fallback.
fn archive_bytes(file: &File) -> Result<&[u8]> {
    file.archive
        .as_deref()
        .ok_or_else(|| invalid("archive descriptor resolves to a non-archive artifact"))
}

fn generic<'a>(value: &Value, resolve: &impl Fn(&Value) -> Result<&'a File>) -> Result<Value> {
    Ok(match value {
        Value::Null | Value::Bool(_) => value.clone(),
        Value::Number(_) | Value::String(_) => {
            let name = artifact(value)?;
            if name.ends_with(".ltar") {
                return Err(invalid("nested archive descriptors are unsupported"));
            }
            Value::String(resolve(value)?.digest.clone())
        }
        Value::Array(values) => Value::Array(
            values
                .iter()
                .map(|value| generic(value, resolve))
                .collect::<Result<_>>()?,
        ),
        Value::Object(values) => {
            if MODULE_KEYS.iter().any(|key| values.contains_key(*key)) {
                return Err(invalid("nested module descriptors are unsupported"));
            }
            Value::Object(
                values
                    .iter()
                    .map(|(key, value)| Ok((key.clone(), generic(value, resolve)?)))
                    .collect::<Result<_>>()?,
            )
        }
    })
}

fn module(
    data: &Value,
    mut resolve: impl FnMut(&str, &Value) -> Result<String>,
) -> Result<Normalized> {
    let object = data
        .as_object()
        .ok_or_else(|| invalid("module outputs are not an object"))?;
    if object
        .keys()
        .any(|key| !MODULE_KEYS.contains(&key.as_str()))
    {
        return Err(invalid("unknown module output field"));
    }
    let oleans = object
        .get("o")
        .and_then(Value::as_array)
        .filter(|parts| (1..=3).contains(&parts.len()))
        .ok_or_else(|| invalid("module requires one to three olean parts"))?;
    let mode = match object.get("m") {
        None => oleans.len() > 1,
        Some(Value::Bool(mode)) => *mode,
        _ => return Err(invalid("module mode is not Boolean")),
    };
    if let Some(value) = object.get("l") {
        module_artifact(value)?;
    }
    let mut roles = BTreeMap::new();
    for (role, value) in ["olean", "olean.server", "olean.private"]
        .into_iter()
        .zip(oleans)
    {
        module_artifact(value)?;
        roles.insert(role.to_owned(), resolve(role, value)?);
    }
    for (key, role) in ROLES {
        match object.get(key) {
            Some(value) => {
                module_artifact(value)?;
                roles.insert(role.to_owned(), resolve(role, value)?);
            }
            None if key == "i" => return Err(invalid("module lacks ilean output")),
            None => {}
        }
    }
    Ok(Normalized::Module { mode, roles })
}

fn archive(bytes: &[u8], name: &str, input: &str) -> Result<Normalized> {
    crate::archives::validate_input(bytes, name, input)?;
    let archive = covalence_lean_ltar::unpack(bytes, 0).map_err(|source| Error::Archive {
        archive: name.to_owned(),
        source,
    })?;
    let trace: Value = json(&archive.trace.bytes)?;
    let data = trace
        .get("outputs")
        .ok_or_else(|| invalid("module archive lacks output descriptors"))?;
    let stem = archive
        .trace
        .path
        .strip_suffix(".trace")
        .ok_or_else(|| invalid("module archive has no module trace"))?;
    let mut declared = BTreeSet::new();
    let normalized = module(data, |role, value| {
        let digest = archive_role(&archive, role, module_artifact(value)?)?;
        declared.insert(format!("{stem}.{role}"));
        Ok(digest)
    })?;
    // Lake extracts every entry, including entries eager placement ignores.
    // Only the exact declared role manifest can establish equal outputs.
    // archive_role already requires unique paths and canonical directories.
    if let Some(file) = archive
        .files
        .iter()
        .find(|file| !declared.contains(&file.path))
    {
        return Err(invalid(format!(
            "module archive has undeclared entry {}",
            file.path
        )));
    }
    Ok(normalized)
}

fn archive_role(archive: &Archive, role: &str, descriptor: &str) -> Result<String> {
    // archives::place selects stem.{descriptor extension}, not stem.{role}.
    // Reject noncanonical role suffixes before equality can certify bytes
    // different from those an actual restore would select.
    let extension = descriptor.split_once('.').map(|(_, extension)| extension);
    if extension != Some(role) {
        return Err(invalid(format!(
            "archive role {role} has noncanonical descriptor {descriptor}"
        )));
    }
    let stem = archive
        .trace
        .path
        .strip_suffix(".trace")
        .ok_or_else(|| invalid("module archive has no module trace"))?;
    let dir = if matches!(role, "c" | "bc") {
        Dir::Ir
    } else {
        Dir::Lib
    };
    let path = format!("{stem}.{role}");
    // Eager placement selects the first matching path regardless of directory,
    // whereas Lake unpacks into directory-specific destinations. Require one
    // unambiguous entry in the expected directory so both consumers agree.
    let mut matches = archive.files.iter().filter(|file| file.path == path);
    let file = matches
        .next()
        .ok_or_else(|| invalid(format!("module archive lacks {role}")))?;
    if matches.next().is_some() {
        return Err(invalid(format!("module archive has ambiguous {role} path")));
    }
    if file.dir != dir {
        return Err(invalid(format!(
            "module archive has wrong directory for {role}"
        )));
    }
    digest(&file.bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (Entry, BTreeMap<String, Vec<u8>>, Vec<u8>) {
        let bytes = include_bytes!("../../../ltar/tests/fixtures/module.ltar").to_vec();
        let archive = covalence_lean_ltar::unpack(&bytes, 0).unwrap();
        let trace: Value = json(&archive.trace.bytes).unwrap();
        let mut data = trace["outputs"].clone();
        data.as_object_mut().unwrap().remove("l");
        let files = mapping::names(&data)
            .unwrap()
            .into_iter()
            .map(|name| {
                let suffix = format!(".{}", name.split_once('.').unwrap().1);
                let bytes = archive
                    .files
                    .iter()
                    .find(|file| file.path.ends_with(&suffix))
                    .unwrap()
                    .bytes
                    .clone();
                (name, bytes)
            })
            .collect::<BTreeMap<_, _>>();
        let entry = make(data, &files);
        (entry, files, bytes)
    }

    fn make(data: Value, files: &BTreeMap<String, Vec<u8>>) -> Entry {
        Entry {
            record: serde_json::json!({"data": data}),
            artifacts: files
                .iter()
                .map(|(name, bytes)| (name.clone(), digest(bytes).unwrap()))
                .collect(),
        }
    }

    fn norm(entry: &Entry, files: &BTreeMap<String, Vec<u8>>) -> Result<Normalized> {
        super::entry(entry, "0123456789abcdef", |name| {
            files
                .get(name)
                .cloned()
                .ok_or_else(|| invalid("missing fixture constituent"))
        })
    }

    fn archive_entry(bytes: Vec<u8>) -> (Entry, BTreeMap<String, Vec<u8>>) {
        let name = "abcdef0123456789.ltar";
        let files = BTreeMap::from([(name.to_owned(), bytes)]);
        (make(Value::String(name.to_owned()), &files), files)
    }

    fn repack(archive: &Archive, trace: &Value) -> Vec<u8> {
        let mut packed = b"LTR4\x00".to_vec();
        let mut append = |dir: Option<Dir>, path: &str, bytes: &[u8]| {
            if let Some(dir) = dir {
                packed.push(if dir == Dir::Lib { 0 } else { 1 });
            }
            packed.extend_from_slice(path.as_bytes());
            packed.push(0);
            packed.push(0);
            let frame = zstd::stream::encode_all(bytes, 3).unwrap();
            packed.extend_from_slice(&(frame.len() as u64).to_le_bytes());
            packed.extend_from_slice(&frame);
        };
        append(
            None,
            &archive.trace.path,
            &serde_json::to_vec(trace).unwrap(),
        );
        for file in &archive.files {
            append(Some(file.dir), &file.path, &file.bytes);
        }
        packed
    }

    #[test]
    fn archives_and_expanded_constituents_agree_but_roles_mode_and_presence_matter() {
        let (expanded, files, bytes) = fixture();
        let expected = norm(&expanded, &files).unwrap();
        let (packed, packed_files) = archive_entry(bytes);
        assert_eq!(norm(&packed, &packed_files).unwrap(), expected);
        let mut inferred_mode = expanded.record["data"].clone();
        inferred_mode.as_object_mut().unwrap().remove("m");
        assert_eq!(
            norm(&make(inferred_mode, &files), &files).unwrap(),
            expected
        );
        let mut changed = expanded.record.clone();
        changed["data"]["m"] = Value::Bool(false);
        assert_ne!(
            norm(
                &Entry {
                    record: changed,
                    artifacts: expanded.artifacts.clone()
                },
                &files
            )
            .unwrap(),
            expected
        );
        let mut changed = expanded.record["data"].clone();
        changed.as_object_mut().unwrap().remove("c");
        let mut fewer_files = files.clone();
        fewer_files.remove(expanded.record["data"]["c"].as_str().unwrap());
        assert_ne!(
            norm(&make(changed, &fewer_files), &fewer_files).unwrap(),
            expected
        );
        let mut swapped = expanded.record["data"].clone();
        swapped["o"].as_array_mut().unwrap().swap(0, 1);
        assert_ne!(norm(&make(swapped, &files), &files).unwrap(), expected);
        let mut changed_files = files.clone();
        changed_files
            .get_mut(expanded.record["data"]["i"].as_str().unwrap())
            .unwrap()
            .push(0);
        assert_ne!(
            norm(
                &make(expanded.record["data"].clone(), &changed_files),
                &changed_files
            )
            .unwrap(),
            expected
        );
    }

    #[test]
    fn archive_packing_logs_and_trace_stamps_are_not_output_identity() {
        let (expanded, files, bytes) = fixture();
        let archive = covalence_lean_ltar::unpack(&bytes, 0).unwrap();
        let mut trace: Value = json(&archive.trace.bytes).unwrap();
        trace["depHash"] = serde_json::json!(12345);
        trace["log"] = serde_json::json!(["different diagnostic"]);
        // Repack every constituent with plain Zstandard, deliberately changing
        // the original olean codec and packing while retaining its real bytes.
        let packed = repack(&archive, &trace);
        assert_ne!(packed, bytes);
        let (entry, packed_files) = archive_entry(packed);
        assert_eq!(
            norm(&entry, &packed_files).unwrap(),
            norm(&expanded, &files).unwrap()
        );
    }

    #[test]
    fn archive_entries_must_exactly_match_declared_roles() {
        let (expanded, files, bytes) = fixture();
        let original = covalence_lean_ltar::unpack(&bytes, 0).unwrap();
        let trace: Value = json(&original.trace.bytes).unwrap();
        let stem = original.trace.path.strip_suffix(".trace").unwrap();
        let (packed, packed_files) = archive_entry(bytes);
        let expected = norm(&expanded, &files).unwrap();
        assert_eq!(norm(&packed, &packed_files).unwrap(), expected);
        // Lazy Lake extraction writes all of these, although eager mapping
        // placement ignores them. Neither consumer may certify equivalence.
        for path in [
            "OtherModule.olean".to_owned(),
            format!("{stem}.unexpected"),
            format!("{stem}.bc"),
        ] {
            for dir in [Dir::Lib, Dir::Ir] {
                let mut archive = original.clone();
                archive.files.push(covalence_lean_ltar::Entry {
                    dir,
                    path: path.clone(),
                    bytes: b"undeclared output".to_vec(),
                });
                let (entry, files) = archive_entry(repack(&archive, &trace));
                assert!(
                    norm(&entry, &files)
                        .unwrap_err()
                        .to_string()
                        .contains("undeclared entry")
                );
            }
        }
        // Declaring an optional role and supplying exactly its canonical entry
        // is supported, and agrees with its expanded representation.
        let name = "9999999999999999.bc";
        let payload = b"declared optional bitcode".to_vec();
        let mut with_optional = original.clone();
        with_optional.files.push(covalence_lean_ltar::Entry {
            dir: Dir::Ir,
            path: format!("{stem}.bc"),
            bytes: payload.clone(),
        });
        let mut optional_trace = trace;
        optional_trace["outputs"]["b"] = Value::String(name.to_owned());
        let mut expanded_data = expanded.record["data"].clone();
        expanded_data["b"] = Value::String(name.to_owned());
        let mut expanded_files = files;
        expanded_files.insert(name.to_owned(), payload);
        let optional_expanded = make(expanded_data, &expanded_files);
        let (entry, files) = archive_entry(repack(&with_optional, &optional_trace));
        let actual = norm(&entry, &files).unwrap();
        assert_eq!(actual, norm(&optional_expanded, &expanded_files).unwrap());
        assert_ne!(actual, expected);
    }

    #[test]
    fn archive_role_paths_reject_wrong_directories_and_ordered_duplicates() {
        let (_, _, bytes) = fixture();
        let original = covalence_lean_ltar::unpack(&bytes, 0).unwrap();
        let trace: Value = json(&original.trace.bytes).unwrap();
        // Cover library and IR outputs: first-path eager placement and
        // directory-specific Lake extraction must select identical bytes.
        for role in ["olean", "c"] {
            let path = format!(
                "{}.{}",
                original.trace.path.strip_suffix(".trace").unwrap(),
                role
            );
            let canonical = original
                .files
                .iter()
                .find(|file| file.path == path)
                .unwrap();
            let mut wrong = canonical.clone();
            wrong.dir = if canonical.dir == Dir::Lib {
                Dir::Ir
            } else {
                Dir::Lib
            };
            wrong.bytes = b"different bytes in wrong directory".to_vec();
            let mut archive = original.clone();
            archive.files.retain(|file| file.path != path);
            archive.files.push(wrong.clone());
            let (entry, files) = archive_entry(repack(&archive, &trace));
            assert!(
                norm(&entry, &files)
                    .unwrap_err()
                    .to_string()
                    .contains("wrong directory")
            );
            // Reject shadowing in either order and same-directory duplicates
            // (both identical and different bytes), rather than pick a winner.
            for duplicate in [wrong, canonical.clone(), {
                let mut different = canonical.clone();
                different.bytes = b"different bytes in same directory".to_vec();
                different
            }] {
                for first in [true, false] {
                    let mut archive = original.clone();
                    if first {
                        archive.files.insert(0, duplicate.clone());
                    } else {
                        archive.files.push(duplicate.clone());
                    }
                    let (entry, files) = archive_entry(repack(&archive, &trace));
                    assert!(
                        norm(&entry, &files)
                            .unwrap_err()
                            .to_string()
                            .contains("ambiguous")
                    );
                }
            }
        }
    }

    #[test]
    fn archive_descriptor_suffixes_must_match_the_bytes_restore_would_select() {
        let (expanded, files, bytes) = fixture();
        assert!(norm(&expanded, &files).is_ok());
        let mut archive = covalence_lean_ltar::unpack(&bytes, 0).unwrap();
        let trace: Value = json(&archive.trace.bytes).unwrap();
        let mut swapped = trace.clone();
        let olean = swapped["outputs"]["o"][0].clone();
        swapped["outputs"]["o"][0] = swapped["outputs"]["i"].clone();
        swapped["outputs"]["i"] = olean;
        let (entry, files) = archive_entry(repack(&archive, &swapped));
        assert!(
            norm(&entry, &files)
                .unwrap_err()
                .to_string()
                .contains("noncanonical descriptor")
        );
        let mut missing_suffix = trace.clone();
        missing_suffix["outputs"]["o"][0] = serde_json::json!("1111111111111111.absent");
        assert!(
            !archive
                .files
                .iter()
                .any(|file| file.path.ends_with(".absent"))
        );
        let (entry, files) = archive_entry(repack(&archive, &missing_suffix));
        assert!(
            norm(&entry, &files)
                .unwrap_err()
                .to_string()
                .contains("noncanonical descriptor")
        );
        // A canonical descriptor whose selected actual file is absent also
        // cannot be accepted merely because another role's file is present.
        archive.files.retain(|file| !file.path.ends_with(".olean"));
        let (entry, files) = archive_entry(repack(&archive, &trace));
        assert!(
            norm(&entry, &files)
                .unwrap_err()
                .to_string()
                .contains("lacks olean")
        );
    }

    #[test]
    fn module_descriptors_reject_numbers_while_generic_mappings_keep_them() {
        let files = BTreeMap::from([
            ("0000000000000001.art".to_owned(), b"olean bytes".to_vec()),
            ("0000000000000002.art".to_owned(), b"ilean bytes".to_vec()),
        ]);
        let numeric = make(serde_json::json!({"o": [1], "i": 2}), &files);
        assert!(
            norm(&numeric, &files)
                .unwrap_err()
                .to_string()
                .contains("string artifact descriptor")
        );
        let generic = make(serde_json::json!([1, 2]), &files);
        assert!(norm(&generic, &files).is_ok());
        let valid =
            serde_json::json!({"o": ["0000000000000001.olean"], "i": "0000000000000002.ilean"});
        for key in ["i", "rs", "r", "c", "b", "l"] {
            let mut numeric = valid.clone();
            numeric[key] = serde_json::json!(3);
            assert!(
                module(&numeric, |_, _| Ok("unused".to_owned())).is_err(),
                "{key}"
            );
        }
    }

    #[test]
    fn malformed_missing_and_contradictory_archives_never_compare_equal() {
        let (mut expanded, mut files, bytes) = fixture();
        let archive_name = "abcdef0123456789.ltar".to_owned();
        expanded.record["data"]["l"] = Value::String(archive_name.clone());
        files.insert(archive_name.clone(), bytes.clone());
        let mut both = make(expanded.record["data"].clone(), &files);
        assert!(norm(&both, &files).is_ok());
        let mut missing = files.clone();
        missing.remove(&archive_name);
        assert!(norm(&both, &missing).is_err());
        files.insert(archive_name.clone(), b"corrupt".to_vec());
        both = make(both.record["data"].clone(), &files);
        assert!(norm(&both, &files).is_err());
        files.insert(archive_name, bytes);
        both = make(both.record["data"].clone(), &files);
        both.record["data"]["m"] = Value::Bool(false);
        assert!(norm(&both, &files).is_err());
        for data in [
            serde_json::json!({"o": []}),
            serde_json::json!({"m": true}),
            serde_json::json!({"o": [], "unknown": true}),
        ] {
            let empty = BTreeMap::new();
            assert!(norm(&make(data, &empty), &empty).is_err());
        }
    }

    #[test]
    fn generic_mapping_shapes_preserve_metadata_and_resolve_artifact_bytes() {
        let files = BTreeMap::from([("0000000000000001.art".to_owned(), b"actual bytes".to_vec())]);
        let left = make(
            serde_json::json!({"metadata": true, "nested": [null, 1]}),
            &files,
        );
        let right = make(
            serde_json::json!({"metadata": true, "nested": [null, "0000000000000001.art"]}),
            &files,
        );
        assert_eq!(norm(&left, &files).unwrap(), norm(&right, &files).unwrap());
        let mut changed = right;
        changed.record["data"]["metadata"] = Value::Bool(false);
        assert_ne!(
            norm(&left, &files).unwrap(),
            norm(&changed, &files).unwrap()
        );
        assert!(
            norm(
                &make(serde_json::json!({"nested": {"m": true}}), &BTreeMap::new()),
                &BTreeMap::new()
            )
            .is_err()
        );
        let archive_files = BTreeMap::from([(
            "0000000000000001.ltar".to_owned(),
            b"unresolved archive".to_vec(),
        )]);
        assert!(
            norm(
                &make(serde_json::json!(["0000000000000001.ltar"]), &archive_files),
                &archive_files,
            )
            .is_err()
        );
    }
}

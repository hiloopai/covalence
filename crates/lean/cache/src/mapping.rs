use std::{
    collections::BTreeMap,
    fs::File,
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
};

use serde_json::Value;

use crate::error::{IoResult, Result, invalid, json};

pub(crate) fn hex(value: &str, length: usize) -> bool {
    value.len() == length
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Whether `name` is a Lake artifact file name: 16 hex digits, optionally
/// followed by `.` and an extension of path-safe characters.
pub(crate) fn artifact_name(name: &str) -> bool {
    let (hash, extension) = name.split_once('.').unwrap_or((name, ""));
    hex(hash, 16)
        && !name.ends_with('.')
        && extension
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
}

pub(crate) fn segment_path(value: &str) -> Result<()> {
    if value.split('/').any(|s| {
        s.is_empty()
            || s == "."
            || s == ".."
            || !s
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
    }) {
        return Err(invalid(
            "cache names must contain nonempty ASCII path segments",
        ));
    }
    Ok(())
}

fn collect(value: &Value, files: &mut BTreeMap<String, String>) -> Result<()> {
    let name = match value {
        Value::Null | Value::Bool(_) => return Ok(()),
        Value::Number(n) => format!(
            "{:016x}.art",
            n.as_u64()
                .ok_or_else(|| invalid("artifact hash is not a UInt64"))?
        ),
        Value::String(s) => {
            if !artifact_name(s) {
                return Err(invalid("invalid artifact filename"));
            }
            s.clone()
        }
        Value::Array(values) => {
            for child in values {
                collect(child, files)?;
            }
            return Ok(());
        }
        Value::Object(values) => {
            for child in values.values() {
                collect(child, files)?;
            }
            return Ok(());
        }
    };
    files.entry(format!("{}.art", &name[..16])).or_insert(name);
    Ok(())
}

/// The schema of the Lake mapping files this tool reads and writes.
pub(crate) const SCHEMA: &str = "2026-03-17";

/// Each row of a Lake mapping: its input hash, its output descriptor as JSON,
/// and the file names of the artifacts the descriptor names.
pub(crate) fn rows(mapping: &Path) -> Result<BTreeMap<String, (String, Vec<String>)>> {
    let mut lines =
        BufReader::new(File::open(mapping).operation("open Lake output mappings")?).lines();
    let header = lines
        .next()
        .ok_or_else(|| invalid("empty Lake mapping"))?
        .operation("read mapping header")?;
    if json::<Value>(header.as_bytes())? != SCHEMA {
        return Err(invalid("unsupported Lake mapping schema"));
    }
    let mut rows = BTreeMap::new();
    for line in lines {
        let line = line.operation("read mapping entry")?;
        let (input, output): (String, Value) = json(line.as_bytes())?;
        let mut files = BTreeMap::new();
        collect(&output, &mut files)?;
        let output =
            serde_json::to_string(&output).map_err(|source| crate::error::Error::Json {
                operation: "write Lake output descriptor".into(),
                source,
            })?;
        if !hex(&input, 16)
            || rows
                .insert(input, (output, files.into_values().collect()))
                .is_some()
        {
            return Err(invalid("invalid or duplicate Lake input mapping"));
        }
    }
    Ok(rows)
}

/// Write `rows` of input hash and output descriptor as a Lake mapping file.
pub(crate) fn write<'a>(
    path: &Path,
    rows: impl IntoIterator<Item = (&'a String, &'a String)>,
) -> Result<()> {
    let mut text = format!("\"{SCHEMA}\"\n");
    for (input, output) in rows {
        text.push_str(&format!("[\"{input}\",{output}]\n"));
    }
    std::fs::write(path, text).operation("write Lake mapping")
}

/// The artifact files a Lake mapping names, which must exist in `directory`.
pub(crate) fn artifacts(mapping: &Path, directory: &Path) -> Result<BTreeMap<String, PathBuf>> {
    rows(mapping)?
        .into_values()
        .flat_map(|(_, names)| names)
        .map(|name| {
            let path = directory.join(&name);
            if !path
                .metadata()
                .operation(format!("read artifact {}", path.display()))?
                .is_file()
            {
                return Err(invalid("artifact is not a file"));
            }
            Ok((name, path))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn nested_outputs_and_numeric_hashes_have_one_remote_key_per_hash() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("0000000000000001.art"), b"x").unwrap();
        fs::write(dir.path().join("0000000000000002.ltar"), b"y").unwrap();
        let mapping = dir.path().join("map");
        fs::write(&mapping, "\"2026-03-17\"\n[\"0000000000000000\",{\"metadata\":true,\"nested\":[null,1,\"0000000000000002.ltar\",\"0000000000000002.other\"]}]\n").unwrap();
        let files = artifacts(&mapping, dir.path()).unwrap();
        assert_eq!(files.len(), 2);
        assert!(files.contains_key("0000000000000002.ltar"));
        let rows = rows(&mapping).unwrap();
        let copy = dir.path().join("copy");
        write(
            &copy,
            rows.iter().map(|(input, (output, _))| (input, output)),
        )
        .unwrap();
        assert_eq!(super::rows(&copy).unwrap(), rows);
    }

    #[test]
    fn malformed_or_escaping_outputs_and_duplicate_inputs_fail() {
        let dir = tempfile::tempdir().unwrap();
        let mapping = dir.path().join("map");
        for value in [
            "-1",
            "18446744073709551616",
            "1.1",
            "\"../../outside\"",
            "\"0000000000000001.../../outside\"",
        ] {
            fs::write(
                &mapping,
                format!("\"2026-03-17\"\n[\"0000000000000000\",{value}]\n"),
            )
            .unwrap();
            assert!(artifacts(&mapping, dir.path()).is_err());
        }
        fs::write(
            &mapping,
            "\"2026-03-17\"\n[\"0000000000000000\",null]\n[\"0000000000000000\",false]\n",
        )
        .unwrap();
        assert!(artifacts(&mapping, dir.path()).is_err());
        for scope in ["../scope", "a//b", "a/./b", "a?secret", "a%2fb", ""] {
            assert!(segment_path(scope).is_err());
        }
    }
}

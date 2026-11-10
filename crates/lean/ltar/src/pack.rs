//! Packing a module's files into an `LTR4` archive.
//!
//! Copied from digama0/leangz `src/ltar.rs` (`pack`, `pack_zstd` and the
//! trace types) at the commit `lib.rs` records.
//! Modified by hiloop: packs from memory; always strips the trace's
//! `depHash` (`leantar -s`, as Lake packs) and so always writes `LTR4`;
//! reads only Lake's `2025-09-10` trace schema and rejects older traces;
//! comments are not supported; output descriptors that would not serialize
//! back to the same text stay opaque JSON; a file is stored as a bare
//! decimal hash only when it is exactly the decimal text of one; a module's
//! `.olean` parts that are not oleans are an error instead of being dropped.
//! License: Apache-2.0, full text in LICENSE-leangz

use std::{io::Write, sync::LazyLock};

use covalence_lib_error::snafu::ResultExt;
use serde_json::Value;
use zstd::dict::EncoderDictionary;

use crate::{
    Archive, Dir, HASH_PLAIN, HASH0_OUTPUT, JsonSnafu, LGZ, LGZ_MODULE, OLEAN_EXTENSIONS,
    OUTPUT_BC, OUTPUT_END, OUTPUT_IR, OUTPUT_IR_SIG, OUTPUT_LTAR, OUTPUT_MODULE_NO,
    OUTPUT_MODULE_YES, OUTPUT_OLEAN, Result, TRACE_SCHEMA, ZSTD, ZstdSnafu, lgz, malformed,
    unsupported,
};

const LEVEL: i32 = 19;

static DICTIONARY: LazyLock<EncoderDictionary<'static>> =
    LazyLock::new(|| EncoderDictionary::copy(crate::DICTIONARY_BYTES, LEVEL));

/// Packs `archive` as `leantar -s` does: the trace without its `depHash`,
/// then each file, a module's three `.olean` parts as one `lgz` stream.
///
/// [`unpack`](crate::unpack) with Lake's input hash gives back the files and
/// the trace with that hash; the trace keeps only its log messages at level
/// `info` and above and its outputs.
///
/// # Errors
///
/// - [`Error::Unsupported`](crate::Error::Unsupported) for a trace without
///   Lake's `2025-09-10` schema, an olean other than version 2, or a trace
///   outside [`Dir::Lib`].
/// - [`Error::Malformed`](crate::Error::Malformed) for an olean whose objects
///   do not parse, and for a trace that is not a Lake trace.
pub fn pack(archive: &Archive) -> Result<Vec<u8>> {
    if archive.trace.dir != Dir::Lib {
        return Err(unsupported("trace directory", 1));
    }
    let mut out = b"LTR4\0".to_vec();
    write_name(&mut out, &archive.trace.path)?;
    trace(&mut out, &archive.trace.bytes)?;
    let mut files = archive.files.iter();
    while let Some(file) = files.next() {
        out.push(index(file.dir));
        write_name(&mut out, &file.path)?;
        let mut parts = vec![file];
        if file.path.ends_with(".olean") {
            let mut rest = files.clone();
            if let (Some(server), Some(private)) = (rest.next(), rest.next())
                && server.path.ends_with(".olean.server")
                && private.path.ends_with(".olean.private")
            {
                parts.extend([server, private]);
                files = rest;
            }
        }
        if file.path.ends_with(".olean") && file.bytes.starts_with(b"olean") {
            out.push(if parts.len() == 1 { LGZ } else { LGZ_MODULE });
            for part in &parts[1..] {
                out.push(index(part.dir));
                write_name(&mut out, &part.path)?;
            }
            let oleans: Vec<&[u8]> = parts.iter().map(|part| &part.bytes[..]).collect();
            let stream = lgz::compress(&oleans)?;
            let mut encoder =
                zstd::stream::Encoder::with_prepared_dictionary(Vec::new(), &DICTIONARY)
                    .context(ZstdSnafu { what: "an olean" })?;
            encoder
                .write_all(&stream)
                .context(ZstdSnafu { what: "an olean" })?;
            sized(
                &mut out,
                &encoder.finish().context(ZstdSnafu { what: "an olean" })?,
            );
        } else if parts.len() > 1 {
            return Err(malformed("a module's .olean is not an olean"));
        } else if let Some(hash) = decimal(&file.bytes) {
            out.push(HASH_PLAIN);
            out.extend_from_slice(&hash.to_le_bytes());
        } else {
            zstd_entry(&mut out, &file.bytes)?;
        }
    }
    Ok(out)
}

fn index(dir: Dir) -> u8 {
    match dir {
        Dir::Lib => 0,
        Dir::Ir => 1,
    }
}

fn write_name(out: &mut Vec<u8>, path: &str) -> Result<()> {
    crate::relative(path.as_bytes())?;
    out.extend_from_slice(path.as_bytes());
    out.push(0);
    Ok(())
}

fn sized(out: &mut Vec<u8>, bytes: &[u8]) {
    out.extend_from_slice(&(bytes.len() as u64).to_le_bytes());
    out.extend_from_slice(bytes);
}

fn zstd_entry(out: &mut Vec<u8>, bytes: &[u8]) -> Result<()> {
    out.push(ZSTD);
    let mut encoder =
        zstd::stream::Encoder::new(Vec::new(), LEVEL).context(ZstdSnafu { what: "a file" })?;
    encoder
        .write_all(bytes)
        .context(ZstdSnafu { what: "a file" })?;
    sized(
        out,
        &encoder.finish().context(ZstdSnafu { what: "a file" })?,
    );
    Ok(())
}

/// The hash a file holds as bare decimal text, if its bytes are exactly that.
fn decimal(bytes: &[u8]) -> Option<u64> {
    let text = std::str::from_utf8(bytes)
        .ok()
        .filter(|text| text.len() <= 20)?;
    let hash: u64 = text.parse().ok()?;
    (hash.to_string() == text).then_some(hash)
}

/// A Lake artifact descriptor, `<16 lowercase hex digits>.<extension>`.
fn descr(value: &Value) -> Option<(u64, &str)> {
    let text = value.as_str()?;
    let (hash, extension) = text.split_at_checked(16)?;
    let extension = extension.strip_prefix('.')?;
    hash.bytes()
        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        .then(|| Some((u64::from_str_radix(hash, 16).ok()?, extension)))
        .flatten()
}

/// A module's outputs: `(name, hash, extension)` in Lake's field order, and
/// whether it uses the module system, if they have the shape `leantar`
/// stores compactly.
type Outputs<'a> = (Option<bool>, Vec<(&'static str, u64, &'a str)>);

fn module_outputs(value: &Value) -> Option<Outputs<'_>> {
    let object = value.as_object()?;
    const NAMES: [&str; 8] = ["m", "o", "i", "rs", "r", "c", "b", "l"];
    if object.keys().any(|name| !NAMES.contains(&name.as_str())) {
        return None;
    }
    let module = match object.get("m") {
        None => None,
        Some(value) => Some(value.as_bool()?),
    };
    let oleans = object.get("o")?.as_array()?;
    if oleans.is_empty() || oleans.len() > OLEAN_EXTENSIONS.len() {
        return None;
    }
    let mut fields = Vec::new();
    for olean in oleans {
        let (hash, extension) = descr(olean)?;
        fields.push(("o", hash, extension));
    }
    for name in ["i", "rs", "r", "c", "b", "l"] {
        match object.get(name) {
            Some(value) => {
                let (hash, extension) = descr(value)?;
                fields.push((name, hash, extension));
            }
            None if matches!(name, "i" | "c") => return None,
            None => {}
        }
    }
    Some((module, fields))
}

/// Whether `leantar` stores the outputs as compact hashes: every extension
/// is the one its name implies.
fn compact(outputs: &Outputs<'_>) -> bool {
    let mut oleans = OLEAN_EXTENSIONS.iter();
    outputs.1.iter().all(|&(name, _, extension)| match name {
        "o" => oleans.next() == Some(&extension),
        "i" => extension == "ilean",
        "rs" => extension == "ir.sig",
        "r" => extension == "ir",
        "c" => extension == "c",
        "b" => extension == "bc",
        _ => extension == "ltar",
    })
}

/// Writes the trace entry: compact output hashes when the trace has no log
/// and module outputs of the usual shape, else the trace as Zstandard JSON.
fn trace(out: &mut Vec<u8>, json: &[u8]) -> Result<()> {
    let value: Value = serde_json::from_slice(json).context(JsonSnafu)?;
    let Some(object) = value
        .as_object()
        .filter(|o| o.get("schemaVersion").and_then(Value::as_str) == Some(TRACE_SCHEMA))
    else {
        return Err(unsupported("trace schema", 0));
    };
    let mut log = Vec::new();
    for message in object
        .get("log")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let (Some(text), Some(level)) = (
            message.get("message").and_then(Value::as_str),
            message.get("level").and_then(Value::as_str),
        ) else {
            return Err(malformed("trace log message"));
        };
        match level {
            "trace" => {}
            "info" | "warning" | "error" => log.push((text, level)),
            _ => return Err(malformed("trace log level")),
        }
    }
    let outputs = object.get("outputs").filter(|value| !value.is_null());
    let module = outputs.and_then(module_outputs);
    if log.is_empty()
        && let Some(module) = module.as_ref().filter(|module| compact(module))
    {
        out.push(HASH0_OUTPUT);
        let hash = |name| {
            module
                .1
                .iter()
                .find(|field| field.0 == name)
                .map(|field| field.1)
        };
        for name in ["o", "i", "c"] {
            out.extend_from_slice(&hash(name).unwrap_or_default().to_le_bytes());
        }
        for &(_, hash, _) in module.1.iter().filter(|field| field.0 == "o").skip(1) {
            out.push(OUTPUT_OLEAN);
            out.extend_from_slice(&hash.to_le_bytes());
        }
        for (name, code) in [
            ("rs", OUTPUT_IR_SIG),
            ("r", OUTPUT_IR),
            ("b", OUTPUT_BC),
            ("l", OUTPUT_LTAR),
        ] {
            if let Some(hash) = hash(name) {
                out.push(code);
                out.extend_from_slice(&hash.to_le_bytes());
            }
        }
        match module.0 {
            Some(true) => out.push(OUTPUT_MODULE_YES),
            Some(false) => out.push(OUTPUT_MODULE_NO),
            None => {}
        }
        out.push(OUTPUT_END);
        return Ok(());
    }
    // `schemaVersion`, `log` and `outputs`, in that order, as `leantar`
    // serializes its trace type; other fields are dropped.
    let mut text = format!(r#"{{"schemaVersion":"{TRACE_SCHEMA}""#);
    if !log.is_empty() {
        let messages: Vec<String> = log
            .iter()
            .map(|(message, level)| {
                serde_json::to_string(message)
                    .map(|message| format!(r#"{{"message":{message},"level":"{level}"}}"#))
            })
            .collect::<Result<_, _>>()
            .context(JsonSnafu)?;
        text.push_str(&format!(r#","log":[{}]"#, messages.join(",")));
    }
    if let Some(outputs) = outputs {
        let outputs = match module {
            Some((module, fields)) => {
                let mut object = String::new();
                if let Some(module) = module {
                    object.push_str(&format!(r#""m":{module},"#));
                }
                let oleans: Vec<String> = fields
                    .iter()
                    .filter(|field| field.0 == "o")
                    .map(|&(_, hash, extension)| format!(r#""{hash:016x}.{extension}""#))
                    .collect();
                object.push_str(&format!(r#""o":[{}]"#, oleans.join(",")));
                for &(name, hash, extension) in fields.iter().filter(|field| field.0 != "o") {
                    object.push_str(&format!(r#","{name}":"{hash:016x}.{extension}""#));
                }
                format!("{{{object}}}")
            }
            None => {
                let mut value = outputs.clone();
                value.sort_all_objects();
                serde_json::to_string(&value).context(JsonSnafu)?
            }
        };
        text.push_str(&format!(r#","outputs":{outputs}"#));
    }
    text.push('}');
    zstd_entry(out, text.as_bytes())
}

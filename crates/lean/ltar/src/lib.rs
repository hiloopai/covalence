// The archive layout and its trace handling are copied from digama0/leangz
// `src/ltar.rs` (`unpack`, `unpack_one`) at commit
// 33570f263222fe8aa4e071f4a30b1b69bfc0f4e3, merged with the head of
// https://github.com/digama0/leangz/pull/22 (b15180b547e17bda933b7c7706db1a1ca611fcc8);
// the merged tree is df689da9d52269452b562ef19b25bdcb7de7b47d. `v1.dict` is
// upstream's `dict/v1.dict`, byte for byte.
// SPDX-License-Identifier: Apache-2.0
// Copyright: upstream states no copyright line beyond its licence; authors are
// Mario Carneiro and the leangz contributors. Upstream ships no NOTICE file.
// Divergence: tracked, as `lgz.rs` records.
// Modified by hiloop: decodes `LTR4` archives in memory instead of writing a
// directory; requires the input hash, as Lake supplies it; rejects truncated
// names, unsafe paths and archives that name a third base directory; trace
// JSON keys are sorted explicitly instead of by serde_json's default map.
// License: Apache-2.0, full text in LICENSE-leangz
#![doc = include_str!("../README.md")]
#![deny(missing_docs)]

mod lgz;

use std::{
    io::{self, Read},
    sync::LazyLock,
};

use covalence_lib_error::snafu::{ResultExt, Snafu};
use zstd::dict::DecoderDictionary;

/// Lake's trace schema whose `depHash` an archive packed with `-s` omits.
const TRACE_SCHEMA: &str = "2025-09-10";
const OLEAN_EXTENSIONS: [&str; 3] = ["olean", "olean.server", "olean.private"];

const ZSTD: u8 = 0;
const LGZ: u8 = 1;
const HASH_PLAIN: u8 = 2;
const HASH_JSON: u8 = 3;
const HASH_OUTPUT: u8 = 4;
const LGZ_MODULE: u8 = 5;
const HASH0_OUTPUT: u8 = 6;

const OUTPUT_END: u8 = 0;
const OUTPUT_OLEAN: u8 = 1;
const OUTPUT_BC: u8 = 2;
const OUTPUT_IR: u8 = 3;
const OUTPUT_IR_SIG: u8 = 4;
const OUTPUT_LTAR: u8 = 5;
const OUTPUT_MODULE_NO: u8 = 6;
const OUTPUT_MODULE_YES: u8 = 7;

static DICTIONARY: LazyLock<DecoderDictionary<'static>> =
    LazyLock::new(|| DecoderDictionary::copy(include_bytes!("v1.dict")));

/// A malformed or unsupported archive.
#[derive(Debug, Snafu)]
#[snafu(crate_root(covalence_lib_error::snafu))]
pub enum Error {
    /// The archive ends inside an entry.
    #[snafu(display("the archive ends early"))]
    Truncated,
    /// The archive breaks the format.
    #[snafu(display("malformed archive: {reason}"))]
    Malformed {
        /// What is wrong.
        reason: &'static str,
    },
    /// The archive uses a version or code this decoder does not support.
    #[snafu(display("unsupported {what} {value:#x}"))]
    Unsupported {
        /// The kind of value.
        what: &'static str,
        /// The value.
        value: u64,
    },
    /// A Zstandard frame does not decode.
    #[snafu(display("decompress {what}: {source}"))]
    Zstd {
        /// The entry being decompressed.
        what: &'static str,
        /// The decoder's error.
        source: io::Error,
    },
    /// The trace is not JSON.
    #[snafu(display("read the trace: {source}"))]
    Json {
        /// The parser's error.
        source: serde_json::Error,
    },
}

/// A result with this crate's [`Error`].
pub type Result<T, E = Error> = std::result::Result<T, E>;

fn malformed(reason: &'static str) -> Error {
    Error::Malformed { reason }
}

fn unsupported(what: &'static str, value: u64) -> Error {
    Error::Unsupported { what, value }
}

/// Lake's base directory for an unpacked file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dir {
    /// The library directory (`-C` index 0): traces, `.olean`, `.ilean` and IR.
    Lib,
    /// The IR directory (`-C` index 1): `.c` and `.bc`.
    Ir,
}

/// One unpacked file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    /// The base directory the path is relative to.
    pub dir: Dir,
    /// The relative path, with `/` separators and no `.` or `..` components.
    pub path: String,
    /// The contents.
    pub bytes: Vec<u8>,
}

/// An unpacked archive.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Archive {
    /// The trace file, which is relative to [`Dir::Lib`].
    pub trace: Entry,
    /// The other files, in archive order.
    pub files: Vec<Entry>,
}

/// Unpacks an `LTR4` archive as `leantar -x` does, with `input_hash` as the
/// trace's `depHash`.
///
/// Lake packs module archives with `-s`, which drops the hash, and supplies
/// its input hash on unpack; this function always uses `input_hash`, as
/// `leantar` does when given one. Comments are skipped.
///
/// # Errors
///
/// - [`Error::Truncated`] when the archive ends inside an entry.
/// - [`Error::Unsupported`] for other archive or `lgz` versions and unknown
///   compression codes or opcodes.
/// - [`Error::Malformed`], [`Error::Zstd`] or [`Error::Json`] for corrupt
///   entries, and for paths that are absolute or leave their base directory.
pub fn unpack(archive: &[u8], input_hash: u64) -> Result<Archive> {
    let mut input = Input::new(archive);
    let magic = input.array::<4>()?;
    if &magic != b"LTR4" {
        return Err(unsupported(
            "archive magic",
            u32::from_be_bytes(magic).into(),
        ));
    }
    match input.u8()? {
        0 => {}
        1 => {
            input.u64()?;
        }
        flag => return Err(unsupported("depHash flag", flag.into())),
    }
    let path = relative(input.cstr()?)?;
    let compression = input.u8()?;
    if matches!(compression, LGZ | LGZ_MODULE) {
        return Err(malformed("the trace is an olean"));
    }
    let [bytes] = decode(&mut input, compression, Some(input_hash))?
        .try_into()
        .map_err(|_| malformed("the trace is several files"))?;
    let trace = Entry {
        dir: Dir::Lib,
        path,
        bytes,
    };
    let mut files = Vec::new();
    while !input.is_empty() {
        let index = input.u8()?;
        let name = input.cstr()?;
        if name.is_empty() {
            // A comment.
            input.cstr()?;
            continue;
        }
        let mut paths = vec![(dir(index)?, relative(name)?)];
        let compression = input.u8()?;
        if compression == LGZ_MODULE {
            for _ in 0..2 {
                let index = input.u8()?;
                paths.push((dir(index)?, relative(input.cstr()?)?));
            }
        }
        let contents = decode(&mut input, compression, None)?;
        files.extend(
            paths
                .into_iter()
                .zip(contents)
                .map(|((dir, path), bytes)| Entry { dir, path, bytes }),
        );
    }
    Ok(Archive { trace, files })
}

fn dir(index: u8) -> Result<Dir> {
    match index {
        0 => Ok(Dir::Lib),
        1 => Ok(Dir::Ir),
        _ => Err(unsupported("base directory", index.into())),
    }
}

/// A relative path that stays inside its base directory.
fn relative(name: &[u8]) -> Result<String> {
    let path = std::str::from_utf8(name).map_err(|_| malformed("path is not UTF-8"))?;
    if path.is_empty()
        || path.starts_with('/')
        || path.split('/').any(|part| matches!(part, "" | "." | ".."))
    {
        return Err(malformed("path leaves its base directory"));
    }
    Ok(path.to_owned())
}

/// Decodes one entry's contents: one file, or the three `.olean` parts of a
/// module. `trace_hash` is the hash to write into a trace.
fn decode(input: &mut Input<'_>, compression: u8, trace_hash: Option<u64>) -> Result<Vec<Vec<u8>>> {
    Ok(match compression {
        ZSTD => {
            let size = input.size()?;
            let bytes = zstd::stream::decode_all(input.take(size)?)
                .context(ZstdSnafu { what: "a file" })?;
            vec![match trace_hash {
                Some(hash) => with_dep_hash(&bytes, hash)?,
                None => bytes,
            }]
        }
        LGZ | LGZ_MODULE => {
            let size = input.size()?;
            let mut stream = Vec::new();
            zstd::stream::Decoder::with_prepared_dictionary(input.take(size)?, &DICTIONARY)
                .and_then(|mut decoder| decoder.read_to_end(&mut stream))
                .context(ZstdSnafu { what: "an olean" })?;
            lgz::decompress(&stream, if compression == LGZ { 1 } else { 3 })?
        }
        HASH_PLAIN => {
            let stored = input.u64()?;
            vec![trace_hash.unwrap_or(stored).to_string().into_bytes()]
        }
        HASH_JSON => {
            let stored = input.u64()?;
            let hash = trace_hash.unwrap_or(stored);
            vec![format!(r#"{{"depHash":"{hash}"}}"#).into_bytes()]
        }
        HASH_OUTPUT | HASH0_OUTPUT => {
            let stored = if compression == HASH_OUTPUT {
                Some(input.u64()?)
            } else {
                None
            };
            vec![outputs_trace(input, trace_hash.or(stored))?]
        }
        _ => return Err(unsupported("compression", compression.into())),
    })
}

/// Splices `hash` into a trace that `-s` stripped, as `leantar` does: the
/// JSON is rewritten compactly with sorted keys, and gains `depHash` when it
/// has Lake's trace schema.
fn with_dep_hash(json: &[u8], hash: u64) -> Result<Vec<u8>> {
    let mut value: serde_json::Value = serde_json::from_slice(json).context(JsonSnafu)?;
    if let Some(object) = value
        .as_object_mut()
        .filter(|o| o.get("schemaVersion").and_then(|v| v.as_str()) == Some(TRACE_SCHEMA))
    {
        object.insert("depHash".into(), format!("{hash:016x}").into());
    }
    // `leantar` sorts keys because serde_json maps are ordered by default;
    // another crate in the build may enable `preserve_order`.
    value.sort_all_objects();
    serde_json::to_vec(&value).context(JsonSnafu)
}

/// Reads the compact output hashes of a module trace and writes the trace
/// JSON that `leantar` writes for them.
fn outputs_trace(input: &mut Input<'_>, hash: Option<u64>) -> Result<Vec<u8>> {
    let descr = |hash: u64, extension: &str| format!(r#""{hash:016x}.{extension}""#);
    let mut oleans = vec![descr(input.u64()?, OLEAN_EXTENSIONS[0])];
    let ilean = descr(input.u64()?, "ilean");
    let c = descr(input.u64()?, "c");
    let (mut module, mut ir_sig, mut ir, mut bc, mut ltar) = (None, None, None, None, None);
    loop {
        let (slot, value) = match input.u8()? {
            OUTPUT_END => break,
            OUTPUT_OLEAN => {
                let extension = OLEAN_EXTENSIONS
                    .get(oleans.len())
                    .ok_or_else(|| malformed("too many olean outputs"))?;
                oleans.push(descr(input.u64()?, extension));
                continue;
            }
            OUTPUT_IR_SIG => (&mut ir_sig, descr(input.u64()?, "ir.sig")),
            OUTPUT_IR => (&mut ir, descr(input.u64()?, "ir")),
            OUTPUT_BC => (&mut bc, descr(input.u64()?, "bc")),
            OUTPUT_LTAR => (&mut ltar, descr(input.u64()?, "ltar")),
            OUTPUT_MODULE_YES => (&mut module, "true".to_owned()),
            OUTPUT_MODULE_NO => (&mut module, "false".to_owned()),
            code => return Err(unsupported("output code", code.into())),
        };
        if slot.replace(value).is_some() {
            return Err(malformed("repeated output"));
        }
    }
    let mut outputs = Vec::new();
    let mut field = |key: &str, value: Option<String>| {
        if let Some(value) = value {
            outputs.push(format!(r#""{key}":{value}"#));
        }
    };
    field("m", module);
    field("o", Some(format!("[{}]", oleans.join(","))));
    field("i", Some(ilean));
    field("rs", ir_sig);
    field("r", ir);
    field("c", Some(c));
    field("b", bc);
    field("l", ltar);
    let dep_hash = hash.map_or(String::new(), |hash| format!(r#""depHash":"{hash:016x}","#));
    Ok(format!(
        r#"{{"schemaVersion":"{TRACE_SCHEMA}",{dep_hash}"outputs":{{{}}}}}"#,
        outputs.join(",")
    )
    .into_bytes())
}

/// Little-endian reads from a byte slice.
struct Input<'a> {
    bytes: &'a [u8],
}

impl<'a> Input<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes }
    }

    fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        if n > self.bytes.len() {
            return Err(Error::Truncated);
        }
        let (head, rest) = self.bytes.split_at(n);
        self.bytes = rest;
        Ok(head)
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N]> {
        Ok(self.take(N)?.try_into().expect("N bytes"))
    }

    fn u8(&mut self) -> Result<u8> {
        Ok(self.array::<1>()?[0])
    }

    fn u16(&mut self) -> Result<u16> {
        self.array().map(u16::from_le_bytes)
    }

    fn u32(&mut self) -> Result<u32> {
        self.array().map(u32::from_le_bytes)
    }

    fn u64(&mut self) -> Result<u64> {
        self.array().map(u64::from_le_bytes)
    }

    /// A `u64` length of bytes that follow.
    fn size(&mut self) -> Result<usize> {
        usize::try_from(self.u64()?).map_err(|_| Error::Truncated)
    }

    /// The bytes before the next NUL, which is consumed.
    fn cstr(&mut self) -> Result<&'a [u8]> {
        let end = self
            .bytes
            .iter()
            .position(|&b| b == 0)
            .ok_or(Error::Truncated)?;
        let head = self.take(end)?;
        self.take(1)?;
        Ok(head)
    }
}

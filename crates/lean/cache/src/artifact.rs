//! Prepare compressed bodies once, and bound their native Lake restoration.

use std::{
    collections::BTreeMap,
    fs::{self, File},
    io::{self, Read, Write},
    path::{Path, PathBuf},
    time::Instant,
};

use covalence_lib_serde::{Deserialize, Serialize};

use crate::{
    error::{IoResult, Result, encode, invalid, json},
    mapping, storage,
};

pub(crate) const MANIFEST: &str = "artifacts.json";
pub(crate) const DIRECTORY: &str = "artifacts";

#[derive(Clone, Deserialize, Serialize)]
#[serde(crate = "covalence_lib_serde::serde", deny_unknown_fields)]
pub(crate) struct Artifact {
    pub bytes: u64,
    pub encoded_bytes: u64,
}

#[derive(Deserialize, Serialize)]
#[serde(crate = "covalence_lib_serde::serde", deny_unknown_fields)]
pub(crate) struct Manifest {
    schema: u32,
    pub artifacts: BTreeMap<String, Artifact>,
}

impl Manifest {
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        let manifest: Self = json(bytes)?;
        if manifest.schema != 1
            || manifest.artifacts.keys().any(|key| {
                !key.strip_suffix(".art")
                    .is_some_and(|hash| mapping::hex(hash, 16))
            })
        {
            return Err(invalid(
                "unsupported or invalid compressed artifact manifest",
            ));
        }
        Ok(manifest)
    }
}

pub(crate) fn filename(name: &str) -> String {
    format!("{name}.zst")
}

pub(crate) fn prepare(files: &BTreeMap<String, PathBuf>, record: &Path) -> Result<()> {
    let started = Instant::now();
    let staging = tempfile::tempdir_in(record).operation("create artifact staging directory")?;
    let files = files.iter().collect::<Vec<_>>();
    let prepared = storage::parallel(&files, |_, (name, path)| {
        let mut input = File::open(path).operation("open Lake artifact for compression")?;
        let bytes = input.metadata().operation("read artifact length")?.len();
        let output = File::create_new(staging.path().join(filename(name)))
            .operation("create compressed artifact")?;
        let mut encoder =
            zstd::stream::write::Encoder::new(output, 1).operation("start Zstandard encoder")?;
        encoder
            .include_checksum(true)
            .operation("enable artifact checksum")?;
        encoder
            .set_pledged_src_size(Some(bytes))
            .operation("set artifact length")?;
        let read = io::copy(&mut Read::by_ref(&mut input).take(bytes), &mut encoder)
            .operation("compress Lake artifact")?;
        if read != bytes {
            return Err(invalid("Lake artifact ended while preparing publication"));
        }
        let output = encoder.finish().operation("finish compressed artifact")?;
        let encoded_bytes = output
            .metadata()
            .operation("read compressed artifact length")?
            .len();
        Ok((
            (*name).clone(),
            Artifact {
                bytes,
                encoded_bytes,
            },
        ))
    })?;
    let manifest = Manifest {
        schema: 1,
        artifacts: prepared.into_iter().collect(),
    };
    let directory = record.join(DIRECTORY);
    if directory.exists() {
        fs::remove_dir_all(&directory).operation("remove prior prepared artifacts")?;
    }
    fs::rename(staging.path(), &directory).operation("install prepared artifact directory")?;
    fs::write(record.join(MANIFEST), encode(&manifest)?)
        .operation("save compressed artifact manifest")?;
    let report = serde_json::json!({
        "artifacts": manifest.artifacts.len(),
        "raw_bytes": manifest.artifacts.values().map(|a| a.bytes).sum::<u64>(),
        "encoded_bytes": manifest.artifacts.values().map(|a| a.encoded_bytes).sum::<u64>(),
        "seconds": started.elapsed().as_secs_f64(),
    });
    fs::write(record.join("preparation.json"), encode(&report)?)
        .operation("save compression measurements")?;
    println!("Lake artifact preparation: {report}");
    Ok(())
}

struct Exact<'a> {
    writer: &'a mut dyn Write,
    left: u64,
}

impl Write for Exact<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() as u64 > self.left {
            return Err(io::Error::other("artifact exceeds recorded decoded length"));
        }
        let written = self.writer.write(bytes)?;
        self.left -= written as u64;
        Ok(written)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.writer.flush()
    }
}

pub(crate) fn decode_to(
    artifact: &Artifact,
    output: &mut dyn Write,
    transfer: impl FnOnce(&mut dyn Write) -> Result<Option<u64>>,
) -> Result<Option<u64>> {
    let mut decoder = zstd::stream::raw::Decoder::new().operation("start artifact decoder")?;
    // Our level-one frames need at most 2 MiB. Reject untrusted window requests
    // above 8 MiB independently of the much larger decoded artifact length.
    decoder
        .set_parameter(zstd::stream::raw::DParameter::WindowLogMax(23))
        .operation("bound artifact decoder window")?;
    let mut decoding = zstd::stream::zio::Writer::new(
        Exact {
            writer: output,
            left: artifact.bytes,
        },
        decoder,
    );
    let Some(bytes) = transfer(&mut decoding)? else {
        return Ok(None);
    };
    if bytes != artifact.encoded_bytes {
        return Err(invalid(
            "compressed artifact length does not match the manifest",
        ));
    }
    decoding
        .finish()
        .operation("finish artifact decompression")?;
    if decoding.into_inner().0.left != 0 {
        return Err(invalid(
            "artifact is shorter than its recorded decoded length",
        ));
    }
    Ok(Some(artifact.bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn malformed_compressed_bodies_cannot_complete_a_restore() {
        let original = vec![b'x'; 32768];
        let compressed = zstd::stream::encode_all(original.as_slice(), 1).unwrap();
        let artifact = Artifact {
            bytes: original.len() as u64,
            encoded_bytes: compressed.len() as u64,
        };
        let decode = |body: &[u8], spec: &Artifact| {
            let mut out = Vec::new();
            let result = decode_to(spec, &mut out, |writer| {
                writer.write_all(body).operation("test transfer")?;
                Ok(Some(body.len() as u64))
            });
            (result, out)
        };
        let (result, bytes) = decode(&compressed, &artifact);
        assert_eq!(result.unwrap(), Some(original.len() as u64));
        assert_eq!(bytes, original);
        for cut in 0..compressed.len() {
            let truncated = Artifact {
                bytes: artifact.bytes,
                encoded_bytes: cut as u64,
            };
            assert!(
                decode(&compressed[..cut], &truncated).0.is_err(),
                "accepted prefix {cut}"
            );
        }
        for bytes in [1, artifact.bytes - 1, artifact.bytes + 1] {
            assert!(
                decode(
                    &compressed,
                    &Artifact {
                        bytes,
                        ..artifact.clone()
                    }
                )
                .0
                .is_err()
            );
        }
        let mut trailing = compressed.clone();
        trailing.extend_from_slice(b"garbage");
        assert!(
            decode(
                &trailing,
                &Artifact {
                    encoded_bytes: trailing.len() as u64,
                    ..artifact.clone()
                }
            )
            .0
            .is_err()
        );
        assert!(
            decode(
                &compressed,
                &Artifact {
                    encoded_bytes: artifact.encoded_bytes + 1,
                    ..artifact
                }
            )
            .0
            .is_err()
        );
    }

    #[test]
    fn oversized_decoder_windows_fail_before_writing_output() {
        let bytes = 16 * 1024 * 1024;
        let mut encoder = zstd::stream::write::Encoder::new(Vec::new(), 1).unwrap();
        encoder.window_log(24).unwrap();
        encoder.set_pledged_src_size(Some(bytes)).unwrap();
        io::copy(&mut io::repeat(0).take(bytes), &mut encoder).unwrap();
        let compressed = encoder.finish().unwrap();
        let artifact = Artifact {
            bytes,
            encoded_bytes: compressed.len() as u64,
        };
        let mut output = Vec::new();
        let result = decode_to(&artifact, &mut output, |writer| {
            writer
                .write_all(&compressed)
                .operation("test oversized window")?;
            Ok(Some(artifact.encoded_bytes))
        });
        assert!(result.is_err());
        assert!(output.is_empty());
    }
}

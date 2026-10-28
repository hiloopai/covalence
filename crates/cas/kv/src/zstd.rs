//! Private Zstandard encoding and decoding for the key-value CAS.

use std::io::{self, Read, Seek, Write};

use covalence_cas_trait::Error;
use covalence_lib_cid::Blake3;
use covalence_lib_data::Bytes;

use super::{Hashing, corrupt, hashes_to, io_error};

pub(super) fn encode(bytes: Bytes, level: i32) -> Result<Bytes, Error> {
    ::zstd::stream::encode_all(bytes.as_ref(), level)
        .map(Into::into)
        .map_err(io_error)
}

/// Finish a compressed frame and check its logical bytes before storage.
pub(super) fn stage(
    reader: &mut dyn Read,
    len: u64,
    cid: &Blake3,
    level: i32,
) -> Result<std::fs::File, Error> {
    let mut file = tempfile::tempfile().map_err(io_error)?;
    let mut encoder = ::zstd::stream::write::Encoder::new(&mut file, level).map_err(io_error)?;
    let mut hashing = Hashing::new(&mut encoder);
    let read = io::copy(&mut reader.take(len), &mut hashing).map_err(io_error)?;
    if read != len {
        return Err(io_error(io::ErrorKind::UnexpectedEof.into()));
    }
    if !hashes_to(&hashing.hasher, cid) {
        return Err(corrupt());
    }
    encoder.finish().map_err(io_error)?;
    file.rewind().map_err(io_error)?;
    Ok(file)
}

/// Decode all frames, retaining only what `writer` accepts.
pub(super) fn decode_to(
    writer: &mut dyn Write,
    read: impl FnOnce(&mut dyn Write) -> Result<Option<u64>, Error>,
) -> Result<Option<(blake3::Hasher, u64)>, Error> {
    let decoder = ::zstd::stream::raw::Decoder::new().map_err(io_error)?;
    let mut decoding = ::zstd::stream::zio::Writer::new(Hashing::new(writer), decoder);
    let Some(_) = read(&mut decoding)? else {
        return Ok(None);
    };
    decoding.finish().map_err(io_error)?;
    let hashing = decoding.into_inner().0;
    Ok(Some((hashing.hasher, hashing.len)))
}

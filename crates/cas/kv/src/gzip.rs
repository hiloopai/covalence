//! Private single-member gzip encoding and decoding for the key-value CAS.

use std::io::{self, Read, Seek, Write};

use flate2::{
    Compression,
    write::{GzDecoder, GzEncoder},
};

use covalence_cas_trait::Error;
use covalence_lib_cid::Blake3;
use covalence_lib_data::Bytes;

use super::{Hashing, corrupt, hashes_to, io_error};

pub(super) fn encode(bytes: Bytes, level: u32) -> Result<Bytes, Error> {
    let mut encoder = GzEncoder::new(Vec::new(), Compression::new(level));
    encoder.write_all(&bytes).map_err(io_error)?;
    encoder.finish().map(Into::into).map_err(io_error)
}

/// Finish the member and check its logical bytes before storage.
pub(super) fn stage(
    reader: &mut dyn Read,
    len: u64,
    cid: &Blake3,
    level: u32,
) -> Result<std::fs::File, Error> {
    let mut file = tempfile::tempfile().map_err(io_error)?;
    let mut encoder = GzEncoder::new(&mut file, Compression::new(level));
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

/// `GzDecoder` stops after one member; additional input is an error here.
struct SingleMember<'a>(GzDecoder<Hashing<'a>>);

impl Write for SingleMember<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let consumed = self.0.write(bytes)?;
        if consumed == 0 && !bytes.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "trailing bytes after gzip member",
            ));
        }
        Ok(consumed)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}

pub(super) fn decode_to(
    writer: &mut dyn Write,
    read: impl FnOnce(&mut dyn Write) -> Result<Option<u64>, Error>,
) -> Result<Option<(blake3::Hasher, u64)>, Error> {
    let mut decoding = SingleMember(GzDecoder::new(Hashing::new(writer)));
    let Some(_) = read(&mut decoding)? else {
        return Ok(None);
    };
    // GzDecoder requires all eight trailer bytes, which it accepts only after
    // the DEFLATE body reaches StreamEnd, and checks CRC32 and ISIZE here.
    // Its header parser also checks FHCRC when present.
    let hashing = decoding.0.finish().map_err(io_error)?;
    Ok(Some((hashing.hasher, hashing.len)))
}

//! Private RFC 1950 zlib encoding and decoding for the key-value CAS.

use std::io::{self, Read, Seek, Write};

use flate2::{Compression, Decompress, FlushDecompress, Status, write::ZlibEncoder};

use covalence_cas_trait::Error;
use covalence_lib_cid::Blake3;
use covalence_lib_data::Bytes;

use super::{Hashing, corrupt, hashes_to, io_error};

pub(super) fn encode(bytes: Bytes, level: u32) -> Result<Bytes, Error> {
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::new(level));
    encoder.write_all(&bytes).map_err(io_error)?;
    encoder.finish().map(Into::into).map_err(io_error)
}

/// Finish the stream and check its logical bytes before storage.
pub(super) fn stage(
    reader: &mut dyn Read,
    len: u64,
    cid: &Blake3,
    level: u32,
) -> Result<std::fs::File, Error> {
    let mut file = tempfile::tempfile().map_err(io_error)?;
    let mut encoder = ZlibEncoder::new(&mut file, Compression::new(level));
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

/// A single zlib stream, explicitly requiring its checksum and end marker.
struct Decoding<'a> {
    decoder: Decompress,
    hashing: Hashing<'a>,
    complete: bool,
}

impl Decoding<'_> {
    /// Consume input and forward decoded bytes, without retaining the block.
    fn step(&mut self, bytes: &[u8]) -> io::Result<(usize, usize)> {
        let mut output = [0; 32 * 1024];
        let before_in = self.decoder.total_in();
        let before_out = self.decoder.total_out();
        let status = self
            .decoder
            .decompress(bytes, &mut output, FlushDecompress::None)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        let consumed = (self.decoder.total_in() - before_in) as usize;
        let produced = (self.decoder.total_out() - before_out) as usize;
        self.hashing.write_all(&output[..produced])?;
        self.complete = status == Status::StreamEnd;
        Ok((consumed, produced))
    }

    fn finish(&mut self) -> io::Result<()> {
        while !self.complete {
            // Drain any pending output; a successful high-level writer finish
            // alone would not prove that a complete zlib stream was supplied.
            let (_, produced) = self.step(&[])?;
            if !self.complete && produced == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "incomplete zlib stream",
                ));
            }
        }
        Ok(())
    }
}

impl Write for Decoding<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.is_empty() {
            return Ok(0);
        }
        let mut consumed = 0;
        while consumed < bytes.len() {
            if self.complete {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "trailing bytes after zlib stream",
                ));
            }
            let (read, produced) = self.step(&bytes[consumed..])?;
            consumed += read;
            if read == 0 && produced == 0 && !self.complete {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "zlib decoder made no progress",
                ));
            }
        }
        Ok(consumed)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.hashing.flush()
    }
}

/// Decode one stream, retaining only what `writer` accepts.
pub(super) fn decode_to(
    writer: &mut dyn Write,
    read: impl FnOnce(&mut dyn Write) -> Result<Option<u64>, Error>,
) -> Result<Option<(blake3::Hasher, u64)>, Error> {
    let mut decoding = Decoding {
        decoder: Decompress::new(true),
        hashing: Hashing::new(writer),
        complete: false,
    };
    let Some(_) = read(&mut decoding)? else {
        return Ok(None);
    };
    decoding.finish().map_err(io_error)?;
    Ok(Some((decoding.hashing.hasher, decoding.hashing.len)))
}

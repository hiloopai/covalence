//! Private single-stream Brotli encoding and decoding for the key-value CAS.

use std::io::{self, Read, Seek, Write};

use ::brotli::{BrotliCompress, DecompressorWriter, enc::BrotliEncoderParams};

use covalence_cas_trait::Error;
use covalence_lib_cid::Blake3;
use covalence_lib_data::Bytes;

use super::{Checked, Hashing, corrupt, hashes_to, io_error};

fn params(level: u32) -> BrotliEncoderParams {
    BrotliEncoderParams {
        quality: level as i32,
        // The ordinary Brotli window, rather than the large-window extension.
        lgwin: 22,
        ..BrotliEncoderParams::default()
    }
}

pub(super) fn encode(bytes: Bytes, level: u32) -> Result<Bytes, Error> {
    let mut encoded = Vec::new();
    BrotliCompress(&mut bytes.as_ref(), &mut encoded, &params(level)).map_err(io_error)?;
    Ok(encoded.into())
}

/// Finish the stream and check its logical bytes before storage.
pub(super) fn stage(
    reader: &mut dyn Read,
    len: u64,
    cid: &Blake3,
    level: u32,
) -> Result<std::fs::File, Error> {
    let mut file = tempfile::tempfile().map_err(io_error)?;
    let mut checked = Checked {
        reader,
        hasher: blake3::Hasher::new(),
        left: len,
        cid: *cid,
        mismatch: false,
    };
    let result = BrotliCompress(&mut checked, &mut file, &params(level));
    if checked.mismatch || (checked.left == 0 && !hashes_to(&checked.hasher, cid)) {
        return Err(corrupt());
    }
    result.map_err(io_error)?;
    if checked.left != 0 {
        return Err(io_error(io::ErrorKind::UnexpectedEof.into()));
    }
    file.rewind().map_err(io_error)?;
    Ok(file)
}

/// The decoder stops at one stream; additional input is an error here.
struct SingleStream<'a> {
    decoder: DecompressorWriter<Hashing<'a>>,
    first: bool,
}

impl Write for SingleStream<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.first
            && let Some(first) = bytes.first()
        {
            self.first = false;
            // RFC 7932 reserves the seven-bit WBITS prefix 0x11.
            // The library otherwise accepts the nonstandard large-window
            // extension using that prefix. Reject it before large-window allocation.
            if first & 0x7f == 0x11 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "large-window Brotli is not an RFC 7932 stream",
                ));
            }
        }
        let consumed = self.decoder.write(bytes)?;
        if consumed == 0 && !bytes.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "trailing bytes after Brotli stream",
            ));
        }
        Ok(consumed)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.decoder.flush()
    }
}

/// Decode one stream, retaining only what `writer` accepts.
pub(super) fn decode_to(
    writer: &mut dyn Write,
    read: impl FnOnce(&mut dyn Write) -> Result<Option<u64>, Error>,
) -> Result<Option<(blake3::Hasher, u64)>, Error> {
    let mut decoding = SingleStream {
        decoder: DecompressorWriter::new(Hashing::new(writer), 32 * 1024),
        first: true,
    };
    let Some(_) = read(&mut decoding)? else {
        return Ok(None);
    };
    // close requires ResultSuccess and preserves any sink I/O error during
    // its final drain. into_inner alone would discard that error's details.
    decoding.decoder.close().map_err(io_error)?;
    let hashing = decoding.decoder.into_inner().map_err(|_| {
        io_error(io::Error::new(
            io::ErrorKind::InvalidData,
            "incomplete or invalid Brotli stream",
        ))
    })?;
    Ok(Some((hashing.hasher, hashing.len)))
}

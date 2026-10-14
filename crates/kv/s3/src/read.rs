//! Whole-value reads, size queries and single-range reads.

use std::{cell::Cell, io::Write};

use covalence_lib_data::Bytes;
use covalence_lib_error::snafu::{OptionExt, ResultExt};
use rusty_s3::S3Action;

use crate::{
    ChangedSnafu, EXPIRY, Error, IoSnafu, MissingFieldSnafu, MissingLengthSnafu,
    RangeResponseSnafu, S3Kv, StatusSnafu, body, status_error, timeout,
};

/// The reply to a ranged GET.
enum Reply {
    /// The requested bytes, the size of the value and its ETag.
    Partial {
        body: Bytes,
        size: u64,
        etag: Option<String>,
    },
    /// The server ignored the range and sent the whole value.
    Whole(Bytes),
    /// The range starts at or past the end of the value.
    Unsatisfiable,
    /// There is no value.
    Missing,
}

impl S3Kv {
    /// The value under `key`, or `None` if missing.
    pub(crate) fn read(&self, key: &[u8]) -> Result<Option<Bytes>, Error> {
        let mut value = Vec::new();
        let size = self.read_parts(key, |part| {
            value.extend_from_slice(part);
            Ok(())
        })?;
        Ok(size.map(|_| value.into()))
    }

    /// Write the value under `key` to `writer`, returning its size, or
    /// `None` if missing.
    pub(crate) fn read_to(&self, key: &[u8], writer: &mut dyn Write) -> Result<Option<u64>, Error> {
        self.read_parts(key, |part| writer.write_all(part).context(IoSnafu))
    }

    /// Read the value under `key` with ranged GETs of the part size, passing
    /// each part to `sink` in order, and return the value's size, or `None`
    /// if missing. The first range gives the size; the rest are fetched
    /// concurrently. Every part must come from the same value: one with the
    /// same size and ETag, which a value of more than one part must have.
    fn read_parts(
        &self,
        key: &[u8],
        mut sink: impl FnMut(&[u8]) -> Result<(), Error>,
    ) -> Result<Option<u64>, Error> {
        let object = self.object(key)?;
        let part = self.part;
        let (size, etag) = match self.fetch(&object, (0, part - 1), part, Vec::new())? {
            Reply::Missing => return Ok(None),
            Reply::Unsatisfiable => return Ok(Some(0)),
            Reply::Whole(body) => {
                sink(&body)?;
                return Ok(Some(body.len() as u64));
            }
            Reply::Partial { body, size, .. } if body.len() as u64 == size => {
                sink(&body)?;
                return Ok(Some(size));
            }
            Reply::Partial { body, size, etag } => {
                // Without an ETag, two values of one size look alike.
                let etag = etag.context(MissingFieldSnafu { field: "ETag" })?;
                sink(&body)?;
                (size, etag)
            }
        };
        // The first range was a whole part, so there are fewer ranges left
        // than bytes in the value.
        let count = (size - part).div_ceil(part);
        // Buffers come from this thread, which frees them once written: had
        // the part threads allocate them, glibc would keep up to twice the
        // ranges in flight in their arenas.
        let ranges = (1..=count).map(|index| {
            let start = index * part;
            let len = part.min(size - start);
            Ok((start, Vec::with_capacity(usize::try_from(len).unwrap_or(0))))
        });
        let range = |(start, buffer): (u64, Vec<u8>)| {
            let end = start.saturating_add(part - 1);
            match self.fetch(&object, (start, end), part, buffer)? {
                Reply::Partial {
                    body,
                    size: other,
                    etag: Some(other_etag),
                } if other == size && other_etag == etag => Ok(body),
                _ => ChangedSnafu.fail(),
            }
        };
        let count = usize::try_from(count).unwrap_or(usize::MAX);
        // Ranges wait their turn in memory, so take no more than run at once.
        self.in_order(count, self.concurrency, ranges, range, |body| sink(&body))?;
        Ok(Some(size))
    }

    pub(crate) fn read_size(&self, key: &[u8]) -> Result<Option<u64>, Error> {
        let object = self.object(key)?;
        let credentials = self.credentials()?;
        let (status, len) = self.send(|| {
            let url = self
                .bucket
                .head_object(Some(&credentials), &object)
                .sign(EXPIRY);
            let response = self.agent.head(url.as_str()).call()?;
            let len = response
                .headers()
                .get("content-length")
                .and_then(|len| len.to_str().ok()?.parse::<u64>().ok());
            Ok((response.status().as_u16(), len))
        })?;
        match status {
            200 => Ok(Some(len.context(MissingLengthSnafu)?)),
            404 => Ok(None),
            status => StatusSnafu { status, code: None }.fail(),
        }
    }

    pub(crate) fn read_range(
        &self,
        key: &[u8],
        start: u64,
        len: u64,
    ) -> Result<Option<Bytes>, Error> {
        let object = self.object(key)?;
        // S3 sizes fit in i64, and servers reject larger offsets. An empty
        // range, or one past any size, cannot be asked for, so ask whether
        // the key exists.
        const MAX: u64 = i64::MAX as u64;
        if len == 0 || start > MAX {
            return Ok(self.read_size(key)?.map(|_| Bytes::new()));
        }
        let end = start.saturating_add(len - 1).min(MAX);
        Ok(match self.fetch(&object, (start, end), len, Vec::new())? {
            Reply::Partial { body, .. } => Some(body),
            Reply::Whole(body) => {
                let first = start.min(body.len() as u64) as usize;
                Some(body.slice(first..))
            }
            Reply::Unsatisfiable => Some(Bytes::new()),
            Reply::Missing => None,
        })
    }

    /// GET bytes `start..=end` of `object`, reading at most `limit` bytes
    /// into `buffer`.
    fn fetch(
        &self,
        object: &str,
        (start, end): (u64, u64),
        limit: u64,
        buffer: Vec<u8>,
    ) -> Result<Reply, Error> {
        // A retry reads into a new buffer, as the old one went with the failure.
        let buffer = Cell::new(buffer);
        let range = format!("bytes={start}-{end}");
        let credentials = self.credentials()?;
        let (status, (body, content_range, etag)) = self.send(|| {
            let mut action = self.bucket.get_object(Some(&credentials), object);
            action.headers_mut().insert("range", &range);
            let url = action.sign(EXPIRY);
            let request = self.agent.get(url.as_str()).config();
            let request = request.timeout_global(Some(timeout(limit))).build();
            let response = request.header("Range", &range).call()?;
            let header = |name| {
                response
                    .headers()
                    .get(name)
                    .and_then(|value| value.to_str().ok())
                    .map(str::to_owned)
            };
            let (content_range, etag) = (header("content-range"), header("etag"));
            let (status, body) = body(response, limit, buffer.take())?;
            Ok((status, (body, content_range, etag)))
        })?;
        match status {
            206 => {
                let len = body.len() as u64;
                let size = matches_range(content_range.as_deref(), start, end, len)
                    .context(RangeResponseSnafu)?;
                Ok(Reply::Partial {
                    body: body.into(),
                    size,
                    etag,
                })
            }
            // A server may ignore Range and return the whole value, which
            // the read limit bounds.
            200 => Ok(Reply::Whole(body.into())),
            416 => Ok(Reply::Unsatisfiable),
            404 => Ok(Reply::Missing),
            status => status_error(status, &body),
        }
    }
}

/// The size of the value, if `header` describes a single S3 range that
/// starts at the requested offset and ends at either the requested end or
/// EOF, and the body holds precisely those `len` bytes.
fn matches_range(header: Option<&str>, start: u64, end: u64, len: u64) -> Option<u64> {
    let (range, size) = header?.strip_prefix("bytes ")?.split_once('/')?;
    let (first, last) = range.split_once('-')?;
    let first = first.parse::<u64>().ok()?;
    let last = last.parse::<u64>().ok()?;
    let size = size.parse::<u64>().ok()?;
    (first == start
        && first <= last
        && last < size
        && last == end.min(size - 1)
        && len == last - first + 1)
        .then_some(size)
}

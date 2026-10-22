//! PUTs: one request for a value up to the part size, a multipart upload for
//! a larger one. A put of a new value is a conditional write (`If-None-Match:
//! *` on the PUT, or on completing the upload). An occupied key returns 412
//! or a `PreconditionFailed` error inside a 200 multipart completion response.

use std::io::Read;

use covalence_lib_data::Bytes;
use covalence_lib_error::snafu::{OptionExt, ResultExt};
use data_encoding::BASE64;
use rusty_s3::{S3Action, actions::CreateMultipartUpload};
use sha2::{Digest, Sha256};

use crate::{
    EXPIRY, Error, IoSnafu, MAX_ERROR, MAX_OBJECT, MAX_PART, MAX_PARTS, MAX_SINGLE_PUT, MIN_PART,
    MissingFieldSnafu, S3Kv, UploadAllocationSnafu, body, status_error, timeout,
};

/// The checked geometry and upload payload reservation for a value.
///
/// Produced by [`S3Kv::upload_plan`], which is also used to admit writes.
/// It does not allocate payloads, reserve budget or send requests.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UploadPlan {
    /// The largest part payload, or zero for an empty value.
    pub part_bytes: u64,
    /// The number of parts, including one for a single PUT.
    pub parts: u16,
    /// The maximum payload held by this upload's concurrent workers and reader.
    ///
    /// Excludes source, allocator, HTTP/TLS and metadata overhead; see
    /// [`S3Kv::upload_buffer_bytes`].
    pub buffer_bytes: u64,
    /// Whether the value uses multipart upload, rather than one PUT.
    pub multipart: bool,
}

/// Reject unsupported quantities before any allocation or upload request.
fn within(field: &'static str, value: u64, minimum: u64, maximum: u64) -> Result<(), Error> {
    if (minimum..=maximum).contains(&value) {
        Ok(())
    } else {
        Err(Error::UploadLimit {
            field,
            value,
            minimum,
            maximum,
        })
    }
}

/// The header carrying the base64 SHA-256 of a PUT body, signed with the URL.
const CHECKSUM: &str = "x-amz-checksum-sha256";

/// The header making a write conditional on the key holding no value.
const IF_NONE_MATCH: &str = "if-none-match";

/// Whether a write may replace a value under its key.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Write {
    /// Replace any value.
    Replace,
    /// Store only if the key holds no value; S3 decides, atomically.
    New,
}

/// An uploaded part: its ETag and base64 SHA-256.
struct Part {
    etag: String,
    checksum: String,
}

/// Whether a write answered with `status` stored: 200 did, and 412 to a new
/// value means the key already holds one.
fn stored(status: u16, body: &[u8], write: Write) -> Result<bool, Error> {
    match status {
        200 => Ok(true),
        412 if write == Write::New => Ok(false),
        status => status_error(status, body),
    }
}

/// `text` escaped for an XML element.
fn escape(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;")
}

impl S3Kv {
    /// Plan a write of `len` bytes without reading, allocating or sending.
    ///
    /// Checks S3's 10,000-part limit, 5 GiB maximum part, 5 MiB minimum
    /// non-final part, 5 GB single-PUT limit and this platform's allocation
    /// limit. Parts grow beyond the configured size when needed. An invalid
    /// length or part size returns [`Error::UploadLimit`].
    ///
    /// The reservation is the smaller of the value's length and the part
    /// size times the number of workers. It covers the reader's next part
    /// as well as in-flight parts: a worker slot is available before the
    /// next part is read. This does not reserve capacity or check current
    /// availability; the write does that atomically before doing any I/O.
    pub fn upload_plan(&self, len: u64) -> Result<UploadPlan, Error> {
        within("object length", len, 0, MAX_OBJECT)?;
        // A small configured part remains useful for ranged reads and tiny
        // single PUTs, but an oversized one is never a supported upload setting.
        if self.part > MAX_PART {
            within("part size", self.part, MIN_PART, MAX_PART)?;
        }
        let part = self.part_for(len);
        let multipart = len > part || len > MAX_SINGLE_PUT;
        if multipart {
            within("part size", part, MIN_PART, MAX_PART)?;
        }
        let parts = if multipart { len.div_ceil(part) } else { 1 };
        within("part count", parts, 1, MAX_PARTS)?;
        let part_bytes = len.min(part);
        // Vec allocations must fit isize, a stronger condition than usize.
        within("part allocation", part_bytes, 0, isize::MAX as u64)?;
        let workers = self.concurrency.min(parts as usize) as u64;
        // Validated S3 geometry bounds this product by MAX_OBJECT.
        let buffer_bytes = len.min(workers * part_bytes);
        Ok(UploadPlan {
            part_bytes,
            parts: parts as u16,
            buffer_bytes,
            multipart,
        })
    }

    /// Store `value` under `key`, as `write` says; whether it stored.
    pub(crate) fn write(&self, key: &[u8], value: Bytes, write: Write) -> Result<bool, Error> {
        let mut offset = 0;
        self.upload(key, value.len() as u64, write, |len| {
            let part = value.slice(offset..offset + len);
            offset += len;
            Ok(part)
        })
    }

    /// Store the next `len` bytes of `reader` under `key`, as `write` says;
    /// whether it stored.
    pub(crate) fn write_from(
        &self,
        key: &[u8],
        reader: &mut dyn Read,
        len: u64,
        write: Write,
    ) -> Result<bool, Error> {
        self.upload(key, len, write, |len| {
            let mut part = Vec::new();
            part.try_reserve_exact(len)
                .context(UploadAllocationSnafu { bytes: len as u64 })?;
            part.resize(len, 0);
            reader.read_exact(&mut part).context(IoSnafu)?;
            Ok(part.into())
        })
    }

    /// Store `len` bytes under `key`, taking each part's bytes from `next`
    /// in order, which is asked for all of them at once if they fit in one
    /// part. Parts go up concurrently; the upload is aborted only once none
    /// is in flight, so that no part outlives it, and also when a new value
    /// finds the key taken, which leaves the parts uploaded.
    fn upload(
        &self,
        key: &[u8],
        len: u64,
        write: Write,
        mut next: impl FnMut(usize) -> Result<Bytes, Error>,
    ) -> Result<bool, Error> {
        let plan = self.upload_plan(len)?;
        // This guard outlives in_order's scoped worker joins and the abort
        // request, on success, error and unwinding alike.
        let _reservation = self
            .upload_budget
            .as_ref()
            .map(|budget| budget.reserve(plan.buffer_bytes))
            .transpose()?;
        let object = self.object(key)?;
        if !plan.multipart {
            return self.put_object(&object, next(plan.part_bytes as usize)?, write);
        }
        let upload_id = self.create(&object)?;
        // The planner checked every allocation and the part count above.
        let part = plan.part_bytes;
        let count = u64::from(plan.parts);
        let mut parts = Vec::with_capacity(count as usize);
        let jobs = (0..count).map(|index| {
            let size = part.min(len - index * part);
            Ok((index as u16 + 1, next(size as usize)?))
        });
        // Part ETags are small, so only the parts in flight bound memory, and
        // one slow part does not hold up the rest.
        let result = self
            .in_order(
                count as usize,
                count as usize,
                jobs,
                |(number, bytes)| self.upload_part(&object, &upload_id, number, &bytes),
                |uploaded| {
                    parts.push(uploaded);
                    Ok(())
                },
            )
            .and_then(|()| self.complete(&object, &upload_id, &parts, write));
        match result {
            Ok(true) => Ok(true),
            Ok(false) => self.abort(&object, &upload_id).map(|()| false),
            Err(cause) => Err(match self.abort(&object, &upload_id) {
                Ok(()) => cause,
                Err(source) => Error::Abort {
                    upload_id,
                    cause: Box::new(cause),
                    source: Box::new(source),
                },
            }),
        }
    }

    /// The part size for a value of `len` bytes: the configured one, or as
    /// many bytes as keep it to [`MAX_PARTS`] parts.
    fn part_for(&self, len: u64) -> u64 {
        self.part.max(len.div_ceil(MAX_PARTS))
    }

    /// PUT `value` as `object` in one request, as `write` says; whether it
    /// stored.
    fn put_object(&self, object: &str, value: Bytes, write: Write) -> Result<bool, Error> {
        let checksum = BASE64.encode(&Sha256::digest(&value));
        let credentials = self.credentials()?;
        let (status, body) = self.send(|| {
            let mut action = self.bucket.put_object(Some(&credentials), object);
            action.headers_mut().insert(CHECKSUM, &checksum);
            if write == Write::New {
                action.headers_mut().insert(IF_NONE_MATCH, "*");
            }
            let url = action.sign(EXPIRY);
            let request = self.agent.put(url.as_str()).config();
            let request = request.timeout_global(Some(timeout(value.len() as u64)));
            let mut request = request.build().header(CHECKSUM, &checksum);
            if write == Write::New {
                request = request.header(IF_NONE_MATCH, "*");
            }
            body(request.send(&value[..])?, MAX_ERROR, Vec::new())
        })?;
        stored(status, &body, write)
    }

    /// Start a multipart upload of `object` with SHA-256 part checksums, and
    /// return its ID.
    fn create(&self, object: &str) -> Result<String, Error> {
        const ALGORITHM: &str = "x-amz-checksum-algorithm";
        let credentials = self.credentials()?;
        let (status, body) = self.send(|| {
            let mut action = self
                .bucket
                .create_multipart_upload(Some(&credentials), object);
            action.headers_mut().insert(ALGORITHM, "SHA256");
            let url = action.sign(EXPIRY);
            let request = self.agent.post(url.as_str()).header(ALGORITHM, "SHA256");
            body(request.send_empty()?, MAX_ERROR, Vec::new())
        })?;
        if status != 200 {
            return status_error(status, &body);
        }
        let id = str::from_utf8(&body)
            .ok()
            .and_then(|body| CreateMultipartUpload::parse_response(body).ok())
            .context(MissingFieldSnafu { field: "upload ID" })?;
        Ok(id.upload_id().to_owned())
    }

    /// Upload `bytes` as part `number` of upload `upload_id`.
    fn upload_part(
        &self,
        object: &str,
        upload_id: &str,
        number: u16,
        bytes: &[u8],
    ) -> Result<Part, Error> {
        let checksum = BASE64.encode(&Sha256::digest(bytes));
        let credentials = self.credentials()?;
        let (status, (body, etag)) = self.send(|| {
            let mut action = self
                .bucket
                .upload_part(Some(&credentials), object, number, upload_id);
            action.headers_mut().insert(CHECKSUM, &checksum);
            let url = action.sign(EXPIRY);
            let request = self.agent.put(url.as_str()).config();
            let request = request.timeout_global(Some(timeout(bytes.len() as u64)));
            let request = request.build().header(CHECKSUM, &checksum);
            let response = request.send(bytes)?;
            let etag = response
                .headers()
                .get("etag")
                .and_then(|etag| etag.to_str().ok())
                .map(str::to_owned);
            let (status, body) = body(response, MAX_ERROR, Vec::new())?;
            Ok((status, (body, etag)))
        })?;
        if status != 200 {
            return status_error(status, &body);
        }
        let etag = etag.context(MissingFieldSnafu { field: "ETag" })?;
        Ok(Part { etag, checksum })
    }

    /// Complete upload `upload_id` from `parts`, as `write` says; whether it
    /// stored. S3 can answer 200 with an error in the body, so that is a
    /// failure too.
    fn complete(
        &self,
        object: &str,
        upload_id: &str,
        parts: &[Part],
        write: Write,
    ) -> Result<bool, Error> {
        let mut xml = String::from("<CompleteMultipartUpload>");
        for (number, part) in (1..).zip(parts) {
            xml += &format!(
                "<Part><ETag>{}</ETag><ChecksumSHA256>{}</ChecksumSHA256>\
                 <PartNumber>{number}</PartNumber></Part>",
                escape(&part.etag),
                escape(&part.checksum),
            );
        }
        xml += "</CompleteMultipartUpload>";
        let credentials = self.credentials()?;
        let (status, body) = self.send(|| {
            // rusty-s3 writes a body without checksums, so only sign with it.
            let mut action = self.bucket.complete_multipart_upload(
                Some(&credentials),
                object,
                upload_id,
                std::iter::empty::<&str>(),
            );
            if write == Write::New {
                action.headers_mut().insert(IF_NONE_MATCH, "*");
            }
            let url = action.sign(EXPIRY);
            // S3 takes longer to join more parts.
            let wait = timeout(parts.len() as u64 * crate::MIN_RATE);
            let request = self.agent.post(url.as_str()).config();
            let mut request = request.timeout_global(Some(wait)).build();
            if write == Write::New {
                request = request.header(IF_NONE_MATCH, "*");
            }
            body(request.send(&xml)?, MAX_ERROR, Vec::new())
        })?;
        if status == 200 && body.windows(7).any(|window| window == b"<Error>") {
            return match status_error(status, &body) {
                Err(Error::Status {
                    code: Some(code), ..
                }) if write == Write::New && code == "PreconditionFailed" => Ok(false),
                result => result,
            };
        }
        stored(status, &body, write)
    }

    /// Abort upload `upload_id`, freeing its parts. A missing upload is
    /// already gone.
    fn abort(&self, object: &str, upload_id: &str) -> Result<(), Error> {
        let credentials = self.credentials()?;
        let (status, body) = self.send(|| {
            let url = self
                .bucket
                .abort_multipart_upload(Some(&credentials), object, upload_id)
                .sign(EXPIRY);
            body(
                self.agent.delete(url.as_str()).call()?,
                MAX_ERROR,
                Vec::new(),
            )
        })?;
        match status {
            200 | 204 | 404 => Ok(()),
            status => status_error(status, &body),
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::{Credentials, MAX_OBJECT, MAX_PARTS, PART, S3Kv};

    #[test]
    fn parts_keep_to_ten_thousand() {
        let location = "s3://test".parse().unwrap();
        let credentials = Credentials::new("key", "secret");
        let kv = S3Kv::new("http://localhost", "us-east-1", location, credentials).unwrap();
        for len in [
            0,
            1,
            PART,
            PART * MAX_PARTS,
            PART * MAX_PARTS + 1,
            MAX_OBJECT,
        ] {
            let part = kv.part_for(len);
            assert!(part >= PART, "{len}");
            assert!(len.div_ceil(part) <= MAX_PARTS, "{len}");
        }
        assert_eq!(kv.part_for(PART * MAX_PARTS), PART);
        assert_eq!(kv.part_for(PART * MAX_PARTS + 1), PART + 1);
    }
}

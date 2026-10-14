//! PUTs: one request for a value up to the part size, a multipart upload for
//! a larger one.

use std::io::Read;

use covalence_lib_data::Bytes;
use covalence_lib_error::snafu::{OptionExt, ResultExt};
use data_encoding::BASE64;
use rusty_s3::{S3Action, actions::CreateMultipartUpload};
use sha2::{Digest, Sha256};

use crate::{
    EXPIRY, Error, IoSnafu, MAX_ERROR, MAX_PARTS, MissingFieldSnafu, S3Kv, body, status_error,
    timeout,
};

/// The header carrying the base64 SHA-256 of a PUT body, signed with the URL.
const CHECKSUM: &str = "x-amz-checksum-sha256";

/// An uploaded part: its ETag and base64 SHA-256.
struct Part {
    etag: String,
    checksum: String,
}

/// `text` escaped for an XML element.
fn escape(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;")
}

impl S3Kv {
    /// Store `value` under `key`.
    pub(crate) fn write(&self, key: &[u8], value: Bytes) -> Result<(), Error> {
        let mut offset = 0;
        self.upload(key, value.len() as u64, |len| {
            let part = value.slice(offset..offset + len);
            offset += len;
            Ok(part)
        })
    }

    /// Store the next `len` bytes of `reader` under `key`.
    pub(crate) fn write_from(
        &self,
        key: &[u8],
        reader: &mut dyn Read,
        len: u64,
    ) -> Result<(), Error> {
        self.upload(key, len, |len| {
            let mut part = vec![0; len];
            reader.read_exact(&mut part).context(IoSnafu)?;
            Ok(part.into())
        })
    }

    /// Store `len` bytes under `key`, taking each part's bytes from `next`
    /// in order, which is asked for all of them at once if they fit in one
    /// part. Parts go up concurrently; the upload is aborted only once none
    /// is in flight, so that no part outlives it.
    fn upload(
        &self,
        key: &[u8],
        len: u64,
        mut next: impl FnMut(usize) -> Result<Bytes, Error>,
    ) -> Result<(), Error> {
        let object = self.object(key)?;
        let part = self.part_for(len);
        if len <= part {
            return self.put_object(&object, next(len as usize)?);
        }
        let upload_id = self.create(&object)?;
        // At most MAX_PARTS, so the count and part numbers fit.
        let count = len.div_ceil(part);
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
            .and_then(|()| self.complete(&object, &upload_id, &parts));
        result.map_err(|cause| match self.abort(&object, &upload_id) {
            Ok(()) => cause,
            Err(source) => Error::Abort {
                upload_id,
                cause: Box::new(cause),
                source: Box::new(source),
            },
        })
    }

    /// The part size for a value of `len` bytes: the configured one, or as
    /// many bytes as keep it to [`MAX_PARTS`] parts.
    fn part_for(&self, len: u64) -> u64 {
        self.part.max(len.div_ceil(MAX_PARTS))
    }

    /// PUT `value` as `object` in one request.
    fn put_object(&self, object: &str, value: Bytes) -> Result<(), Error> {
        let checksum = BASE64.encode(&Sha256::digest(&value));
        let credentials = self.credentials()?;
        let (status, body) = self.send(|| {
            let mut action = self.bucket.put_object(Some(&credentials), object);
            action.headers_mut().insert(CHECKSUM, &checksum);
            let url = action.sign(EXPIRY);
            let request = self.agent.put(url.as_str()).config();
            let request = request.timeout_global(Some(timeout(value.len() as u64)));
            let request = request.build().header(CHECKSUM, &checksum);
            body(request.send(&value[..])?, MAX_ERROR, Vec::new())
        })?;
        match status {
            200 => Ok(()),
            status => status_error(status, &body),
        }
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

    /// Complete upload `upload_id` from `parts`. S3 can answer 200 with an
    /// error in the body, so that is a failure too.
    fn complete(&self, object: &str, upload_id: &str, parts: &[Part]) -> Result<(), Error> {
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
            let action = self.bucket.complete_multipart_upload(
                Some(&credentials),
                object,
                upload_id,
                std::iter::empty::<&str>(),
            );
            let url = action.sign(EXPIRY);
            // S3 takes longer to join more parts.
            let wait = timeout(parts.len() as u64 * crate::MIN_RATE);
            let request = self.agent.post(url.as_str()).config();
            let request = request.timeout_global(Some(wait)).build();
            body(request.send(&xml)?, MAX_ERROR, Vec::new())
        })?;
        match status {
            200 if !body.windows(7).any(|window| window == b"<Error>") => Ok(()),
            status => status_error(status, &body),
        }
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

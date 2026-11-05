//! Bounded, paginated inventories using the same signed transport as reads.

use covalence_lib_error::snafu::ResultExt;
use instant_xml::FromXml;
use rusty_s3::S3Action;

use crate::{EXPIRY, Error, ListingXmlSnafu, S3Kv, body, status_error, valid_key};

/// One object's key and stored size, without downloading its value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ObjectInfo {
    /// The key relative to the store's configured prefix.
    pub key: String,
    /// The object's size in bytes.
    pub size: u64,
}

/// Up to 1,000 objects from a listing, with an optional next-page token.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ListPage {
    /// The objects in this page, without a guaranteed order.
    pub objects: Vec<ObjectInfo>,
    /// An opaque token to pass to the next call; `None` ends the listing.
    pub next: Option<String>,
}

const NS: &str = "http://s3.amazonaws.com/doc/2006-03-01/";

#[derive(FromXml)]
#[xml(rename = "ListBucketResult", ns(NS))]
struct Reply {
    #[xml(rename = "Prefix")]
    prefix: String,
    #[xml(rename = "IsTruncated")]
    truncated: bool,
    #[xml(rename = "NextContinuationToken")]
    next: Option<String>,
    #[xml(rename = "EncodingType")]
    encoding: Option<String>,
    contents: Vec<Object>,
}

#[derive(FromXml)]
#[xml(rename = "Contents", ns(NS))]
struct Object {
    #[xml(rename = "Key")]
    key: String,
    #[xml(rename = "Size")]
    size: u64,
}

fn invalid(reason: &'static str) -> Error {
    Error::Listing { reason }
}

fn decode(value: &str, encoded: bool) -> Result<String, Error> {
    if encoded {
        percent_encoding::percent_decode_str(value)
            .decode_utf8()
            .map(|s| s.into_owned())
            .map_err(|_| invalid("a percent-encoded key is not UTF-8"))
    } else {
        Ok(value.to_owned())
    }
}

impl S3Kv {
    /// List a page of keys starting with `prefix`, relative to this store.
    ///
    /// An empty prefix lists the store. A nonempty prefix follows the store's
    /// key rules, with an optional final `/`. Pass the returned token to read
    /// another page. Callers must detect cycles across multiple tokens.
    ///
    /// Each response is limited to 8 MiB and 1,000 objects. Invalid namespaces,
    /// encoding and incomplete pagination fail instead of returning a partial
    /// inventory. Listings across pages are not a snapshot of concurrent writes.
    pub fn list(&self, prefix: &str, continuation: Option<&str>) -> Result<ListPage, Error> {
        if !prefix.is_empty() && !valid_key(prefix.strip_suffix('/').unwrap_or(prefix)) {
            return Err(Error::Key {
                key: prefix.as_bytes().to_vec(),
            });
        }
        if continuation.is_some_and(str::is_empty) {
            return Err(invalid("empty continuation token"));
        }
        let full_prefix = format!("{}{prefix}", self.prefix);
        let credentials = self.credentials()?;
        let (status, bytes) = self.send(|| {
            let mut request = self.bucket.list_objects_v2(Some(&credentials));
            request.with_prefix(&full_prefix);
            request.with_max_keys(1000);
            if let Some(token) = continuation {
                request.with_continuation_token(token);
            }
            body(
                self.agent.get(request.sign(EXPIRY).as_str()).call()?,
                8 * 1024 * 1024,
                Vec::new(),
            )
        })?;
        if status != 200 {
            return status_error(status, &bytes);
        }
        let xml = str::from_utf8(&bytes).map_err(|_| invalid("XML is not UTF-8"))?;
        self.parse_page(xml, &full_prefix, continuation)
    }

    fn parse_page(
        &self,
        xml: &str,
        prefix: &str,
        continuation: Option<&str>,
    ) -> Result<ListPage, Error> {
        let reply: Reply = instant_xml::from_str(xml).context(ListingXmlSnafu)?;
        let encoded = match reply.encoding.as_deref() {
            Some("url") => true,
            None => false,
            _ => return Err(invalid("unsupported key encoding")),
        };
        if decode(&reply.prefix, encoded)? != prefix || reply.contents.len() > 1000 {
            return Err(invalid("wrong prefix or too many objects"));
        }
        match (reply.truncated, reply.next.as_deref()) {
            (false, None) => {}
            (true, Some(next)) if !next.is_empty() && Some(next) != continuation => {}
            _ => return Err(invalid("inconsistent or repeated continuation token")),
        }
        let objects = reply
            .contents
            .into_iter()
            .map(|object| {
                let key = decode(&object.key, encoded)?;
                if !key.starts_with(prefix) {
                    return Err(invalid("object outside requested prefix"));
                }
                let key = key
                    .strip_prefix(&self.prefix)
                    .ok_or_else(|| invalid("object outside store"))?;
                if !valid_key(key) {
                    return Err(invalid("object is not a supported store key"));
                }
                Ok(ObjectInfo {
                    key: key.to_owned(),
                    size: object.size,
                })
            })
            .collect::<Result<_, Error>>()?;
        Ok(ListPage {
            objects,
            next: reply.next,
        })
    }
}

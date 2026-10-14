//! A key-value store on S3.
//!
//! [`S3Kv`] keeps the values of a key-value store as objects in an
//! [Amazon S3](https://docs.aws.amazon.com/AmazonS3/latest/API/Welcome.html)
//! bucket, or one on any server speaking the S3 API. An [`S3Location`] names
//! the bucket and a key prefix within it.
//!
//! - [`S3Kv::new`] takes an endpoint, a region and fixed [`Credentials`]
//!   (re-exported from `rusty-s3`).
//! - [`S3Kv::from_aws`] finds the region, endpoint and credentials as the AWS
//!   SDKs do.
//! - Every request blocks, including those of the async methods.
//!
//! ```
//! use covalence_kv_s3::{Credentials, Error, S3Kv, S3Location};
//! use covalence_kv_trait::Kv;
//! let location: S3Location = "s3://bucket/some/prefix".parse()?;
//! assert_eq!(location.to_string(), "s3://bucket/some/prefix/");
//! let credentials = Credentials::new("key", "secret");
//! let store = S3Kv::new("http://localhost:9000", "us-east-1", location, credentials)?;
//! // Keys that request URLs would rewrite fail before any request is sent.
//! let Err(covalence_kv_trait::Error::Backend { source }) = store.get(b"a//b") else {
//!     panic!("expected a backend error");
//! };
//! assert!(matches!(source.downcast_ref(), Some(Error::Key { .. })));
//! # Ok::<(), Error>(())
//! ```
#![deny(missing_docs)]

mod read;
mod write;

use std::{
    collections::BTreeMap,
    fmt,
    io::{self, Read},
    panic::{self, AssertUnwindSafe},
    str::FromStr,
    sync::{
        Mutex, PoisonError,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, SystemTime},
};

use aws_config::{
    BehaviorVersion, profile::ProfileFileCredentialsProvider, provider_config::ProviderConfig,
};
use aws_credential_types::provider::{
    ProvideCredentials, SharedCredentialsProvider, error::CredentialsError,
};
use aws_smithy_http_client::tls::{Provider, rustls_provider::CryptoMode};
use covalence_kv_trait::{Kv, KvFuture, ready};
use covalence_lib_data::Bytes;
use covalence_lib_error::snafu::{OptionExt, ResultExt, Snafu};
pub use rusty_s3::Credentials;
use rusty_s3::{Bucket, BucketError, UrlStyle};

/// Errors from configuring and using an S3 store.
///
/// [`Kv`] methods return [`Error::Io`] as [`covalence_kv_trait::Error::Io`],
/// and the rest as [`covalence_kv_trait::Error::Backend`].
#[derive(Debug, Snafu)]
#[snafu(crate_root(covalence_lib_error::snafu))]
#[non_exhaustive]
pub enum Error {
    /// The endpoint is not a URL.
    #[snafu(display("invalid endpoint {endpoint}: {source}"))]
    Endpoint {
        /// The endpoint given.
        endpoint: String,
        /// Why it is not a URL.
        source: url::ParseError,
    },
    /// rusty-s3 rejected the bucket.
    #[snafu(display("invalid bucket: {source}"))]
    Bucket {
        /// Why.
        source: BucketError,
    },
    /// A request failed to complete, after any retries.
    #[snafu(display("S3 request failed: {source}"))]
    Http {
        /// The last attempt's error.
        source: ureq::Error,
    },
    /// A request completed with an unexpected status, after any retries.
    #[snafu(display(
        "S3 request failed with status {status}{}",
        code.as_deref().map(|code| format!(" ({code})")).unwrap_or_default()
    ))]
    Status {
        /// The HTTP status.
        status: u16,
        /// The S3 error code from the response body, if it had one.
        code: Option<String>,
    },
    /// The store cannot hold `key`; see [`S3Kv`].
    #[snafu(display(
        "invalid key \"{}\": expected UTF-8 with non-empty '/'-separated segments other than '.' and '..'",
        key.escape_ascii()
    ))]
    Key {
        /// The key.
        key: Vec<u8>,
    },
    /// The location is not `s3://bucket[/prefix/]`; see [`S3Location::new`].
    #[snafu(display("invalid S3 location {location:?}: expected s3://bucket[/prefix/]"))]
    Location {
        /// The location given.
        location: String,
    },
    /// A successful HEAD response lacks a valid length.
    #[snafu(display("S3 response has no valid Content-Length"))]
    MissingLength,
    /// A partial response does not describe the requested bytes.
    #[snafu(display("S3 response does not match the requested range"))]
    RangeResponse,
    /// A successful response lacks a field it needs, such as an ETag.
    #[snafu(display("S3 response has no valid {field}"))]
    MissingField {
        /// The field.
        field: &'static str,
    },
    /// The value changed between the ranged GETs reading it.
    #[snafu(display("the S3 value changed while it was read"))]
    Changed,
    /// A multipart upload failed with `cause`, and so did aborting it.
    #[snafu(display("{cause}; aborting multipart upload {upload_id} also failed: {source}"))]
    Abort {
        /// The upload's ID.
        upload_id: String,
        /// Why the upload failed.
        cause: Box<Error>,
        /// Why aborting it failed.
        source: Box<Error>,
    },
    /// The reader of a value written, or the writer of a value read, failed.
    #[snafu(display("{source}"))]
    Io {
        /// The I/O error.
        source: io::Error,
    },
    /// The AWS configuration names no region.
    #[snafu(display("no AWS region: pass one, or set AWS_REGION or a profile's region"))]
    Region,
    /// The AWS credential chain found no credentials, or failed to fetch them.
    #[snafu(display("could not load AWS credentials: {source}"))]
    Credentials {
        /// The AWS SDK's error.
        source: CredentialsError,
    },
    /// The Tokio runtime for AWS configuration could not start, or a thread
    /// for AWS configuration and credentials or for moving parts could not
    /// start or ended early.
    #[snafu(display("could not run a thread: {message}"))]
    Runtime {
        /// What failed.
        message: String,
    },
}

impl From<Error> for covalence_kv_trait::Error {
    fn from(error: Error) -> Self {
        match error {
            Error::Io { source } => Self::Io { source },
            error => Self::Backend {
                source: Box::new(error),
            },
        }
    }
}

/// Whether request URLs keep `key` as is: no empty, `.` or `..` segments.
fn valid_key(key: &str) -> bool {
    !key.split('/').any(|s| matches!(s, "" | "." | ".."))
}

/// A bucket and a key prefix within it, written `s3://bucket/prefix/`.
///
/// It parses from `s3://bucket`, `s3://bucket/` or `s3://bucket/prefix`, with
/// or without a final `/`, and displays with one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct S3Location {
    bucket: String,
    prefix: String,
}

impl S3Location {
    /// The location of `prefix` in `bucket`, or [`Error::Location`]:
    ///
    /// - `bucket` must be non-empty, without `/`, and not `.` or `..`.
    /// - `prefix` must be empty, or a valid key (see [`S3Kv`]) optionally
    ///   ending in `/`.
    pub fn new(bucket: &str, prefix: &str) -> Result<Self, Error> {
        let location = || LocationSnafu {
            location: format!("s3://{bucket}/{prefix}"),
        };
        if bucket.contains('/') || !valid_key(bucket) {
            return location().fail();
        }
        let trimmed = prefix.strip_suffix('/').unwrap_or(prefix);
        let prefix = match prefix {
            "" => String::new(),
            _ if valid_key(trimmed) => format!("{trimmed}/"),
            _ => return location().fail(),
        };
        Ok(Self {
            bucket: bucket.into(),
            prefix,
        })
    }

    /// The bucket name.
    pub fn bucket(&self) -> &str {
        &self.bucket
    }

    /// The key prefix: empty or ending in `/`.
    pub fn prefix(&self) -> &str {
        &self.prefix
    }
}

impl FromStr for S3Location {
    type Err = Error;

    /// Parse `s3://bucket` or `s3://bucket/some/prefix/`.
    fn from_str(s: &str) -> Result<Self, Error> {
        let rest = s
            .strip_prefix("s3://")
            .context(LocationSnafu { location: s })?;
        let (bucket, prefix) = rest.split_once('/').unwrap_or((rest, ""));
        Self::new(bucket, prefix)
    }
}

impl fmt::Display for S3Location {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "s3://{}/{}", self.bucket, self.prefix)
    }
}

/// Retries after the first attempt for 5xx, 429 and transport failures.
const RETRIES: u32 = 3;
/// Lifetime of each signed request URL.
const EXPIRY: Duration = Duration::from_secs(60);
/// Limit on each request attempt, including reading the response body, on
/// top of [`MIN_RATE`] for its body.
const TIMEOUT: Duration = Duration::from_secs(30);
/// The slowest transfer, in bytes per second, a request attempt waits for.
const MIN_RATE: u64 = 1 << 20;
/// The largest S3 object, in bytes.
const MAX_OBJECT: u64 = 5 << 40;
/// The most parts a multipart upload may have.
const MAX_PARTS: u64 = 10_000;
/// The default part size, 8 MiB: values larger are uploaded in parts of at
/// least this size, and values are read in ranges of it.
pub const PART: u64 = 8 << 20;
/// The default number of parts or ranges moving at once, 16.
pub const CONCURRENCY: usize = 16;
/// Limit on the size of an error response body, in bytes.
const MAX_ERROR: u64 = 64 << 10;
/// Credentials closer than this to expiring are fetched again, leaving room
/// for a signed URL's [`EXPIRY`] and its retries.
const REFRESH: Duration = Duration::from_secs(300);

/// Where [`S3Kv::from_aws`] looks beyond the AWS SDK's defaults.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AwsOptions {
    /// The AWS profile to read the region, the endpoint and credentials from,
    /// instead of `AWS_PROFILE` or `default`. Credentials then come only from
    /// this profile's settings (which may name another profile or a
    /// `credential_source`), not from the default chain.
    pub profile: Option<String>,
    /// The region, instead of the configured one.
    pub region: Option<String>,
}

/// The limit on a request attempt moving `bytes` of body.
fn timeout(bytes: u64) -> Duration {
    TIMEOUT + Duration::from_secs(bytes.min(MAX_OBJECT) / MIN_RATE)
}

/// Run `future` to completion on `runtime`, on a thread of its own so that
/// callers may already be inside an async runtime.
fn block_on<F: Future<Output: Send> + Send>(
    runtime: &tokio::runtime::Runtime,
    future: F,
) -> Result<F::Output, Error> {
    thread::scope(|scope| {
        thread::Builder::new()
            .name("covalence-kv-s3-aws".into())
            .spawn_scoped(scope, || runtime.block_on(future))
            .map_err(|error| Error::Runtime {
                message: error.to_string(),
            })?
            .join()
            .map_err(|_| Error::Runtime {
                message: "the thread panicked".into(),
            })
    })
}

/// A Tokio runtime that shuts down without blocking, so that a store may be
/// dropped inside an async runtime, where dropping a runtime panics.
struct Runtime(Option<tokio::runtime::Runtime>);

impl Runtime {
    fn new() -> Result<Self, Error> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|error| Error::Runtime {
                message: error.to_string(),
            })?;
        Ok(Self(Some(runtime)))
    }

    fn get(&self) -> &tokio::runtime::Runtime {
        self.0.as_ref().expect("taken only when dropped")
    }
}

impl Drop for Runtime {
    fn drop(&mut self) {
        if let Some(runtime) = self.0.take() {
            runtime.shutdown_background();
        }
    }
}

/// Where requests get their credentials.
enum Source {
    /// The same credentials for every request.
    Static(Credentials),
    /// An AWS credential provider, asked again as its credentials near expiry.
    Provider(Box<Cached>),
}

/// An AWS credential provider and its last answer.
struct Cached {
    runtime: Runtime,
    provider: SharedCredentialsProvider,
    last: Mutex<Option<(Credentials, Option<SystemTime>)>>,
}

impl Cached {
    fn new(runtime: Runtime, provider: SharedCredentialsProvider) -> Self {
        Self {
            runtime,
            provider,
            last: Mutex::new(None),
        }
    }

    /// The last credentials, or new ones if there are none or they expire
    /// within [`REFRESH`].
    fn get(&self) -> Result<Credentials, Error> {
        let mut last = self
            .last
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let fresh = |expiry: &Option<SystemTime>| {
            expiry.is_none_or(|expiry| SystemTime::now() + REFRESH < expiry)
        };
        if let Some((credentials, expiry)) = &*last
            && fresh(expiry)
        {
            return Ok(credentials.clone());
        }
        let aws = block_on(self.runtime.get(), self.provider.provide_credentials())?
            .context(CredentialsSnafu)?;
        let credentials = match aws.session_token() {
            Some(token) => {
                Credentials::new_with_token(aws.access_key_id(), aws.secret_access_key(), token)
            }
            None => Credentials::new(aws.access_key_id(), aws.secret_access_key()),
        };
        *last = Some((credentials.clone(), aws.expiry()));
        Ok(credentials)
    }
}

/// A store under an S3 location, using path-style requests
/// (`<endpoint>/<bucket>/<key>`).
///
/// # Keys
///
/// A key is stored as the object `<prefix><key>`. Keys must be non-empty
/// UTF-8 whose `/`-separated segments are non-empty and not `.` or `..`,
/// since request URLs would rewrite them: `a/b` is a key, but `a//b`, `/a`,
/// `a/` and `a/../b` are not. Other keys fail with [`Error::Key`], though
/// other stores such as `MemoryKv` accept them.
///
/// # Writes
///
/// - A value of up to the part size ([`PART`] unless set with
///   [`S3Kv::part_size`]) is written with one PUT.
/// - A larger value is written with a multipart upload, in parts of the part
///   size, or larger parts if needed to keep to S3's limit of 10,000 parts.
/// - Each PUT or part sends the SHA-256 of its body as
///   `x-amz-checksum-sha256`, which S3 checks against the bytes it receives.
/// - A failed multipart upload is aborted once its parts in flight finish.
///   If aborting fails too, the error is [`Error::Abort`].
///
/// # Reads
///
/// - Values are read with ranged GETs of the part size. The first gives the
///   value's size and ETag; a value larger than one part must have an ETag.
/// - Every later range must report the same size and ETag, or the read fails
///   with [`Error::Changed`], so the bytes all come from one value.
/// - A server may ignore `Range` and send the whole value. That works for a
///   value of up to the part size; for a larger one the read fails.
/// - [`Kv::size`] and [`Kv::exists`] use HEAD; [`Kv::get_range`] uses one
///   ranged GET.
///
/// # Requests
///
/// - Up to [`CONCURRENCY`] parts or ranges (unless set with
///   [`S3Kv::concurrency`]) move at once, each on its own thread, while the
///   reader or writer sees the bytes in order.
/// - A request is tried up to 4 times, waiting 0.5, 1 and 2 seconds between
///   attempts, when it gets status 429 or 5xx, or fails to complete for a
///   reason other than a bad URL or an oversized response body.
/// - Each attempt may take 30 seconds, plus a second per MiB of the body it
///   sends or may receive (per part, when completing a multipart upload).
/// - Redirects are not followed: the request fails with [`Error::Status`].
///
/// Every request blocks: the async methods finish theirs before returning a
/// ready future, so do not await them on an async runtime thread.
///
/// ```no_run
/// use covalence_kv_s3::{AwsOptions, S3Kv};
/// use covalence_kv_trait::Kv;
/// let store = S3Kv::from_aws("s3://example/packs/".parse()?, &AwsOptions::default())?;
/// if let Some(size) = store.size(b"pack")? {
///     let prefix = store.get_range(b"pack", 0, size.min(1024))?;
///     // A missing key is None; an empty or exhausted range is empty bytes.
///     if let Some(prefix) = prefix {
///         assert!(prefix.len() <= 1024);
///     }
/// }
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
pub struct S3Kv {
    bucket: Bucket,
    prefix: String,
    credentials: Source,
    agent: ureq::Agent,
    part: u64,
    concurrency: usize,
}

impl fmt::Debug for S3Kv {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("S3Kv")
            .field("bucket", &self.bucket)
            .field("prefix", &self.prefix)
            .finish_non_exhaustive()
    }
}

impl S3Kv {
    /// A store for `location` at `endpoint`, such as
    /// `https://s3.us-east-1.amazonaws.com`, signing requests for `region`
    /// with `credentials`. Sends no request.
    pub fn new(
        endpoint: &str,
        region: &str,
        location: S3Location,
        credentials: Credentials,
    ) -> Result<Self, Error> {
        Self::with_source(endpoint, region, location, Source::Static(credentials))
    }

    fn with_source(
        endpoint: &str,
        region: &str,
        location: S3Location,
        credentials: Source,
    ) -> Result<Self, Error> {
        let url = url::Url::parse(endpoint).context(EndpointSnafu { endpoint })?;
        let bucket = Bucket::new(url, UrlStyle::Path, location.bucket, region.to_owned())
            .context(BucketSnafu)?;
        Ok(Self {
            bucket,
            prefix: location.prefix,
            credentials,
            agent: agent(CONCURRENCY),
            part: PART,
            concurrency: CONCURRENCY,
        })
    }

    /// Write values over `bytes` in parts of at least `bytes`, and read
    /// values in ranges of `bytes`; 0 counts as 1. S3 needs parts of at least
    /// 5 MiB, except the last.
    pub fn part_size(mut self, bytes: u64) -> Self {
        self.part = bytes.max(1);
        self
    }

    /// Move up to `parts` parts or ranges of a value at once, holding about
    /// that many parts in memory. 1 moves them one at a time, in order.
    pub fn concurrency(mut self, parts: usize) -> Self {
        self.concurrency = parts.max(1);
        self.agent = agent(self.concurrency);
        self
    }

    /// A store for `location`, configured as the AWS SDKs are.
    ///
    /// - Profile: `options.profile`, else `AWS_PROFILE`, else `default`.
    /// - Region: `options.region`, else `AWS_REGION`, `AWS_DEFAULT_REGION`,
    ///   the profile, or EC2 instance metadata, in that order; else
    ///   [`Error::Region`].
    /// - Endpoint: `AWS_ENDPOINT_URL` or the profile's `endpoint_url`, else
    ///   `https://s3.<region>.amazonaws.com`, or `.amazonaws.com.cn` for a
    ///   region starting `cn-`. Other partitions need `AWS_ENDPOINT_URL`.
    /// - Credentials: the SDKs' default chain (environment variables, profile
    ///   files including SSO and `credential_process`, web identity, ECS and
    ///   EKS Pod Identity, EC2 instance roles), or with `options.profile` only
    ///   that profile's settings (which may name another profile or a
    ///   `credential_source`).
    ///
    /// Credentials are fetched at the first request, and again when they are
    /// within 5 minutes of expiring. Finding the region may query instance
    /// metadata, which takes a moment off EC2 unless a region is set. This
    /// may be called inside an async runtime, though it blocks.
    pub fn from_aws(location: S3Location, options: &AwsOptions) -> Result<Self, Error> {
        let runtime = Runtime::new()?;
        let http = aws_smithy_http_client::Builder::new()
            .tls_provider(Provider::Rustls(CryptoMode::Ring))
            .build_https();
        let mut loader = aws_config::defaults(BehaviorVersion::latest()).http_client(http.clone());
        if let Some(region) = &options.region {
            loader = loader.region(aws_config::Region::new(region.clone()));
        }
        if let Some(profile) = &options.profile {
            let config = ProviderConfig::without_region().with_http_client(http);
            let credentials = ProfileFileCredentialsProvider::builder()
                .configure(&config)
                .profile_name(profile)
                .build();
            loader = loader
                .profile_name(profile)
                .credentials_provider(credentials);
        }
        let config = block_on(runtime.get(), loader.load())?;
        let region = config.region().context(RegionSnafu)?.to_string();
        let endpoint = match config.endpoint_url() {
            Some(endpoint) => endpoint.to_owned(),
            // Other partitions, such as the ISO regions, need AWS_ENDPOINT_URL.
            None if region.starts_with("cn-") => format!("https://s3.{region}.amazonaws.com.cn"),
            None => format!("https://s3.{region}.amazonaws.com"),
        };
        let provider = config
            .credentials_provider()
            .ok_or_else(|| Error::Credentials {
                source: CredentialsError::not_loaded("no credential provider configured"),
            })?;
        let source = Source::Provider(Box::new(Cached::new(runtime, provider)));
        Self::with_source(&endpoint, &region, location, source)
    }

    /// The credentials to sign the next request with.
    fn credentials(&self) -> Result<Credentials, Error> {
        match &self.credentials {
            Source::Static(credentials) => Ok(credentials.clone()),
            Source::Provider(cached) => cached.get(),
        }
    }

    /// The object name for `key`, which must be UTF-8 and a valid key.
    fn object(&self, key: &[u8]) -> Result<String, Error> {
        match str::from_utf8(key) {
            Ok(key) if valid_key(key) => Ok(format!("{}{key}", self.prefix)),
            _ => KeySnafu { key }.fail(),
        }
    }

    /// Take `count` jobs from `jobs` in order, run `work` on each on up to
    /// [`S3Kv::concurrency`] threads, and pass the results to `done` in job
    /// order. A job is taken only while fewer than that many are running and
    /// fewer than `ahead` are taken but not done, so at most that many jobs
    /// and `ahead` results are held at once. Stops at the first failure of
    /// `jobs`, `work` or `done`, once the work started has finished.
    fn in_order<J: Send, R: Send>(
        &self,
        count: usize,
        ahead: usize,
        jobs: impl Iterator<Item = Result<J, Error>>,
        work: impl Fn(J) -> Result<R, Error> + Sync,
        mut done: impl FnMut(R) -> Result<(), Error>,
    ) -> Result<(), Error> {
        let window = self.concurrency;
        let stop = AtomicBool::new(false);
        let (job_tx, job_rx) = mpsc::channel::<(usize, J)>();
        let job_rx = Mutex::new(job_rx);
        thread::scope(|scope| {
            // Dropped on the way out, even by a panic, so idle workers stop.
            let job_tx = job_tx;
            let (result_tx, result_rx) = mpsc::channel();
            for _ in 0..window.min(count) {
                let (result_tx, job_rx, work, stop) = (result_tx.clone(), &job_rx, &work, &stop);
                let worker = move || {
                    loop {
                        let job = job_rx.lock().unwrap_or_else(PoisonError::into_inner).recv();
                        let Ok((index, job)) = job else { break };
                        if stop.load(Ordering::Relaxed) {
                            break;
                        }
                        let result = panic::catch_unwind(AssertUnwindSafe(|| work(job)));
                        if result_tx.send((index, result)).is_err() {
                            break;
                        }
                    }
                };
                let spawned = thread::Builder::new()
                    .name("covalence-kv-s3-part".into())
                    .spawn_scoped(scope, worker);
                if let Err(error) = spawned {
                    stop.store(true, Ordering::Relaxed);
                    return Err(Error::Runtime {
                        message: error.to_string(),
                    });
                }
            }
            // Only workers hold senders now, so `recv` fails if all are gone.
            drop(result_tx);
            let mut jobs = jobs.fuse();
            let mut pending = BTreeMap::new();
            let (mut taken, mut returned, mut finished) = (0, 0, 0);
            let result = (|| {
                loop {
                    while taken - returned < window
                        && taken - finished < ahead
                        && let Some(job) = jobs.next()
                    {
                        // The receiver lives as long as this scope.
                        let _ = job_tx.send((taken, job?));
                        taken += 1;
                    }
                    if finished == taken {
                        return Ok(());
                    }
                    if let Some(result) = pending.remove(&finished) {
                        done(result)?;
                        finished += 1;
                        continue;
                    }
                    let (index, result) = result_rx.recv().map_err(|_| Error::Runtime {
                        message: "every part thread stopped".into(),
                    })?;
                    returned += 1;
                    match result {
                        Ok(result) => pending.insert(index, result?),
                        Err(payload) => panic::resume_unwind(payload),
                    };
                }
            })();
            stop.store(true, Ordering::Relaxed);
            drop(job_tx);
            result
        })
    }

    /// Run a request, retrying transient failures with backoff. `request`
    /// returns the status and whatever it read from the response, so a body
    /// that fails partway through is retried too.
    fn send<T>(
        &self,
        request: impl Fn() -> Result<(u16, T), ureq::Error>,
    ) -> Result<(u16, T), Error> {
        let mut attempt = 0;
        loop {
            let result = request();
            let transient = match &result {
                Ok((status, _)) => *status == 429 || (500..600).contains(status),
                Err(error) => !matches!(
                    error,
                    ureq::Error::BadUri(_)
                        | ureq::Error::BodyExceedsLimit(_)
                        | ureq::Error::InvalidProxyUrl
                ),
            };
            if !transient || attempt == RETRIES {
                return result.context(HttpSnafu);
            }
            thread::sleep(Duration::from_millis(500 << attempt));
            attempt += 1;
        }
    }
}

/// An agent keeping a connection for each of `concurrency` requests.
fn agent(concurrency: usize) -> ureq::Agent {
    ureq::Agent::config_builder()
        .http_status_as_error(false)
        .max_redirects(0)
        .timeout_global(Some(TIMEOUT))
        .max_idle_connections(concurrency)
        .max_idle_connections_per_host(concurrency)
        .build()
        .into()
}

/// The status and body of a response, read into `buffer`, reading at most
/// `limit` bytes of a 200 or 206 body and [`MAX_ERROR`] bytes of any other.
fn body(
    mut response: ureq::http::Response<ureq::Body>,
    limit: u64,
    mut body: Vec<u8>,
) -> Result<(u16, Vec<u8>), ureq::Error> {
    let status = response.status().as_u16();
    let limit = if matches!(status, 200 | 206) {
        limit
    } else {
        MAX_ERROR
    };
    // Reading into a buffer of the announced size, up to `limit`, keeps a
    // part from taking up to twice its size as the buffer grows.
    let announced = response
        .headers()
        .get("content-length")
        .and_then(|len| len.to_str().ok()?.parse::<u64>().ok())
        .unwrap_or(0)
        .min(limit);
    body.clear();
    body.reserve(usize::try_from(announced).unwrap_or(0));
    // ureq fails once it has read `limit` bytes, even at the end of the body.
    let config = response.body_mut().with_config();
    let mut reader = config.limit(limit.saturating_add(1)).reader();
    reader.read_to_end(&mut body)?;
    Ok((status, body))
}

/// [`Error::Status`], with the `<Code>` of the S3 error `body` if it has one.
fn status_error<T>(status: u16, body: &[u8]) -> Result<T, Error> {
    let code = str::from_utf8(body)
        .ok()
        .and_then(|body| body.split_once("<Code>"))
        .and_then(|(_, rest)| rest.split_once("</Code>"))
        .map(|(code, _)| code.to_owned());
    StatusSnafu { status, code }.fail()
}

impl Kv for S3Kv {
    fn get(&self, key: &[u8]) -> Result<Option<Bytes>, covalence_kv_trait::Error> {
        Ok(self.read(key)?)
    }

    /// Query the object size with HEAD.
    ///
    /// - Returns `None` on 404, and zero for an empty object.
    /// - Requires a valid Content-Length on a successful response, else
    ///   [`Error::MissingLength`].
    /// - Does not download the value.
    fn size(&self, key: &[u8]) -> Result<Option<u64>, covalence_kv_trait::Error> {
        Ok(self.read_size(key)?)
    }

    /// Fetch a single range using a signed Range header.
    ///
    /// - Follows [`Kv::get_range`], including its missing/empty/EOF rules.
    /// - A zero-length range asks HEAD, to tell a missing key from an empty range.
    /// - Validates Content-Range and the partial body length.
    /// - Limits the reply to `len`.
    /// - A whole-value 200 reply is sliced, if it is within that limit.
    /// - Servers reject offsets past `i64::MAX`: a start past it asks HEAD, as
    ///   an empty range does, and an end past it is clipped to it.
    /// - Rejects larger or differently positioned replies; no full-fetch fallback.
    fn get_range(
        &self,
        key: &[u8],
        start: u64,
        len: u64,
    ) -> Result<Option<Bytes>, covalence_kv_trait::Error> {
        Ok(self.read_range(key, start, len)?)
    }

    fn put(&self, key: &[u8], value: Bytes) -> Result<(), covalence_kv_trait::Error> {
        Ok(self.write(key, value)?)
    }

    /// Stream the value up in parts, reading them in order and holding about
    /// as many parts in memory as move at once.
    fn put_from(
        &self,
        key: &[u8],
        reader: &mut dyn io::Read,
        len: u64,
    ) -> Result<(), covalence_kv_trait::Error> {
        Ok(self.write_from(key, reader, len)?)
    }

    /// Stream the value down in ranges, writing them in order and holding
    /// about as many ranges in memory as move at once.
    fn get_to(
        &self,
        key: &[u8],
        writer: &mut dyn io::Write,
    ) -> Result<Option<u64>, covalence_kv_trait::Error> {
        Ok(self.read_to(key, writer)?)
    }

    fn aget<'a>(&'a self, key: &'a [u8]) -> KvFuture<'a, Option<Bytes>> {
        ready(self.get(key))
    }

    /// Query the size before returning a ready future.
    ///
    /// This blocks just like [`Kv::size`]; keep it off async runtime threads.
    fn asize<'a>(&'a self, key: &'a [u8]) -> KvFuture<'a, Option<u64>> {
        ready(self.size(key))
    }

    /// Fetch the range before returning a ready future.
    ///
    /// This blocks just like [`Kv::get_range`]; keep it off async runtime threads.
    fn aget_range<'a>(
        &'a self,
        key: &'a [u8],
        start: u64,
        len: u64,
    ) -> KvFuture<'a, Option<Bytes>> {
        ready(self.get_range(key, start, len))
    }

    fn aput<'a>(&'a self, key: &'a [u8], value: Bytes) -> KvFuture<'a, ()> {
        ready(self.put(key, value))
    }

    /// Ask with HEAD, as [`Kv::size`] does.
    fn exists(&self, key: &[u8]) -> Result<bool, covalence_kv_trait::Error> {
        Ok(self.read_size(key)?.is_some())
    }

    fn aexists<'a>(&'a self, key: &'a [u8]) -> KvFuture<'a, bool> {
        ready(self.exists(key))
    }
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        time::{Duration, SystemTime},
    };

    use aws_credential_types::provider::{
        ProvideCredentials, SharedCredentialsProvider, future::ProvideCredentials as Future,
    };

    use super::{Cached, REFRESH, Runtime};

    /// A provider counting its calls, whose credentials last `lifetime`.
    #[derive(Debug)]
    struct Counting {
        calls: Arc<AtomicUsize>,
        lifetime: Option<Duration>,
    }

    impl ProvideCredentials for Counting {
        fn provide_credentials<'a>(&'a self) -> Future<'a>
        where
            Self: 'a,
        {
            let call = self.calls.fetch_add(1, Ordering::SeqCst);
            let expiry = self.lifetime.map(|lifetime| SystemTime::now() + lifetime);
            Future::ready(Ok(aws_credential_types::Credentials::new(
                format!("key{call}"),
                "secret",
                Some("token".into()),
                expiry,
                "test",
            )))
        }
    }

    /// Credentials from a [`Counting`] provider, and how often it was asked.
    fn fetch(lifetime: Option<Duration>, times: usize) -> (Vec<String>, usize) {
        let calls = Arc::new(AtomicUsize::new(0));
        let provider = Counting {
            calls: calls.clone(),
            lifetime,
        };
        let cached = Cached::new(
            Runtime::new().unwrap(),
            SharedCredentialsProvider::new(provider),
        );
        let keys = (0..times)
            .map(|_| {
                let credentials = cached.get().unwrap();
                assert_eq!(credentials.token(), Some("token"));
                credentials.key().to_owned()
            })
            .collect();
        (keys, calls.load(Ordering::SeqCst))
    }

    #[test]
    fn credentials_are_kept_until_near_expiry() {
        assert_eq!(fetch(None, 3), (vec!["key0".into(); 3], 1));
        let lasting = Some(REFRESH * 2);
        assert_eq!(fetch(lasting, 3), (vec!["key0".into(); 3], 1));
        let expiring = Some(REFRESH / 2);
        let keys = vec!["key0".into(), "key1".into(), "key2".into()];
        assert_eq!(fetch(expiring, 3), (keys, 3));
    }

    #[test]
    fn credentials_load_inside_an_async_runtime() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let (keys, calls) = runtime.block_on(async { fetch(None, 1) });
        assert_eq!((keys, calls), (vec!["key0".into()], 1));
    }
}

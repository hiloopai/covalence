use std::{
    fs,
    io::{self, Read, Write},
    net::TcpListener,
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
        mpsc,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use covalence_kv_s3::{Credentials, Error, S3Kv, S3Location};
use covalence_kv_trait::Kv;
use covalence_lib_data::Bytes;
use hyper_util::{
    rt::{TokioExecutor, TokioIo},
    server::conn::auto::Builder,
};
use pollster::block_on;
use s3s::{auth::SimpleAuth, service::S3ServiceBuilder};
use s3s_fs::FileSystem;
use tokio::sync::oneshot;

const KEY: &str = "test-key";
const SECRET: &str = "test-secret";

fn check(kv: &S3Kv) {
    assert_eq!(kv.get(b"a/b").unwrap(), None);
    assert!(!kv.exists(b"a/b").unwrap());
    kv.put(b"a/b", Bytes::from_static(b"one")).unwrap();
    assert!(kv.exists(b"a/b").unwrap());
    assert!(block_on(kv.aexists(b"a/b")).unwrap());
    assert!(!kv.exists(b"a/c").unwrap());
    assert_eq!(block_on(kv.aget(b"a/b")).unwrap().unwrap(), "one");
    block_on(kv.aput(b"a/b", Bytes::from_static(b"two"))).unwrap();
    assert_eq!(kv.get(b"a/b").unwrap().unwrap(), "two");
}

/// The S3 error behind a store error.
fn s3_error(error: covalence_kv_trait::Error) -> Error {
    match error {
        covalence_kv_trait::Error::Backend { source } => *source.downcast().unwrap(),
        error => panic!("{error}"),
    }
}

/// Keys that S3 cannot hold as given.
const S3_INVALID: [&[u8]; 9] = [
    b"", b"/a", b"a/", b"a//b", b".", b"a/./b", b"..", b"a/..", b"a\xff",
];

fn location(s: &str) -> S3Location {
    s.parse().unwrap()
}

/// A store for `location` on the server at `endpoint`.
fn kv(endpoint: &str, location: &str, secret: &str) -> S3Kv {
    let credentials = Credentials::new(KEY, secret);
    S3Kv::new(endpoint, "us-east-1", self::location(location), credentials).unwrap()
}

/// An S3 server on its own thread, stopped and joined when dropped.
struct Server {
    endpoint: String,
    stop: Option<oneshot::Sender<()>>,
    thread: Option<JoinHandle<()>>,
    /// Each request as `METHOD URI RANGE`, in the order they were answered.
    answered: Arc<Mutex<Vec<String>>>,
    /// The most requests served at once.
    peak: Arc<AtomicUsize>,
}

/// What a server does with a request besides serving it.
#[derive(Clone, Copy)]
enum Act {
    Serve,
    /// Serve it once a request containing this has been answered, or after
    /// ten seconds.
    After(&'static str),
    /// Serve it, then answer 403 instead.
    Deny,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.stop.take().unwrap().send(());
        self.thread.take().unwrap().join().unwrap();
    }
}

/// Serve an S3 bucket named `test` from `root`.
fn serve(root: &Path) -> Server {
    serve_with(root, |_| Act::Serve)
}

/// Serve an S3 bucket named `test` from `root`, acting on each request as
/// `hook` says given its `METHOD URI RANGE`.
fn serve_with(root: &Path, hook: impl Fn(&str) -> Act + Send + Sync + 'static) -> Server {
    std::fs::create_dir(root.join("test")).unwrap();
    let mut builder = S3ServiceBuilder::new(FileSystem::new(root).unwrap());
    builder.set_auth(SimpleAuth::from_single(KEY, SECRET));
    let service = builder.build();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    listener.set_nonblocking(true).unwrap();
    let (stop, stopped) = oneshot::channel();
    let answered = Arc::new(Mutex::new(Vec::<String>::new()));
    let peak = Arc::new(AtomicUsize::new(0));
    let live = Arc::new(AtomicUsize::new(0));
    let hook = Arc::new(hook);
    let (log, top) = (answered.clone(), peak.clone());
    let handle = move |request: hyper::Request<hyper::body::Incoming>| {
        let (service, hook, log, live, top) = (
            service.clone(),
            hook.clone(),
            log.clone(),
            live.clone(),
            top.clone(),
        );
        async move {
            let range = request
                .headers()
                .get("range")
                .map(|range| range.to_str().unwrap());
            let line = format!(
                "{} {} {}",
                request.method(),
                request.uri(),
                range.unwrap_or("")
            );
            let act = hook(&line);
            top.fetch_max(live.fetch_add(1, Ordering::SeqCst) + 1, Ordering::SeqCst);
            if let Act::After(other) = act {
                let deadline = Instant::now() + Duration::from_secs(10);
                let answered = || log.lock().unwrap().iter().any(|line| line.contains(other));
                while !answered() && Instant::now() < deadline {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            }
            let mut response = service.call(request.map(s3s::Body::from)).await;
            live.fetch_sub(1, Ordering::SeqCst);
            if let (Act::Deny, Ok(response)) = (act, &mut response) {
                let denied = "<Error><Code>AccessDenied</Code></Error>".to_owned();
                *response = hyper::Response::builder()
                    .status(403)
                    .body(denied.into())
                    .unwrap();
            }
            log.lock().unwrap().push(line);
            response
        }
    };
    let thread = thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        // Dropping the runtime when `stopped` resolves cancels these tasks.
        runtime.spawn(async move {
            let listener = tokio::net::TcpListener::from_std(listener).unwrap();
            loop {
                let (socket, _) = listener.accept().await.unwrap();
                let service = hyper::service::service_fn(handle.clone());
                tokio::spawn(async move {
                    let _ = Builder::new(TokioExecutor::new())
                        .serve_connection(TokioIo::new(socket), service)
                        .await;
                });
            }
        });
        let _ = runtime.block_on(stopped);
    });
    Server {
        endpoint,
        stop: Some(stop),
        thread: Some(thread),
        answered,
        peak,
    }
}

/// A fake server that answers one connection with each of `responses` in
/// turn, or gives up after ten seconds. Joining it gives each request.
fn fake(responses: Vec<String>) -> (String, JoinHandle<Vec<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    listener.set_nonblocking(true).unwrap();
    let thread = thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut heads = Vec::new();
        for response in responses {
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(_) if Instant::now() < deadline => thread::sleep(Duration::from_millis(5)),
                    Err(_) => return heads,
                }
            };
            stream.set_nonblocking(false).unwrap();
            heads.push(read_request(&mut stream));
            stream.write_all(response.as_bytes()).unwrap();
        }
        heads
    });
    (endpoint, thread)
}

/// Read a request with a `Content-Length` body, returning it as text.
fn read_request(stream: &mut impl Read) -> String {
    let mut request = Vec::new();
    let mut buffer = [0; 4096];
    let mut expected = None;
    loop {
        let read = stream.read(&mut buffer).unwrap();
        assert_ne!(read, 0, "the request ended early");
        request.extend_from_slice(&buffer[..read]);
        if expected.is_none()
            && let Some(end) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n")
        {
            let head = str::from_utf8(&request[..end]).unwrap();
            let length = head
                .lines()
                .filter_map(|line| line.split_once(": "))
                .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                .map_or(0, |(_, value)| value.parse::<usize>().unwrap());
            expected = Some(end + 4 + length);
        }
        if expected.is_some_and(|length| request.len() >= length) {
            return String::from_utf8_lossy(&request).into_owned();
        }
    }
}

/// A response with `status`, closing the connection, and `body`.
fn response(status: &str, body: &str) -> String {
    let length = body.len();
    format!("HTTP/1.1 {status}\r\nContent-Length: {length}\r\nConnection: close\r\n\r\n{body}")
}

#[test]
fn locations() {
    for (s, bucket, prefix) in [
        ("s3://b", "b", ""),
        ("s3://b/", "b", ""),
        ("s3://b/p", "b", "p/"),
        ("s3://b/p/q/", "b", "p/q/"),
    ] {
        let location = location(s);
        assert_eq!((location.bucket(), location.prefix()), (bucket, prefix));
    }
    assert_eq!(location("s3://b/p").to_string(), "s3://b/p/");
    for s in [
        "b",
        "s3://",
        "s3:///p",
        "s3://b//",
        "s3://b//p",
        "s3://b/./p",
        "s3://../p",
    ] {
        let error = s.parse::<S3Location>().unwrap_err();
        assert!(matches!(error, Error::Location { .. }), "{s}: {error}");
    }
}

#[test]
fn s3() {
    let root = tempfile::tempdir().unwrap();
    let server = serve(root.path());
    let kv = kv(&server.endpoint, "s3://test", SECRET);
    check(&kv);
    for key in S3_INVALID {
        let invalid = |error| matches!(s3_error(error), Error::Key { .. });
        assert!(invalid(kv.get(key).unwrap_err()), "{key:?}");
        assert!(invalid(kv.size(key).unwrap_err()), "{key:?}");
        for len in [0, 1] {
            assert!(invalid(kv.get_range(key, 0, len).unwrap_err()), "{key:?}");
        }
        let error = kv.put(key, Bytes::from_static(b"x")).unwrap_err();
        assert!(invalid(error), "{key:?}");
        assert!(invalid(kv.exists(key).unwrap_err()), "{key:?}");
    }
    assert!(!format!("{kv:?}").contains(SECRET));
}

#[test]
fn s3_prefix() {
    let root = tempfile::tempdir().unwrap();
    let server = serve(root.path());
    check(&kv(&server.endpoint, "s3://test/p/q", SECRET));
    let file = std::fs::read(root.path().join("test/p/q/a/b")).unwrap();
    assert_eq!(file, b"two");
    let bare = kv(&server.endpoint, "s3://test", SECRET);
    assert_eq!(bare.get(b"p/q/a/b").unwrap().unwrap(), "two");
    assert_eq!(bare.get(b"a/b").unwrap(), None);
}

#[test]
fn s3_wrong_credentials() {
    let root = tempfile::tempdir().unwrap();
    let server = serve(root.path());
    let kv = self::kv(&server.endpoint, "s3://test", "wrong");
    let denied = |error| match s3_error(error) {
        Error::Status { status, code } => (status, code),
        error => panic!("{error}"),
    };
    let expected = (403, Some("SignatureDoesNotMatch".to_owned()));
    assert_eq!(
        denied(kv.put(b"a", Bytes::from_static(b"x")).unwrap_err()),
        expected
    );
    assert_eq!(denied(kv.get(b"a").unwrap_err()), expected);
    // HEAD does not carry an S3 error body.
    assert_eq!(denied(kv.size(b"a").unwrap_err()), (403, None));
    assert_eq!(denied(kv.get_range(b"a", 0, 1).unwrap_err()), expected);
    assert_eq!(denied(kv.exists(b"a").unwrap_err()), (403, None));
    let kv = self::kv(&server.endpoint, "s3://test", SECRET);
    assert_eq!(kv.get(b"a").unwrap(), None);
}

/// A GET whose body is cut off is retried.
#[test]
fn s3_retries_cut_off_body() {
    let cut_off = response("200 OK", "partial").replace("partial", "par");
    let (endpoint, server) = fake(vec![cut_off, response("200 OK", "partial")]);
    let kv = kv(&endpoint, "s3://test", SECRET);
    assert_eq!(kv.get(b"a").unwrap().unwrap(), "partial");
    assert_eq!(server.join().unwrap().len(), 2);
}

/// A PUT is retried after 429 and 5xx, and sends its SHA-256 every time.
#[test]
fn s3_retries_put() {
    let (endpoint, server) = fake(vec![
        response("429 Too Many Requests", ""),
        response("500 Internal Server Error", ""),
        response("200 OK", ""),
    ]);
    let kv = kv(&endpoint, "s3://test", SECRET).upload_buffer_bytes(1);
    kv.put(b"a", Bytes::from_static(b"x")).unwrap();
    let heads = server.join().unwrap();
    assert_eq!(heads.len(), 3);
    // The base64 SHA-256 of `x`.
    let checksum = "x-amz-checksum-sha256: LXEWQrcmsEQBYnyp+6wy9chTD7GQPMTbAiWHF5IaSIE=";
    for head in heads {
        assert!(head.starts_with("PUT /test/a?"), "{head}");
        assert!(head.contains(checksum), "{head}");
    }
}

/// A GET gives up after four attempts, keeping the last status and S3 code.
#[test]
fn s3_stops_after_four_attempts() {
    let slow = response(
        "503 Service Unavailable",
        "<Error><Code>SlowDown</Code></Error>",
    );
    let (endpoint, server) = fake(vec![slow; 4]);
    let kv = kv(&endpoint, "s3://test", SECRET);
    let error = s3_error(kv.get(b"a").unwrap_err());
    assert!(
        matches!(&error, Error::Status { status: 503, code: Some(code) } if code == "SlowDown"),
        "{error}"
    );
    assert_eq!(server.join().unwrap().len(), 4);
}

/// Both direct and shared stores expose native reads, including EOF and misses.
#[test]
fn sizes_and_ranges() {
    let root = tempfile::tempdir().unwrap();
    let server = serve(root.path());
    let store = kv(&server.endpoint, "s3://test/p/q", SECRET).into_dyn();
    assert_eq!(store.size(b"v").unwrap(), None);
    assert_eq!(block_on(store.asize(b"v")).unwrap(), None);
    assert_eq!(store.get_range(b"v", 0, 4).unwrap(), None);
    assert_eq!(block_on(store.aget_range(b"v", 0, 4)).unwrap(), None);
    assert_eq!(store.get_range(b"v", 0, 0).unwrap(), None);
    store.put(b"v", "hello world".into()).unwrap();
    assert_eq!(store.size(b"v").unwrap(), Some(11));
    assert_eq!(block_on(store.asize(b"v")).unwrap(), Some(11));
    for (start, len, expected) in [
        (0, 5, "hello"),
        (6, 5, "world"),
        (9, 10, "ld"),
        (11, 1, ""),
        (12, 1, ""),
        (0, 0, ""),
        (6, u64::MAX, "world"),
        (u64::MAX, 1, ""),
    ] {
        assert_eq!(
            store
                .get_range(b"v", start, len)
                .unwrap_or_else(|error| panic!("{start}+{len}: {error}"))
                .unwrap(),
            expected
        );
        assert_eq!(
            block_on(store.aget_range(b"v", start, len))
                .unwrap()
                .unwrap(),
            expected
        );
    }
    store.put(b"empty", Bytes::new()).unwrap();
    assert_eq!(store.get(b"empty").unwrap().unwrap(), "");
    assert_eq!(store.size(b"empty").unwrap(), Some(0));
    assert!(store.get_range(b"empty", 0, 1).unwrap().unwrap().is_empty());
}

/// HEAD reads no value body, accepts u64 sizes and retries transient failures.
#[test]
fn size_uses_head_and_checks_length() {
    let head = |headers: &str| format!("HTTP/1.1 200 OK\r\n{headers}Connection: close\r\n\r\n");
    let (endpoint, server) = fake(vec![
        response("503 Service Unavailable", ""),
        head("Content-Length: 18446744073709551615\r\n"),
        head("Content-Length: 0\r\n"),
        head(""),
    ]);
    let kv = kv(&endpoint, "s3://test/p/q", SECRET);
    assert_eq!(kv.size(b"v").unwrap(), Some(u64::MAX));
    assert_eq!(kv.size(b"v").unwrap(), Some(0));
    assert!(matches!(
        s3_error(kv.size(b"v").unwrap_err()),
        Error::MissingLength
    ));
    let heads = server.join().unwrap();
    assert_eq!(heads.len(), 4);
    assert!(
        heads
            .iter()
            .all(|head| head.starts_with("HEAD /test/p/q/v?"))
    );
}

fn partial(range: &str, body: &str) -> String {
    response("206 Partial Content", body).replace(
        "Connection: close",
        &format!("Content-Range: {range}\r\nConnection: close"),
    )
}

/// A cut-off partial body is retried with the same signed range.
#[test]
fn ranges_retry_and_sign_the_range() {
    let full = partial("bytes 2-4/7", "rti");
    let cut_off = full.strip_suffix('i').unwrap().to_owned();
    let (endpoint, server) = fake(vec![cut_off, full]);
    let kv = kv(&endpoint, "s3://test", SECRET);
    assert_eq!(kv.get_range(b"a", 2, 3).unwrap().unwrap(), "rti");
    let heads = server.join().unwrap();
    assert_eq!(heads.len(), 2);
    for head in heads {
        assert!(head.starts_with("GET /test/a?"), "{head}");
        assert!(head.contains("X-Amz-SignedHeaders=host%3Brange"), "{head}");
        assert!(
            head.to_ascii_lowercase().contains("range: bytes=2-4"),
            "{head}"
        );
    }
}

/// Partial responses use the requested limit, including above MAX_ERROR.
#[test]
fn ranges_obey_read_limits() {
    let large = "x".repeat(72 << 10);
    let (endpoint, server) = fake(vec![
        partial("bytes 0-73727/73728", &large),
        partial("bytes 0-3/4", "four"),
    ]);
    let kv = kv(&endpoint, "s3://test", SECRET);
    assert_eq!(
        kv.get_range(b"a", 0, large.len() as u64).unwrap().unwrap(),
        large
    );
    assert!(matches!(
        s3_error(kv.get_range(b"a", 0, 3).unwrap_err()),
        Error::Http { .. }
    ));
    assert_eq!(server.join().unwrap().len(), 2);
}

/// Invalid offsets, lengths and metadata must not be returned as useful data.
#[test]
fn ranges_reject_mismatched_replies() {
    let replies = vec![
        response("206 Partial Content", "ab"),
        partial("nonsense", "ab"),
        partial("bytes 0-1/10", "ab"),
        partial("bytes 2-3/3", "ab"),
        partial("bytes 2-4/10", "ab"),
        partial("bytes 2-2/10", "a"),
        partial("bytes 3-2/10", "ab"),
        partial("bytes 2-3/*", "ab"),
    ];
    let count = replies.len();
    let (endpoint, server) = fake(replies);
    let kv = kv(&endpoint, "s3://test", SECRET);
    for _ in 0..count {
        assert!(matches!(
            s3_error(kv.get_range(b"a", 2, 2).unwrap_err()),
            Error::RangeResponse
        ));
    }
    assert_eq!(server.join().unwrap().len(), count);
}

/// A whole-value response is sliced, if it fits the requested length.
#[test]
fn ranges_handle_ignored_range_headers() {
    let (endpoint, server) = fake(vec![response("200 OK", "four"); 4]);
    let kv = kv(&endpoint, "s3://test", SECRET);
    assert_eq!(kv.get_range(b"a", 0, 10).unwrap().unwrap(), "four");
    assert_eq!(kv.get_range(b"a", 1, 10).unwrap().unwrap(), "our");
    assert!(kv.get_range(b"a", 9, 10).unwrap().unwrap().is_empty());
    assert!(matches!(
        s3_error(kv.get_range(b"a", 0, 3).unwrap_err()),
        Error::Http { .. }
    ));
    assert_eq!(server.join().unwrap().len(), 4);
}

/// Empty ranges ask HEAD whether the key exists, after validating it.
#[test]
fn empty_ranges_use_head() {
    let (endpoint, server) = fake(vec![
        response("404 Not Found", ""),
        "HTTP/1.1 200 OK\r\nContent-Length: 11\r\nConnection: close\r\n\r\n".into(),
    ]);
    let kv = kv(&endpoint, "s3://test", SECRET);
    assert_eq!(kv.get_range(b"a", 0, 0).unwrap(), None);
    assert!(
        block_on(kv.aget_range(b"a", u64::MAX, 0))
            .unwrap()
            .unwrap()
            .is_empty()
    );
    assert!(matches!(
        s3_error(kv.get_range(b"../escape", 0, 0).unwrap_err()),
        Error::Key { .. }
    ));
    let heads = server.join().unwrap();
    assert_eq!(heads.len(), 2);
    assert!(heads.iter().all(|head| head.starts_with("HEAD /test/a?")));
}

/// Offsets past `i64::MAX`, the largest S3 size, are clamped or asked by
/// HEAD, since servers reject them.
#[test]
fn ranges_clamp_offsets() {
    let (endpoint, server) = fake(vec![
        "HTTP/1.1 200 OK\r\nContent-Length: 11\r\nConnection: close\r\n\r\n".into(),
        partial("bytes 6-10/11", "world"),
    ]);
    let kv = kv(&endpoint, "s3://test", SECRET);
    assert!(kv.get_range(b"a", u64::MAX, 2).unwrap().unwrap().is_empty());
    assert_eq!(kv.get_range(b"a", 6, u64::MAX).unwrap().unwrap(), "world");
    let heads = server.join().unwrap();
    assert_eq!(heads.len(), 2);
    assert!(heads[0].starts_with("HEAD /test/a?"), "{}", heads[0]);
    let range = format!("range: bytes=6-{}", i64::MAX);
    assert!(
        heads[1]
            .lines()
            .any(|line| line.eq_ignore_ascii_case(&range)),
        "{}",
        heads[1]
    );
}

const MIB: usize = 1 << 20;

/// `len` bytes that differ from part to part.
fn data(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i % 251) as u8).collect()
}

/// Three legal multipart bodies, with a two-byte final part.
fn multipart_ascii() -> String {
    "0".repeat(5 * MIB) + &"1".repeat(5 * MIB) + "23"
}

/// The multipart uploads on the server at `root` that are in progress or
/// left behind.
fn uploads(root: &Path) -> Vec<String> {
    fs::read_dir(root)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with(".upload"))
        .collect()
}

/// A reader of `ok` bytes that then fails.
struct Failing {
    ok: usize,
}

impl Read for Failing {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.ok == 0 {
            return Err(io::Error::other("disk"));
        }
        let len = buf.len().min(self.ok);
        buf[..len].fill(1);
        self.ok -= len;
        Ok(len)
    }
}

/// Records an unexpected read without allocating the advertised upload.
#[derive(Default)]
struct Unread {
    reads: usize,
}

impl Read for Unread {
    fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
        self.reads += 1;
        Err(io::Error::other("the upload must fail before reading"))
    }
}

/// Holds a reservation in a reader until the test permits its upload.
struct HeldReader {
    entered: Option<mpsc::SyncSender<()>>,
    release: mpsc::Receiver<()>,
}

impl Read for HeldReader {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if let Some(entered) = self.entered.take() {
            entered.send(()).unwrap();
            self.release
                .recv_timeout(Duration::from_secs(10))
                .map_err(io::Error::other)?;
        }
        buffer.fill(b'x');
        Ok(buffer.len())
    }
}

/// Preflight rejects impossible requests before reading or creating uploads.
#[test]
fn upload_limits_fail_before_io() {
    const MAX_PART: u64 = 5 << 30;
    const MAX_OBJECT: u64 = MAX_PART * 10_000;
    let root = tempfile::tempdir().unwrap();
    let server = serve(root.path());
    for (part, len, field, value, minimum, maximum) in [
        (
            5 << 20,
            MAX_OBJECT + 1,
            "object length",
            MAX_OBJECT + 1,
            0,
            MAX_OBJECT,
        ),
        (5 << 20, u64::MAX, "object length", u64::MAX, 0, MAX_OBJECT),
        (4, 5, "part size", 4, 5 << 20, MAX_PART),
        (
            MAX_PART + 1,
            1,
            "part size",
            MAX_PART + 1,
            5 << 20,
            MAX_PART,
        ),
    ] {
        // Even a regressed length check cannot allocate the huge source:
        // the separately enforced upload budget is only one byte.
        let store = kv(&server.endpoint, "s3://test", SECRET)
            .part_size(part)
            .upload_buffer_bytes(1);
        for new in [false, true] {
            let mut reader = Unread::default();
            let error = if new {
                store.put_new_from(b"v", &mut reader, len).unwrap_err()
            } else {
                store.put_from(b"v", &mut reader, len).unwrap_err()
            };
            let error = s3_error(error);
            assert!(
                matches!(error, Error::UploadLimit {
                    field: actual_field,
                    value: actual_value,
                    minimum: actual_minimum,
                    maximum: actual_maximum,
                } if (actual_field, actual_value, actual_minimum, actual_maximum)
                    == (field, value, minimum, maximum)),
                "part={part}, len={len}, new={new}: {error}"
            );
            assert_eq!(reader.reads, 0);
        }
    }
    assert!(server.answered.lock().unwrap().is_empty());
    assert!(uploads(root.path()).is_empty());
}

/// A provider-legal part must also fit this platform's Vec allocation limit.
#[test]
#[cfg(target_pointer_width = "32")]
fn upload_part_allocation_limit_precedes_io() {
    let root = tempfile::tempdir().unwrap();
    let server = serve(root.path());
    let len = isize::MAX as u64 + 1;
    let store = kv(&server.endpoint, "s3://test", SECRET)
        .part_size(len)
        .upload_buffer_bytes(1);
    let mut reader = Unread::default();
    let error = s3_error(store.put_from(b"v", &mut reader, len).unwrap_err());
    assert!(matches!(error, Error::UploadLimit {
        field: "part allocation", value, minimum: 0, maximum,
    } if value == len && maximum == isize::MAX as u64));
    assert_eq!(reader.reads, 0);
    assert!(server.answered.lock().unwrap().is_empty());
}

/// The planner reports native payload memory for actual workers and the tail.
#[test]
fn upload_plan_accounts_for_size_and_concurrency() {
    let store = kv("http://127.0.0.1:1", "s3://test", SECRET)
        .part_size(5 << 20)
        .concurrency(2);
    for (len, part_bytes, parts, buffer_bytes, multipart) in [
        (0, 0, 1, 0, false),
        (1, 1, 1, 1, false),
        (5 << 20, 5 << 20, 1, 5 << 20, false),
        ((5 << 20) + 1, 5 << 20, 2, (5 << 20) + 1, true),
        ((10 << 20) + 1, 5 << 20, 3, 10 << 20, true),
    ] {
        let plan = store.upload_plan(len).unwrap();
        assert_eq!(plan.part_bytes, part_bytes, "len={len}");
        assert_eq!(plan.parts, parts, "len={len}");
        assert_eq!(plan.buffer_bytes, buffer_bytes, "len={len}");
        assert_eq!(plan.multipart, multipart, "len={len}");
    }
    #[cfg(target_pointer_width = "64")]
    {
        let store = store.part_size(5 << 30).upload_buffer_bytes(u64::MAX);
        assert!(!store.upload_plan(5_000_000_000).unwrap().multipart);
        assert!(store.upload_plan(5_000_000_001).unwrap().multipart);
        let largest = store.upload_plan((5_u64 << 30) * 10_000).unwrap();
        assert_eq!(largest.part_bytes, 5 << 30);
        assert_eq!(largest.parts, 10_000);
        assert_eq!(largest.buffer_bytes, 10 << 30);
        assert!(largest.multipart);
    }
}

/// Budget rejection reads no source and sends neither PUT nor multipart create.
#[test]
fn upload_buffer_rejection_precedes_io() {
    let root = tempfile::tempdir().unwrap();
    let server = serve(root.path());
    for (len, concurrency, required) in [
        (4, 1, 4),
        ((5 << 20) + 1, 1, 5 << 20),
        ((5 << 20) + 1, 2, (5 << 20) + 1),
        ((10 << 20) + 1, 2, 10 << 20),
        ((5_u64 << 30) * 10_000, 1, 5 << 30),
    ] {
        if required > isize::MAX as u64 {
            // The platform allocation limit is checked before this budget.
            continue;
        }
        let available = if required == 5 << 30 { 1 } else { required - 1 };
        let store = kv(&server.endpoint, "s3://test", SECRET)
            .part_size(5 << 20)
            .concurrency(concurrency)
            .upload_buffer_bytes(available);
        for new in [false, true] {
            let mut reader = Unread::default();
            let error = if new {
                store.put_new_from(b"v", &mut reader, len).unwrap_err()
            } else {
                store.put_from(b"v", &mut reader, len).unwrap_err()
            };
            let error = s3_error(error);
            assert!(
                matches!(error, Error::UploadBuffer {
                    required: actual_required, available: actual_available,
                } if (actual_required, actual_available) == (required, available)),
                "len={len}, concurrency={concurrency}, new={new}: {error}"
            );
            assert_eq!(reader.reads, 0);
        }
    }
    // The owned-bytes API uses the same reservation path.
    let store = kv(&server.endpoint, "s3://test", SECRET).upload_buffer_bytes(0);
    for error in [
        store.put(b"v", "x".into()).unwrap_err(),
        store.put_new(b"v", "x".into()).unwrap_err(),
        block_on(store.aput(b"v", "x".into())).unwrap_err(),
        block_on(store.aput_new(b"v", "x".into())).unwrap_err(),
    ] {
        assert!(matches!(
            s3_error(error),
            Error::UploadBuffer {
                required: 1,
                available: 0
            }
        ));
    }
    assert!(server.answered.lock().unwrap().is_empty());
    assert!(uploads(root.path()).is_empty());
    // An empty object needs no payload buffer and is still an actual PUT.
    store.put_from(b"empty", &mut Unread::default(), 0).unwrap();
    assert_eq!(store.get(b"empty").unwrap().unwrap(), Bytes::new());
}

/// A huge declared source starts multipart before reading or allocating it.
/// Small parts and a small budget keep a regression from allocating gigabytes;
/// the planner test separately covers the single-PUT size boundary.
#[test]
fn huge_upload_creates_before_reading_source() {
    let denied = response("403 Forbidden", "<Error><Code>AccessDenied</Code></Error>");
    let (endpoint, server) = fake(vec![denied]);
    let store = kv(&endpoint, "s3://test", SECRET)
        .part_size(5 << 20)
        .concurrency(1)
        .upload_buffer_bytes(5 << 20);
    let mut reader = Unread::default();
    let error = s3_error(
        store
            .put_from(b"v", &mut reader, 5_000_000_001)
            .unwrap_err(),
    );
    assert!(
        matches!(error, Error::Status { status: 403, .. }),
        "{error}"
    );
    assert_eq!(reader.reads, 0);
    let requests = server.join().unwrap();
    assert_eq!(requests.len(), 1);
    assert!(requests[0].starts_with("POST /test/v?"));
    assert!(requests[0].contains("&uploads="));
}

/// Concurrent calls share a store's budget and release it after completion.
#[test]
fn concurrent_uploads_share_buffer_budget() {
    let root = tempfile::tempdir().unwrap();
    let server = serve(root.path());
    let store = Arc::new(kv(&server.endpoint, "s3://test", SECRET).upload_buffer_bytes(4));
    thread::scope(|scope| {
        let (entered, observed) = mpsc::sync_channel(1);
        let (release, proceed) = mpsc::sync_channel(1);
        let first_store = Arc::clone(&store);
        let first = scope.spawn(move || {
            let mut reader = HeldReader {
                entered: Some(entered),
                release: proceed,
            };
            first_store.put_from(b"first", &mut reader, 4)
        });
        observed.recv_timeout(Duration::from_secs(10)).unwrap();
        let mut reader = Unread::default();
        let error = s3_error(store.put_new_from(b"second", &mut reader, 1).unwrap_err());
        assert!(
            matches!(
                error,
                Error::UploadBuffer {
                    required: 1,
                    available: 0
                }
            ),
            "{error}"
        );
        assert_eq!(reader.reads, 0);
        assert!(server.answered.lock().unwrap().is_empty());
        release.send(()).unwrap();
        first.join().unwrap().unwrap();
    });
    assert!(store.put_new(b"second", "done".into()).unwrap());
    assert_eq!(store.get(b"first").unwrap().unwrap(), "xxxx");
    assert_eq!(store.get(b"second").unwrap().unwrap(), "done");
}

/// An upload retains its payload reservation until abort succeeds or fails.
#[test]
fn upload_budget_covers_abort_and_unwind() {
    for denied in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let (entered, observed) = mpsc::sync_channel(1);
        let (release, proceed) = mpsc::sync_channel(1);
        let proceed = Mutex::new(proceed);
        let server = serve_with(root.path(), move |line| {
            if line.starts_with("DELETE") {
                entered.send(()).unwrap();
                proceed
                    .lock()
                    .unwrap()
                    .recv_timeout(Duration::from_secs(10))
                    .unwrap();
                if denied {
                    return Act::Deny;
                }
            }
            Act::Serve
        });
        let store = Arc::new(
            kv(&server.endpoint, "s3://test", SECRET)
                .part_size(5 << 20)
                .concurrency(1)
                .upload_buffer_bytes(5 << 20),
        );
        thread::scope(|scope| {
            let first_store = Arc::clone(&store);
            let first = scope.spawn(move || {
                first_store.put_from(b"failed", &mut Failing { ok: 0 }, (5 << 20) + 1)
            });
            observed.recv_timeout(Duration::from_secs(10)).unwrap();
            let error = s3_error(store.put(b"busy", "x".into()).unwrap_err());
            assert!(matches!(
                error,
                Error::UploadBuffer {
                    required: 1,
                    available: 0
                }
            ));
            release.send(()).unwrap();
            let error = first.join().unwrap().unwrap_err();
            if denied {
                assert!(matches!(s3_error(error), Error::Abort { .. }));
            } else {
                assert!(matches!(error, covalence_kv_trait::Error::Io { .. }));
            }
        });
        store.put(b"after-abort", "x".into()).unwrap();

        // Occupy the entire budget so even one leaked byte fails the retry.
        let store = kv(&server.endpoint, "s3://test", SECRET).upload_buffer_bytes(1);
        struct Panics;
        impl Read for Panics {
            fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
                panic!("source panicked");
            }
        }
        assert!(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                store.put_from(b"panic", &mut Panics, 1)
            }))
            .is_err()
        );
        store.put(b"after-panic", "x".into()).unwrap();
    }
}

/// A writer that always fails.
struct Full;

impl Write for Full {
    fn write(&mut self, _: &[u8]) -> io::Result<usize> {
        Err(io::Error::other("full"))
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Values over the part size go up in parts and come down in ranges, whole
/// or streamed, leaving no upload behind; values up to it go in one PUT.
#[test]
fn multipart() {
    let root = tempfile::tempdir().unwrap();
    let server = serve(root.path());
    let kv = kv(&server.endpoint, "s3://test/p", SECRET)
        .part_size(5 << 20)
        .upload_buffer_bytes(15 << 20);
    let value = Bytes::from(data(11 * MIB + 3));
    kv.put(b"v", value.clone()).unwrap();
    assert_eq!(kv.get(b"v").unwrap().unwrap(), value);
    assert_eq!(kv.size(b"v").unwrap(), Some(value.len() as u64));
    let mut other: Vec<u8> = data(10 * MIB + 1);
    other.reverse();
    for (key, len) in [(&b"w"[..], other.len()), (b"x", 5 * MIB), (b"e", 0)] {
        // The reader has more; only `len` bytes are stored.
        kv.put_from(key, &mut &other[..], len as u64).unwrap();
        let mut out = Vec::new();
        assert_eq!(kv.get_to(key, &mut out).unwrap(), Some(len as u64));
        assert_eq!(out, other[..len]);
    }
    let mut out = Vec::new();
    assert_eq!(kv.get_to(b"missing", &mut out).unwrap(), None);
    assert!(out.is_empty());
    let error = kv.get_to(b"v", &mut Full).unwrap_err();
    assert!(
        matches!(error, covalence_kv_trait::Error::Io { .. }),
        "{error}"
    );
    assert_eq!(uploads(root.path()), Vec::<String>::new());
}

/// A multipart upload whose reader fails or ends early is aborted, storing
/// nothing.
#[test]
fn multipart_aborts_on_failure() {
    let root = tempfile::tempdir().unwrap();
    let server = serve(root.path());
    let kv = kv(&server.endpoint, "s3://test", SECRET)
        .part_size(5 << 20)
        .upload_buffer_bytes(15 << 20);
    let len = 11 * MIB as u64;
    let error = kv.put_from(b"v", &mut Failing { ok: 6 * MIB }, len);
    assert!(
        matches!(&error, Err(covalence_kv_trait::Error::Io { source }) if source.to_string() == "disk"),
        "{error:?}"
    );
    let short = data(7 * MIB);
    let error = kv.put_from(b"v", &mut &short[..], len);
    assert!(
        matches!(&error, Err(covalence_kv_trait::Error::Io { source }) if source.kind() == io::ErrorKind::UnexpectedEof),
        "{error:?}"
    );
    assert_eq!(kv.get(b"v").unwrap(), None);
    assert_eq!(uploads(root.path()), Vec::<String>::new());
}

/// A new value goes only under a free key, in one PUT or a multipart upload;
/// an upload that finds the key taken is aborted, leaving the value as it was.
#[test]
fn new_values_never_replace() {
    let root = tempfile::tempdir().unwrap();
    let server = serve(root.path());
    let kv = kv(&server.endpoint, "s3://test/p", SECRET)
        .part_size(5 << 20)
        .upload_buffer_bytes(15 << 20);
    assert!(kv.put_new(b"small", "one".into()).unwrap());
    assert!(!kv.put_new(b"small", "two".into()).unwrap());
    assert!(!block_on(kv.aput_new(b"small", "two".into())).unwrap());
    let big = data(11 * MIB);
    let len = big.len() as u64;
    assert!(kv.put_new_from(b"big", &mut &big[..], len).unwrap());
    let mut other = big.clone();
    other.reverse();
    assert!(!kv.put_new_from(b"big", &mut &other[..], len).unwrap());
    assert!(!kv.put_new_from(b"small", &mut &other[..], len).unwrap());
    assert_eq!(kv.get(b"small").unwrap().unwrap(), "one");
    assert_eq!(kv.get(b"big").unwrap().unwrap(), big);
    assert_eq!(uploads(root.path()), Vec::<String>::new());
}

/// A new value signs and sends `If-None-Match: *` with its PUT, or with the
/// request completing its upload; 412 there means the key is taken, and the
/// upload is aborted. To a plain put, 412 is a failure.
#[test]
fn new_value_requests() {
    let taken = || {
        response(
            "412 Precondition Failed",
            "<Error><Code>PreconditionFailed</Code></Error>",
        )
    };
    let (endpoint, server) = fake(vec![
        taken(),
        created(),
        with_header("200 OK", "ETag: \"e1\"", ""),
        with_header("200 OK", "ETag: \"e2\"", ""),
        with_header("200 OK", "ETag: \"e3\"", ""),
        taken(),
        response("204 No Content", ""),
        taken(),
    ]);
    // The fake server answers requests in turn, so send them one at a time.
    let kv = kv(&endpoint, "s3://test", SECRET)
        .part_size(5 << 20)
        .concurrency(1)
        .upload_buffer_bytes(5 << 20);
    assert!(!kv.put_new(b"a", "01".into()).unwrap());
    assert!(!kv.put_new(b"a", multipart_ascii().into()).unwrap());
    let error = s3_error(kv.put(b"a", "01".into()).unwrap_err());
    assert!(
        matches!(error, Error::Status { status: 412, .. }),
        "{error}"
    );
    let requests = server.join().unwrap();
    let methods: Vec<_> = requests
        .iter()
        .map(|request| request.split_once(' ').unwrap().0)
        .collect();
    let expected = ["PUT", "POST", "PUT", "PUT", "PUT", "POST", "DELETE", "PUT"];
    assert_eq!(methods, expected);
    for (index, request) in requests.iter().enumerate() {
        let conditional = matches!(index, 0 | 5);
        let signed = request
            .split_once(" HTTP/1.1")
            .unwrap()
            .0
            .contains("if-none-match");
        assert_eq!(
            request.contains("\r\nif-none-match: *\r\n"),
            conditional,
            "{request}"
        );
        assert_eq!(signed, conditional, "{request}");
    }
    assert!(requests[5].contains("uploadId=u1"), "{}", requests[5]);
    assert!(requests[6].contains("uploadId=u1"), "{}", requests[6]);
}

/// A response with `status`, `header`, closing the connection, and `body`.
fn with_header(status: &str, header: &str, body: &str) -> String {
    response(status, body).replace(
        "Connection: close",
        &format!("{header}\r\nConnection: close"),
    )
}

/// CreateMultipartUpload's reply, for upload `u1`.
fn created() -> String {
    response(
        "200 OK",
        r#"<InitiateMultipartUploadResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/"><UploadId>u1</UploadId></InitiateMultipartUploadResult>"#,
    )
}

/// The base64 SHA-256 of `bytes`.
fn sha256(bytes: &[u8]) -> String {
    use sha2::Digest;
    data_encoding::BASE64.encode(&sha2::Sha256::digest(bytes))
}

/// A multipart upload declares SHA-256 checksums, sends one with each part
/// and lists them on completion; S3 failing it with a 200 aborts it.
#[test]
fn multipart_requests() {
    let failed = response("200 OK", "<Error><Code>InternalError</Code></Error>");
    let (endpoint, server) = fake(vec![
        created(),
        with_header("200 OK", "ETag: \"e1\"", ""),
        with_header("200 OK", "ETag: \"e2\"", ""),
        with_header("200 OK", "ETag: \"e3\"", ""),
        failed,
        response("204 No Content", ""),
    ]);
    // The fake server answers requests in turn, so send them one at a time.
    let kv = kv(&endpoint, "s3://test", SECRET)
        .part_size(5 << 20)
        .concurrency(1)
        .upload_buffer_bytes(5 << 20);
    let error = s3_error(kv.put(b"a", multipart_ascii().into()).unwrap_err());
    assert!(
        matches!(&error, Error::Status { status: 200, code: Some(code) } if code == "InternalError"),
        "{error}"
    );
    let requests = server.join().unwrap();
    assert_eq!(requests.len(), 6);
    assert!(requests[0].starts_with("POST /test/a?"), "{}", requests[0]);
    assert!(requests[0].contains("&uploads="), "{}", requests[0]);
    assert!(requests[0].contains("x-amz-checksum-algorithm: SHA256"));
    let mut complete = String::from("<CompleteMultipartUpload>");
    let value = multipart_ascii();
    for (offset, part) in value.as_bytes().chunks(5 * MIB).enumerate() {
        let number = offset + 1;
        let request = &requests[number];
        assert!(request.starts_with("PUT /test/a?"), "{request}");
        assert!(
            request.contains(&format!("partNumber={number}&uploadId=u1")),
            "{request}"
        );
        let checksum = sha256(part);
        assert!(request.contains(&format!("x-amz-checksum-sha256: {checksum}")));
        assert_eq!(request.split_once("\r\n\r\n").unwrap().1.as_bytes(), part);
        complete += &format!(
            "<Part><ETag>\"e{number}\"</ETag><ChecksumSHA256>{checksum}</ChecksumSHA256>\
             <PartNumber>{number}</PartNumber></Part>"
        );
    }
    complete += "</CompleteMultipartUpload>";
    assert!(requests[4].starts_with("POST /test/a?"), "{}", requests[4]);
    assert!(requests[4].contains("uploadId=u1"));
    assert!(requests[4].ends_with(&complete), "{}", requests[4]);
    assert!(
        requests[5].starts_with("DELETE /test/a?"),
        "{}",
        requests[5]
    );
    assert!(requests[5].contains("uploadId=u1"));
}

/// A failed part aborts the upload, and a failed abort is reported with
/// the failure that caused it.
#[test]
fn multipart_abort_failures() {
    let denied = || response("403 Forbidden", "<Error><Code>AccessDenied</Code></Error>");
    let (endpoint, server) = fake(vec![
        created(),
        with_header("200 OK", "ETag: \"e1\"", ""),
        denied(),
        response("204 No Content", ""),
        created(),
        denied(),
        denied(),
    ]);
    // The fake server answers requests in turn, so send them one at a time.
    let kv = kv(&endpoint, "s3://test", SECRET)
        .part_size(5 << 20)
        .concurrency(1)
        .upload_buffer_bytes(5 << 20);
    let error = s3_error(kv.put(b"a", multipart_ascii().into()).unwrap_err());
    assert!(
        matches!(&error, Error::Status { status: 403, code: Some(code) } if code == "AccessDenied"),
        "{error}"
    );
    let error = s3_error(kv.put(b"a", multipart_ascii().into()).unwrap_err());
    let Error::Abort {
        upload_id,
        cause,
        source,
    } = error
    else {
        panic!("{error}");
    };
    assert_eq!(upload_id, "u1");
    assert!(
        matches!(*cause, Error::Status { status: 403, .. }),
        "{cause}"
    );
    assert!(
        matches!(*source, Error::Status { status: 403, .. }),
        "{source}"
    );
    let requests = server.join().unwrap();
    let methods: Vec<_> = requests
        .iter()
        .map(|request| request.split_once(' ').unwrap().0)
        .collect();
    let expected = ["POST", "PUT", "PUT", "DELETE", "POST", "PUT", "DELETE"];
    assert_eq!(methods, expected);
}

/// Values come down in ranges of the part size, all of one value: the same
/// size and ETag, which a value of more than one part must have.
#[test]
fn reads_are_of_one_value() {
    let part = |range: &str, body: &str, etag: &str| {
        partial(range, body).replace(
            "Connection: close",
            &format!("ETag: {etag}\r\nConnection: close"),
        )
    };
    let (endpoint, server) = fake(vec![
        part("bytes 0-3/10", "0123", "\"a\""),
        part("bytes 4-7/10", "4567", "\"a\""),
        part("bytes 8-9/10", "89", "\"a\""),
        part("bytes 0-3/10", "0123", "\"a\""),
        part("bytes 4-7/10", "4567", "\"b\""),
        part("bytes 0-3/10", "0123", "\"a\""),
        part("bytes 4-7/11", "4567", "\"a\""),
        part("bytes 0-3/10", "0123", "\"a\""),
        response("404 Not Found", ""),
        partial("bytes 0-3/10", "0123"),
        partial("bytes 0-3/4", "0123"),
    ]);
    // The fake server answers requests in turn, so send them one at a time.
    let kv = kv(&endpoint, "s3://test", SECRET)
        .part_size(4)
        .concurrency(1);
    assert_eq!(kv.get(b"a").unwrap().unwrap(), "0123456789");
    for _ in 0..3 {
        let mut out = Vec::new();
        let error = s3_error(kv.get_to(b"a", &mut out).unwrap_err());
        assert!(matches!(error, Error::Changed), "{error}");
    }
    // Without an ETag, a value can only be read in one part.
    let error = s3_error(kv.get(b"a").unwrap_err());
    assert!(
        matches!(error, Error::MissingField { field: "ETag" }),
        "{error}"
    );
    assert_eq!(kv.get(b"a").unwrap().unwrap(), "0123");
    let requests = server.join().unwrap();
    let ranges: Vec<_> = requests
        .iter()
        .map(|request| {
            let lower = request.to_ascii_lowercase();
            let range = lower.lines().find(|line| line.starts_with("range: "));
            range.unwrap().to_owned()
        })
        .collect();
    assert_eq!(
        ranges[..3],
        ["range: bytes=0-3", "range: bytes=4-7", "range: bytes=8-11"]
    );
}

/// The position of the first answered request containing all of `parts`.
fn answered(server: &Server, parts: &[&str]) -> usize {
    let answered = server.answered.lock().unwrap();
    answered
        .iter()
        .position(|line| parts.iter().all(|part| line.contains(part)))
        .unwrap_or_else(|| panic!("no request with {parts:?} in {answered:#?}"))
}

/// Parts go up and ranges come down several at a time, never more than the
/// concurrency, and a slow first one still yields the value in order.
#[test]
fn parts_move_concurrently_in_order() {
    let root = tempfile::tempdir().unwrap();
    let server = serve_with(root.path(), |line| {
        if line.contains("partNumber=1&") {
            Act::After("partNumber=2&")
        } else if line.ends_with(" bytes=65536-131071") {
            Act::After("bytes=131072-")
        } else {
            Act::Serve
        }
    });
    let kv = kv(&server.endpoint, "s3://test", SECRET)
        .part_size(5 << 20)
        .concurrency(3);
    let value = data(36 * MIB + 7);
    kv.put_from(b"v", &mut &value[..], value.len() as u64)
        .unwrap();
    assert_eq!(uploads(root.path()), Vec::<String>::new());
    let parts = server.peak.swap(0, Ordering::SeqCst);
    assert!((2..=3).contains(&parts), "{parts} parts at once");
    assert!(answered(&server, &["partNumber=2&"]) < answered(&server, &["partNumber=1&"]));

    let kv = kv.part_size(64 << 10).concurrency(8);
    let mut out = Vec::new();
    assert_eq!(kv.get_to(b"v", &mut out).unwrap(), Some(value.len() as u64));
    assert!(out == value, "the value came down out of order");
    assert_eq!(kv.get(b"v").unwrap().unwrap(), value);
    let ranges = server.peak.load(Ordering::SeqCst);
    assert!((2..=8).contains(&ranges), "{ranges} ranges at once");
    assert!(answered(&server, &["bytes=131072-"]) < answered(&server, &["bytes=65536-"]));
}

/// A part failing while others are in flight aborts the upload once they
/// finish, leaving neither the value nor its parts behind.
#[test]
fn failed_part_aborts_after_parts_in_flight() {
    let root = tempfile::tempdir().unwrap();
    let server = serve_with(root.path(), |line| {
        if line.contains("partNumber=3&") {
            Act::Deny
        } else if line.contains("partNumber=") && !line.contains("partNumber=1&") {
            Act::After("partNumber=3&")
        } else {
            Act::Serve
        }
    });
    let kv = kv(&server.endpoint, "s3://test", SECRET)
        .part_size(5 << 20)
        .concurrency(4)
        .upload_buffer_bytes(20 << 20);
    let value = data(36 * MIB);
    let error = s3_error(
        kv.put_from(b"v", &mut &value[..], value.len() as u64)
            .unwrap_err(),
    );
    assert!(
        matches!(&error, Error::Status { status: 403, code: Some(code) } if code == "AccessDenied"),
        "{error}"
    );
    assert_eq!(kv.get(b"v").unwrap(), None);
    assert_eq!(uploads(root.path()), Vec::<String>::new());
    let answered = server.answered.lock().unwrap();
    let last_part = answered
        .iter()
        .rposition(|line| line.contains("partNumber="));
    let abort = answered.iter().position(|line| line.starts_with("DELETE"));
    assert!(abort > last_part, "{answered:#?}");
    drop(answered);
    kv.put(b"after-failure", "x".into()).unwrap();
}

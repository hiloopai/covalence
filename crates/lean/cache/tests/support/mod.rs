use std::{
    fs,
    net::TcpListener,
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
};

use hyper_util::{
    rt::{TokioExecutor, TokioIo},
    server::conn::auto::Builder,
};
use s3s::{auth::SimpleAuth, service::S3ServiceBuilder};
use s3s_fs::FileSystem;
use tokio::sync::oneshot;

pub struct Server {
    pub endpoint: String,
    pub writes: Arc<Mutex<Vec<String>>>,
    pub reads: Arc<Mutex<Vec<String>>>,
    pub deny: Arc<AtomicBool>,
    stop: Option<oneshot::Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

impl Server {
    pub fn new(root: &Path) -> Self {
        fs::create_dir_all(root.join("bucket")).unwrap();
        let mut builder = S3ServiceBuilder::new(FileSystem::new(root).unwrap());
        builder.set_auth(SimpleAuth::from_single("test-key", "test-secret"));
        let service = builder.build();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let writes = Arc::new(Mutex::new(Vec::new()));
        let reads = Arc::new(Mutex::new(Vec::new()));
        let deny = Arc::new(AtomicBool::new(false));
        let (log, read_log, fail) = (writes.clone(), reads.clone(), deny.clone());
        let handle = move |request: hyper::Request<hyper::body::Incoming>| {
            let (service, log, read_log, fail) =
                (service.clone(), log.clone(), read_log.clone(), fail.clone());
            async move {
                let write = request.method() == "PUT";
                if write {
                    log.lock().unwrap().push(request.uri().path().to_owned());
                } else if request.method() == "GET" {
                    read_log
                        .lock()
                        .unwrap()
                        .push(request.uri().path().to_owned());
                }
                if write && fail.load(Ordering::SeqCst) {
                    return Ok(hyper::Response::builder()
                        .status(403)
                        .body("<Error><Code>AccessDenied</Code></Error>".to_owned().into())
                        .unwrap());
                }
                service.call(request.map(s3s::Body::from)).await
            }
        };
        let (stop, stopped) = oneshot::channel();
        let thread = thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
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
        Self {
            endpoint,
            writes,
            reads,
            deny,
            stop: Some(stop),
            thread: Some(thread),
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.stop.take().unwrap().send(());
        self.thread.take().unwrap().join().unwrap();
    }
}

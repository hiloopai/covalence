//! Host name lookups shared by every request of a store.
//!
//! ureq resolves a request's host before it looks in its pool, so every
//! request costs a lookup, even on a reused connection, and S3 closes a
//! connection after about 100 requests anyway. Under a Kubernetes resolver
//! with `ndots:5` one lookup of an S3 endpoint is up to ten queries, each a new
//! UDP flow, so a client reading thousands of objects a second floods the
//! cluster's DNS. [`Reusing`] looks a host up once, gives its addresses to every
//! request for a set period, and lets requests that ask while a lookup is in
//! flight share its answer.

use std::{
    collections::HashMap,
    io,
    sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError},
    time::{Duration, Instant},
};

use ureq::{
    config::Config,
    http::Uri,
    unversioned::{
        resolver::{ResolvedSocketAddrs, Resolver},
        transport::{NextTimeout, time},
    },
};

/// A resolver that reuses `R`'s answer for each host for a period.
///
/// - A host's addresses are reused for `period` after its lookup succeeds; a
///   zero period looks the host up for every request.
/// - Requests that ask while a lookup of their host is in flight wait for its
///   answer, success or failure, instead of sending their own. Each waits only
///   as long as its own timeout, then fails with [`ureq::Error::Timeout`].
/// - A failed lookup is not reused: the next request after it looks again.
#[derive(Debug)]
pub(crate) struct Reusing<R> {
    resolver: R,
    period: Duration,
    hosts: Mutex<HashMap<String, Host>>,
}

/// What is known about one host.
#[derive(Debug)]
enum Host {
    /// Addresses looked up at `at`.
    Found {
        at: Instant,
        addrs: Box<ResolvedSocketAddrs>,
    },
    /// A lookup in flight.
    Pending(Arc<Lookup>),
}

/// One lookup's answer, once it lands.
#[derive(Debug, Default)]
struct Lookup {
    answer: Mutex<Option<Result<ResolvedSocketAddrs, Arc<ureq::Error>>>>,
    landed: Condvar,
}

/// The caller's part in a lookup.
enum Role {
    Lead(Arc<Lookup>),
    Wait(Arc<Lookup>),
}

impl<R: Resolver> Reusing<R> {
    /// Look hosts up with `resolver`, reusing each answer for `period`.
    pub(crate) fn new(resolver: R, period: Duration) -> Self {
        Self {
            resolver,
            period,
            hosts: Mutex::default(),
        }
    }

    fn hosts(&self) -> MutexGuard<'_, HashMap<String, Host>> {
        self.hosts.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The reused addresses of `host`, or the caller's part in looking it up.
    fn role(&self, host: &str) -> Result<ResolvedSocketAddrs, Role> {
        let mut hosts = self.hosts();
        match hosts.get(host) {
            Some(Host::Found { at, addrs }) if at.elapsed() < self.period => Ok(*addrs.clone()),
            Some(Host::Pending(lookup)) => Err(Role::Wait(lookup.clone())),
            _ => {
                let lookup = Arc::new(Lookup::default());
                hosts.insert(host.to_owned(), Host::Pending(lookup.clone()));
                Err(Role::Lead(lookup))
            }
        }
    }

    /// Give `answer` to the lookup's waiters and record it for `host`.
    fn land(
        &self,
        host: &str,
        lookup: &Arc<Lookup>,
        answer: Result<ResolvedSocketAddrs, Arc<ureq::Error>>,
    ) {
        let mut hosts = self.hosts();
        if matches!(hosts.get(host), Some(Host::Pending(own)) if Arc::ptr_eq(own, lookup)) {
            match &answer {
                Ok(addrs) => {
                    let addrs = Box::new(addrs.clone());
                    let at = Instant::now();
                    hosts.insert(host.to_owned(), Host::Found { at, addrs });
                }
                Err(_) => {
                    hosts.remove(host);
                }
            }
        }
        drop(hosts);
        *lookup.answer.lock().unwrap_or_else(PoisonError::into_inner) = Some(answer);
        lookup.landed.notify_all();
    }
}

impl<R: Resolver> Resolver for Reusing<R> {
    fn resolve(
        &self,
        uri: &Uri,
        config: &Config,
        timeout: NextTimeout,
    ) -> Result<ResolvedSocketAddrs, ureq::Error> {
        let host = uri.authority().map_or("", |authority| authority.as_str());
        match self.role(host) {
            Ok(addrs) => Ok(addrs),
            Err(Role::Wait(lookup)) => lookup.wait(timeout),
            Err(Role::Lead(lookup)) => {
                let landing = Landing {
                    reusing: self,
                    host,
                    lookup: &lookup,
                    landed: false,
                };
                let result = self.resolver.resolve(uri, config, timeout);
                landing.land(match &result {
                    Ok(addrs) => Ok(addrs.clone()),
                    Err(error) => Err(Arc::new(copy(error))),
                });
                result
            }
        }
    }
}

impl Lookup {
    /// The lookup's answer, or a timeout if it has not landed within `timeout`.
    fn wait(&self, timeout: NextTimeout) -> Result<ResolvedSocketAddrs, ureq::Error> {
        let answer = self.answer.lock().unwrap_or_else(PoisonError::into_inner);
        let pending = |answer: &mut Option<_>| answer.is_none();
        let answer = match timeout.after {
            time::Duration::Exact(after) => {
                self.landed
                    .wait_timeout_while(answer, after, pending)
                    .unwrap_or_else(PoisonError::into_inner)
                    .0
            }
            time::Duration::NotHappening => self
                .landed
                .wait_while(answer, pending)
                .unwrap_or_else(PoisonError::into_inner),
        };
        match answer.as_ref() {
            Some(Ok(addrs)) => Ok(addrs.clone()),
            Some(Err(error)) => Err(copy(error)),
            None => Err(ureq::Error::Timeout(timeout.reason)),
        }
    }
}

/// The leader's duty to land its lookup, done on drop if the lookup panics,
/// so its waiters and later requests never wait on a lookup no one finishes.
struct Landing<'a, R: Resolver> {
    reusing: &'a Reusing<R>,
    host: &'a str,
    lookup: &'a Arc<Lookup>,
    landed: bool,
}

impl<R: Resolver> Landing<'_, R> {
    fn land(mut self, answer: Result<ResolvedSocketAddrs, Arc<ureq::Error>>) {
        self.landed = true;
        self.reusing.land(self.host, self.lookup, answer);
    }
}

impl<R: Resolver> Drop for Landing<'_, R> {
    fn drop(&mut self) {
        if !self.landed {
            let error = io::Error::other("the host lookup panicked");
            let answer = Err(Arc::new(ureq::Error::Io(error)));
            self.reusing.land(self.host, self.lookup, answer);
        }
    }
}

/// `error` again, for each request that shares a failed lookup. Resolvers fail
/// with the variants kept as they are; any other keeps its message.
fn copy(error: &ureq::Error) -> ureq::Error {
    match error {
        ureq::Error::HostNotFound => ureq::Error::HostNotFound,
        ureq::Error::Timeout(reason) => ureq::Error::Timeout(*reason),
        ureq::Error::BadUri(uri) => ureq::Error::BadUri(uri.clone()),
        ureq::Error::Io(error) => ureq::Error::Io(io::Error::new(error.kind(), error.to_string())),
        error => ureq::Error::Io(io::Error::other(error.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use std::{
        io::{BufRead, BufReader, Write},
        net::{SocketAddr, TcpListener},
        sync::{
            Barrier,
            atomic::{AtomicUsize, Ordering},
        },
        thread,
    };

    use ureq::unversioned::{resolver::DefaultResolver, transport::DefaultConnector};

    use super::*;

    /// What a [`Fake`] lookup does after its delay.
    #[derive(Debug, Clone, Copy)]
    enum Outcome {
        Found,
        Fails,
    }

    /// A resolver that takes `delay` (or the caller's timeout, if shorter) and
    /// counts its lookups and the most in flight at once.
    #[derive(Debug, Default)]
    struct Fake {
        delay: Duration,
        outcome: Option<Outcome>,
        lookups: AtomicUsize,
        flying: AtomicUsize,
        peak: AtomicUsize,
    }

    impl Fake {
        fn new(delay: Duration, outcome: Outcome) -> Self {
            Self {
                delay,
                outcome: Some(outcome),
                ..Self::default()
            }
        }
    }

    impl Resolver for &'static Fake {
        fn resolve(
            &self,
            _uri: &Uri,
            _config: &Config,
            timeout: NextTimeout,
        ) -> Result<ResolvedSocketAddrs, ureq::Error> {
            self.lookups.fetch_add(1, Ordering::SeqCst);
            let flying = self.flying.fetch_add(1, Ordering::SeqCst) + 1;
            self.peak.fetch_max(flying, Ordering::SeqCst);
            thread::sleep(self.delay.min(*timeout.after));
            self.flying.fetch_sub(1, Ordering::SeqCst);
            if self.delay > *timeout.after {
                return Err(ureq::Error::Timeout(timeout.reason));
            }
            match self.outcome {
                Some(Outcome::Found) => {
                    let mut addrs = self.empty();
                    addrs.push(SocketAddr::from(([127, 0, 0, 1], 443)));
                    Ok(addrs)
                }
                _ => Err(ureq::Error::Io(io::Error::other(
                    "failed to lookup address information: Temporary failure in name resolution",
                ))),
            }
        }
    }

    fn after(millis: u64) -> NextTimeout {
        NextTimeout {
            after: time::Duration::from_millis(millis),
            reason: ureq::Timeout::Resolve,
        }
    }

    /// `callers` threads resolving the same host at once, each with a
    /// `timeout_ms` budget: each one's result and how long it took.
    fn race(
        reusing: &Reusing<&'static Fake>,
        callers: usize,
        timeout_ms: u64,
    ) -> Vec<(Result<ResolvedSocketAddrs, ureq::Error>, Duration)> {
        let uri: Uri = "https://s3.example.com/".parse().unwrap();
        let config = Config::default();
        let start = Barrier::new(callers);
        thread::scope(|scope| {
            let callers: Vec<_> = (0..callers)
                .map(|_| {
                    scope.spawn(|| {
                        start.wait();
                        let began = Instant::now();
                        let result = reusing.resolve(&uri, &config, after(timeout_ms));
                        (result, began.elapsed())
                    })
                })
                .collect();
            callers
                .into_iter()
                .map(|caller| caller.join().unwrap())
                .collect()
        })
    }

    fn fake(delay_ms: u64, outcome: Outcome) -> &'static Fake {
        Box::leak(Box::new(Fake::new(
            Duration::from_millis(delay_ms),
            outcome,
        )))
    }

    #[test]
    fn callers_share_one_lookup() {
        let fake = fake(200, Outcome::Found);
        let reusing = Reusing::new(fake, Duration::from_secs(60));
        for (result, _) in race(&reusing, 16, 5_000) {
            assert_eq!(result.unwrap().len(), 1);
        }
        assert_eq!(fake.lookups.load(Ordering::SeqCst), 1);
        assert_eq!(fake.peak.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn a_slow_lookup_holds_no_caller_past_its_timeout() {
        let fake = fake(3_000, Outcome::Found);
        let reusing = Reusing::new(fake, Duration::from_secs(60));
        for (result, took) in race(&reusing, 16, 200) {
            assert!(matches!(result, Err(ureq::Error::Timeout(_))), "{result:?}");
            assert!(took < Duration::from_millis(1_000), "{took:?}");
        }
        assert_eq!(fake.peak.load(Ordering::SeqCst), 1);
        assert_eq!(fake.lookups.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn callers_share_a_failure_and_the_next_caller_looks_again() {
        let fake = fake(200, Outcome::Fails);
        let reusing = Reusing::new(fake, Duration::from_secs(60));
        for (result, took) in race(&reusing, 16, 5_000) {
            let error = result.unwrap_err().to_string();
            assert!(
                error.contains("Temporary failure in name resolution"),
                "{error}"
            );
            assert!(took < Duration::from_millis(1_000), "{took:?}");
        }
        assert_eq!(fake.lookups.load(Ordering::SeqCst), 1);
        assert_eq!(fake.peak.load(Ordering::SeqCst), 1);
        race(&reusing, 1, 5_000);
        assert_eq!(fake.lookups.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn addresses_are_reused_for_the_period_only() {
        let fake = fake(0, Outcome::Found);
        let reusing = Reusing::new(fake, Duration::from_millis(300));
        race(&reusing, 1, 1_000);
        race(&reusing, 1, 1_000);
        assert_eq!(fake.lookups.load(Ordering::SeqCst), 1);
        thread::sleep(Duration::from_millis(400));
        race(&reusing, 1, 1_000);
        assert_eq!(fake.lookups.load(Ordering::SeqCst), 2);

        let always = self::fake(0, Outcome::Found);
        let every = Reusing::new(always, Duration::ZERO);
        race(&every, 1, 1_000);
        race(&every, 1, 1_000);
        assert_eq!(always.lookups.load(Ordering::SeqCst), 2);
    }

    /// The default resolver, counting its lookups.
    #[derive(Debug, Default)]
    struct Counting(Arc<AtomicUsize>);

    impl Resolver for Counting {
        fn resolve(
            &self,
            uri: &Uri,
            config: &Config,
            timeout: NextTimeout,
        ) -> Result<ResolvedSocketAddrs, ureq::Error> {
            self.0.fetch_add(1, Ordering::Relaxed);
            DefaultResolver::default().resolve(uri, config, timeout)
        }
    }

    /// A server answering every request with an empty 200 and closing the
    /// connection, as S3 does every so often; it counts the connections.
    fn serve() -> (u16, Arc<AtomicUsize>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let accepted = Arc::new(AtomicUsize::new(0));
        let counter = accepted.clone();
        thread::spawn(move || {
            for stream in listener.incoming() {
                let mut stream = stream.unwrap();
                counter.fetch_add(1, Ordering::Relaxed);
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                while reader.read_line(&mut line).unwrap_or(0) > 0 && line != "\r\n" {
                    line.clear();
                }
                let reply = b"HTTP/1.1 200 OK\r\nconnection: close\r\ncontent-length: 0\r\n\r\n";
                let _ = stream.write_all(reply);
            }
        });
        (port, accepted)
    }

    #[test]
    fn many_connections_cost_one_lookup() {
        let (port, accepted) = serve();
        let lookups = Arc::new(AtomicUsize::new(0));
        let config = ureq::Agent::config_builder()
            .proxy(None)
            .ip_family(ureq::config::IpFamily::Ipv4Only)
            .build();
        let resolver = Reusing::new(Counting(lookups.clone()), Duration::from_secs(60));
        let agent = ureq::Agent::with_parts(config, DefaultConnector::default(), resolver);
        for _ in 0..20 {
            let response = agent
                .get(format!("http://localhost:{port}/"))
                .call()
                .unwrap();
            assert_eq!(response.status(), 200);
            response.into_body().read_to_vec().unwrap();
        }
        assert_eq!(accepted.load(Ordering::Relaxed), 20);
        assert_eq!(lookups.load(Ordering::Relaxed), 1);
    }
}

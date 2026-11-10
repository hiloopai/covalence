//! Whether to reach the pool through S3 Transfer Acceleration: only outside
//! AWS, where every transfer crosses the internet. Inside AWS the regional
//! endpoint is cheaper and acceleration would only add its per-GB fee.

use std::{
    env,
    io::{Read, Write},
    net::{SocketAddr, TcpStream},
    sync::OnceLock,
    time::{Duration, Instant},
};

use crate::error::{Result, invalid};

/// `on`, `off` or `auto`, the default: accelerate unless running in AWS.
pub(crate) const VARIABLE: &str = "COVALENCE_S3_ACCELERATE";

/// The instance metadata probe's whole budget. AWS's answers in about a
/// millisecond; off AWS the address usually goes unanswered, so this bounds
/// what detection costs there.
const PROBE: Duration = Duration::from_millis(40);

/// Whether this process accelerates. Detection runs at most once.
pub(crate) fn enabled() -> Result<bool> {
    static IN_AWS: OnceLock<bool> = OnceLock::new();
    choose(env::var(VARIABLE).ok().as_deref(), || {
        if let Some(in_aws) = IN_AWS.get() {
            return Ok(*in_aws);
        }
        let lookup = |name: &str| env::var(name).ok();
        let in_aws = aws_runtime(lookup) || metadata(lookup)?.is_some_and(|at| imds(at, PROBE));
        Ok(*IN_AWS.get_or_init(|| in_aws))
    })
}

/// The decision for a [`VARIABLE`] setting, asking `in_aws` only for `auto`.
fn choose(setting: Option<&str>, in_aws: impl FnOnce() -> Result<bool>) -> Result<bool> {
    match setting {
        Some("on") => Ok(true),
        Some("off") => Ok(false),
        None | Some("auto") => Ok(!in_aws()?),
        Some(other) => Err(invalid(format!(
            "{VARIABLE} must be auto, on or off, not {other:?}"
        ))),
    }
}

/// Whether variables that only AWS's own runtimes set are present: Lambda,
/// ECS and CodeBuild (`AWS_EXECUTION_ENV`), ECS task metadata and roles, and
/// the EKS Pod Identity agent. Web identity variables do not count: CI and
/// other clouds federate into AWS that way too.
fn aws_runtime(var: impl Fn(&str) -> Option<String>) -> bool {
    let set = |name| var(name).is_some_and(|value| !value.is_empty());
    set("AWS_EXECUTION_ENV")
        || set("ECS_CONTAINER_METADATA_URI_V4")
        || set("ECS_CONTAINER_METADATA_URI")
        || set("AWS_CONTAINER_CREDENTIALS_RELATIVE_URI")
        || var("AWS_CONTAINER_CREDENTIALS_FULL_URI").is_some_and(|uri| {
            uri.starts_with("http://169.254.170.23/") || uri.starts_with("http://[fd00:ec2::23]/")
        })
}

/// The instance metadata address, as the AWS SDKs configure it, or `None`
/// with `AWS_EC2_METADATA_DISABLED=true`.
fn metadata(var: impl Fn(&str) -> Option<String>) -> Result<Option<SocketAddr>> {
    if var("AWS_EC2_METADATA_DISABLED").is_some_and(|value| value.eq_ignore_ascii_case("true")) {
        return Ok(None);
    }
    let Some(endpoint) = var("AWS_EC2_METADATA_SERVICE_ENDPOINT") else {
        return Ok(Some(SocketAddr::from(([169, 254, 169, 254], 80))));
    };
    // An IP address only: resolving a name could cost more than the probe.
    let authority = endpoint
        .strip_prefix("http://")
        .map(|rest| rest.trim_end_matches('/'));
    authority
        .and_then(|authority| {
            authority
                .parse()
                .or_else(|_| format!("{authority}:80").parse())
                .ok()
        })
        .map(Some)
        .ok_or_else(|| {
            invalid(format!(
                "AWS_EC2_METADATA_SERVICE_ENDPOINT must be http://<ip>[:port], not {endpoint:?}"
            ))
        })
}

/// Whether the metadata service at `address` answers within `budget` as only
/// AWS's does, with `Server: EC2ws`. Any status counts, so hosts that require
/// IMDSv2 and containers beyond its token hop limit count too. No answer, or
/// another cloud's metadata service, means not in AWS.
fn imds(address: SocketAddr, budget: Duration) -> bool {
    let deadline = Instant::now() + budget;
    let left = || Some(deadline.saturating_duration_since(Instant::now())).filter(|d| !d.is_zero());
    let Ok(mut stream) = TcpStream::connect_timeout(&address, budget) else {
        return false;
    };
    let request = format!("GET /latest/meta-data/ HTTP/1.0\r\nHost: {address}\r\n\r\n");
    if stream.set_write_timeout(left()).is_err() || stream.write_all(request.as_bytes()).is_err() {
        return false;
    }
    let mut head = Vec::new();
    let mut buffer = [0; 1024];
    while !head.windows(4).any(|end| end == b"\r\n\r\n") && head.len() < 8192 {
        let Some(timeout) = left() else {
            return false;
        };
        match stream
            .set_read_timeout(Some(timeout))
            .and_then(|()| stream.read(&mut buffer))
        {
            Ok(0) | Err(_) => break,
            Ok(read) => head.extend_from_slice(&buffer[..read]),
        }
    }
    String::from_utf8_lossy(&head)
        .split("\r\n")
        .skip(1)
        .take_while(|line| !line.is_empty())
        .filter_map(|line| line.split_once(':'))
        .any(|(name, value)| name.eq_ignore_ascii_case("server") && value.trim() == "EC2ws")
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, net::TcpListener, thread};

    use super::*;

    /// A metadata stub that answers each connection with `response`, or
    /// holds it unanswered when `response` is `None`.
    fn stub(response: Option<&'static str>) -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        thread::spawn(move || {
            for stream in listener.incoming() {
                let mut stream = stream.unwrap();
                let mut request = [0; 1024];
                let _ = stream.read(&mut request);
                match response {
                    Some(response) => {
                        let _ = stream.write_all(response.as_bytes());
                    }
                    None => thread::sleep(Duration::from_secs(5)),
                }
            }
        });
        address
    }

    fn vars(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map = pairs
            .iter()
            .map(|(name, value)| (name.to_string(), value.to_string()))
            .collect::<BTreeMap<_, _>>();
        move |name| map.get(name).cloned()
    }

    #[test]
    fn the_setting_overrides_detection() {
        let unasked = || -> Result<bool> { panic!("detection ran") };
        assert!(choose(Some("on"), unasked).unwrap());
        assert!(!choose(Some("off"), unasked).unwrap());
        assert!(choose(None, || Ok(false)).unwrap());
        assert!(!choose(Some("auto"), || Ok(true)).unwrap());
        assert!(choose(Some("yes"), unasked).is_err());
    }

    #[test]
    fn aws_metadata_answers_with_any_status() {
        for status in ["200 OK", "401 Unauthorized"] {
            let response = format!(
                "HTTP/1.1 {status}\r\nContent-Length: 0\r\nServer: EC2ws\r\nConnection: close\r\n\r\n"
            )
            .leak();
            assert!(imds(stub(Some(response)), PROBE), "{status}");
        }
    }

    #[test]
    fn other_metadata_services_are_not_aws() {
        let gce = "HTTP/1.1 404 Not Found\r\nServer: Metadata Server for VM\r\n\r\n";
        assert!(!imds(stub(Some(gce)), PROBE));
        // EC2ws only in the body is not the header.
        let body = "HTTP/1.1 200 OK\r\nServer: nginx\r\n\r\nServer: EC2ws\r\n";
        assert!(!imds(stub(Some(body)), PROBE));
    }

    #[test]
    fn an_unanswered_probe_stops_within_its_budget() {
        let refused = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap();
        for address in [stub(None), refused] {
            let started = Instant::now();
            assert!(!imds(address, PROBE));
            assert!(started.elapsed() < PROBE + Duration::from_millis(20));
        }
    }

    #[test]
    fn aws_runtimes_are_recognized_by_their_variables() {
        for pair in [
            ("AWS_EXECUTION_ENV", "AWS_ECS_FARGATE"),
            ("ECS_CONTAINER_METADATA_URI_V4", "http://169.254.170.2/v4/x"),
            (
                "AWS_CONTAINER_CREDENTIALS_RELATIVE_URI",
                "/v2/credentials/x",
            ),
            (
                "AWS_CONTAINER_CREDENTIALS_FULL_URI",
                "http://169.254.170.23/v1/credentials",
            ),
        ] {
            assert!(aws_runtime(vars(&[pair])), "{pair:?}");
        }
        for pair in [
            (
                "AWS_CONTAINER_CREDENTIALS_FULL_URI",
                "http://127.0.0.1:9911/",
            ),
            ("AWS_WEB_IDENTITY_TOKEN_FILE", "/var/run/token"),
            ("AWS_EXECUTION_ENV", ""),
        ] {
            assert!(!aws_runtime(vars(&[pair])), "{pair:?}");
        }
    }

    #[test]
    fn metadata_address_follows_the_sdk_variables() {
        assert_eq!(
            metadata(vars(&[])).unwrap(),
            Some(SocketAddr::from(([169, 254, 169, 254], 80)))
        );
        assert_eq!(
            metadata(vars(&[("AWS_EC2_METADATA_DISABLED", "TRUE")])).unwrap(),
            None
        );
        let endpoint = |value| metadata(vars(&[("AWS_EC2_METADATA_SERVICE_ENDPOINT", value)]));
        assert_eq!(
            endpoint("http://127.0.0.1:8111/").unwrap(),
            Some(SocketAddr::from(([127, 0, 0, 1], 8111)))
        );
        assert_eq!(
            endpoint("http://[fd00:ec2::254]").unwrap(),
            Some("[fd00:ec2::254]:80".parse().unwrap())
        );
        assert!(endpoint("http://metadata.internal").is_err());
    }
}

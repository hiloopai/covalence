//! [`S3Kv::from_aws`] with no AWS configuration. The SDK reads the process
//! environment and the home directory, so each test runs again in a child
//! process with neither.

use std::{env, process::Command};

use covalence_kv_s3::{AwsOptions, Error, S3Kv};

/// Set in the child process.
const CHILD: &str = "COVALENCE_KV_S3_TEST_CHILD";

/// Run the test `name` in a child process with an empty home directory, no
/// instance metadata, and require it to pass.
fn in_child(name: &str) {
    let home = tempfile::tempdir().unwrap();
    let output = Command::new(env::current_exe().unwrap())
        .args([name, "--exact", "--test-threads=1"])
        .env_clear()
        .env(CHILD, "1")
        .env("HOME", home.path())
        .env("AWS_EC2_METADATA_DISABLED", "true")
        .output()
        .unwrap();
    let report = String::from_utf8_lossy(&output.stdout) + String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{report}");
    assert!(report.contains("1 passed"), "{report}");
}

#[test]
fn missing_region_fails_inside_an_async_task() {
    if env::var_os(CHILD).is_none() {
        return in_child("missing_region_fails_inside_an_async_task");
    }
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    let result =
        runtime.block_on(async { S3Kv::from_aws("s3://bucket/".parse()?, &AwsOptions::default()) });
    assert!(matches!(result, Err(Error::Region)), "{result:?}");
}

#[test]
fn default_endpoints_follow_the_partition() {
    if env::var_os(CHILD).is_none() {
        return in_child("default_endpoints_follow_the_partition");
    }
    for (region, host) in [
        ("us-east-1", "s3.us-east-1.amazonaws.com"),
        ("cn-north-1", "s3.cn-north-1.amazonaws.com.cn"),
        ("cn-northwest-1", "s3.cn-northwest-1.amazonaws.com.cn"),
    ] {
        let options = AwsOptions {
            region: Some(region.into()),
            ..AwsOptions::default()
        };
        let kv = S3Kv::from_aws("s3://bucket/".parse().unwrap(), &options).unwrap();
        let debug = format!("{kv:?}");
        assert!(debug.contains(&format!("Domain({host:?})")), "{debug}");
    }
}

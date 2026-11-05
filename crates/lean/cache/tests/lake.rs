mod support;

use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
    sync::atomic::Ordering,
};

use serde_json::Value;
use support::Server;

const TOOLCHAIN: &str = "leanprover/lean4:nightly-2026-10-30";
// s3s-fs base64-encodes the entire key in one metadata filename (NAME_MAX=255).
const REPOSITORY: &str = "t/p";

fn checked(command: &mut Command) -> Output {
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "command failed\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

struct Fixture {
    temporary: tempfile::TempDir,
    root: PathBuf,
    server: Server,
}

impl Fixture {
    fn new() -> Self {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("project");
        fs::create_dir_all(root.join("Demo")).unwrap();
        fs::write(root.join("lean-toolchain"), format!("{TOOLCHAIN}\n")).unwrap();
        fs::write(
            root.join("lakefile.toml"),
            "name = \"demo\"\ndefaultTargets = [\"Demo\"]\n[[lean_lib]]\nname = \"Demo\"\n",
        )
        .unwrap();
        fs::write(root.join(".gitignore"), ".lake/\nbuild/\n").unwrap();
        fs::write(
            root.join("Demo/A.lean"),
            "module\npublic theorem a (n : Nat) : n + 0 = n := by simp\n",
        )
        .unwrap();
        fs::write(
            root.join("Demo/B.lean"),
            "module\npublic theorem b (n : Nat) : 0 + n = n := by simp\n",
        )
        .unwrap();
        fs::write(
            root.join("Demo.lean"),
            "module\npublic import Demo.A\npublic import Demo.B\n",
        )
        .unwrap();
        checked(Command::new("git").args(["init", "-q"]).arg(&root));
        let server = Server::new(&temporary.path().join("s3"));
        let fixture = Self {
            temporary,
            root,
            server,
        };
        checked(fixture.lake().args(["env", "true"]));
        fixture.commit();
        fixture
    }

    fn env(&self, command: &mut Command) {
        command
            .current_dir(&self.root)
            .env("ELAN_TOOLCHAIN", TOOLCHAIN)
            .env("LAKE_CACHE_DIR", self.temporary.path().join("cache"))
            .env("LAKE_ARTIFACT_CACHE", "true")
            .env("AWS_REGION", "us-east-1")
            .env("AWS_ENDPOINT_URL", &self.server.endpoint)
            .env("AWS_ACCESS_KEY_ID", "test-key")
            .env("AWS_SECRET_ACCESS_KEY", "test-secret")
            .env_remove("AWS_SESSION_TOKEN")
            .env_remove("AWS_PROFILE")
            .env_remove("COVALENCE_LAKE_SCOPE")
            .env_remove("CURL")
            .env("LAKE_CI_BUCKET", "bucket")
            .env("LAKE_CI_PREFIX", "proofs")
            .env("GITHUB_ACTIONS", "true")
            .env("GITHUB_REPOSITORY", REPOSITORY)
            .env("GITHUB_EVENT_NAME", "push")
            .env("GITHUB_REF", "refs/heads/main")
            .env("GITHUB_RUN_ID", "1")
            .env("GITHUB_RUN_ATTEMPT", "1");
    }

    fn lake(&self) -> Command {
        let mut cmd = Command::new("lake");
        self.env(&mut cmd);
        cmd
    }

    fn cli(&self, action: &str) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_cov-lake-cache"));
        self.env(&mut cmd);
        cmd.args([action, "--repository", REPOSITORY]);
        cmd
    }

    fn commit(&self) {
        checked(
            Command::new("git")
                .current_dir(&self.root)
                .args(["add", "."]),
        );
        checked(Command::new("git").current_dir(&self.root).args([
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.invalid",
            "commit",
            "-qm",
            "test fixture",
        ]));
    }

    fn build(&self) {
        checked(self.cli("build").args(["--", "Demo"]));
    }
    fn record(&self, name: &str) -> PathBuf {
        self.root.join("build/lake-cache/root").join(name)
    }
    fn publish(&self) -> Value {
        checked(&mut self.cli("put"));
        serde_json::from_slice(&fs::read(self.record("publication.json")).unwrap()).unwrap()
    }
    fn scope(&self) -> String {
        let receipt: Value =
            serde_json::from_slice(&fs::read(self.record("success.json")).unwrap()).unwrap();
        receipt["identity"]["scope"].as_str().unwrap().to_owned()
    }
    fn bucket(&self) -> PathBuf {
        self.temporary.path().join("s3/bucket/proofs")
    }
    fn reset_local(&self) {
        fs::remove_dir_all(self.root.join(".lake/build")).unwrap();
        fs::remove_dir_all(self.temporary.path().join("cache")).unwrap();
    }
}

fn oleans(root: &Path) -> Vec<Vec<u8>> {
    ["A", "B"]
        .into_iter()
        .map(|name| fs::read(root.join(format!(".lake/build/lib/lean/Demo/{name}.olean"))).unwrap())
        .collect()
}

#[test]
fn incremental_publish_repairs_missing_objects_and_native_restore_reuses_proofs() {
    let fixture = Fixture::new();
    fixture.build();
    let first = fixture.publish();
    assert!(first["uploaded"].as_u64().unwrap() > 2);
    let before = fixture.server.writes.lock().unwrap().len();
    let warm = fixture.publish();
    assert_eq!(warm["uploaded"], 0);
    assert_eq!(warm["artifact_bytes"], 0);
    let writes = fixture.server.writes.lock().unwrap();
    assert_eq!(writes.len() - before, 3);
    assert!(writes[before..].iter().all(|s| s.contains("/r/")));
    drop(writes);
    let path = fixture.root.join("Demo/A.lean");
    fs::write(
        &path,
        format!(
            "{}public theorem added (n : Nat) : n = n := rfl\n",
            fs::read_to_string(&path).unwrap()
        ),
    )
    .unwrap();
    fixture.commit();
    fixture.build();
    let changed = fixture.publish();
    assert!(changed["uploaded"].as_u64().unwrap() > 0);
    assert!(changed["reused"].as_u64().unwrap() > 0);
    assert!(changed["uploaded"].as_u64().unwrap() < first["uploaded"].as_u64().unwrap());
    let object = fs::read_dir(fixture.bucket().join("a").join(fixture.scope()))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    fs::remove_file(&object).unwrap();
    assert_eq!(fixture.publish()["uploaded"], 1);
    let expected = oleans(&fixture.root);
    fixture.reset_local();
    checked(&mut fixture.cli("get"));
    let result = checked(fixture.lake().args(["build", "--wfail", "Demo"]));
    let log = format!(
        "{}{}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(
        !log.contains("Built Demo"),
        "recompiled restored proofs: {log}"
    );
    assert_eq!(oleans(&fixture.root), expected);
    // The same native path must reject corrupted bytes, including same-size data.
    let bytes = fs::read(&object).unwrap();
    let mut decoded = zstd::stream::decode_all(bytes.as_slice()).unwrap();
    decoded[0] ^= 1;
    let corrupt = zstd::stream::encode_all(decoded.as_slice(), 1).unwrap();
    fs::write(&object, &corrupt).unwrap();
    let revision = checked(
        Command::new("git")
            .current_dir(&fixture.root)
            .args(["rev-parse", "HEAD"]),
    );
    let revision = String::from_utf8(revision.stdout).unwrap();
    let manifest_path = fixture
        .bucket()
        .join("r")
        .join(fixture.scope())
        .join(format!("{}.artifacts.json", revision.trim()));
    let mut manifest: Value = serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
    let name = object
        .file_name()
        .unwrap()
        .to_str()
        .unwrap()
        .strip_suffix(".zst")
        .unwrap();
    manifest["artifacts"][name]["encoded_bytes"] = corrupt.len().into();
    fs::write(manifest_path, serde_json::to_vec(&manifest).unwrap()).unwrap();
    fixture.reset_local();
    let corrupt = fixture.cli("get").output().unwrap();
    assert!(!corrupt.status.success());
    assert!(String::from_utf8_lossy(&corrupt.stderr).contains("hash mismatch"));
}

#[test]
fn unsuccessful_or_stale_receipts_cannot_publish() {
    let fixture = Fixture::new();
    fixture.build();
    assert!(
        !fixture
            .cli("put")
            .env("GITHUB_RUN_ID", "other")
            .status()
            .unwrap()
            .success()
    );
    let mapping = fixture.record("outputs.jsonl");
    let original = fs::read(&mapping).unwrap();
    fs::write(
        &mapping,
        [original.as_slice(), b"[\"0000000000000000\",null]\n"].concat(),
    )
    .unwrap();
    assert!(!fixture.cli("put").status().unwrap().success());
    fs::write(&mapping, original).unwrap();
    let manifest = fixture.record("artifacts.json");
    let original = fs::read(&manifest).unwrap();
    fs::write(&manifest, [original.as_slice(), b"\n"].concat()).unwrap();
    assert!(!fixture.cli("put").status().unwrap().success());
    fs::write(&manifest, original).unwrap();
    assert!(fixture.server.writes.lock().unwrap().is_empty());
    fs::write(
        fixture.root.join("Demo/A.lean"),
        "module\npublic theorem impossible : False := by trivial\n",
    )
    .unwrap();
    fixture.commit();
    assert!(!fixture.cli("build").status().unwrap().success());
    assert!(!fixture.record("success.json").exists());
    assert!(!fixture.cli("put").status().unwrap().success());
    assert!(fixture.server.writes.lock().unwrap().is_empty());
}

#[test]
fn transfer_or_inventory_failure_never_publishes_a_pointer() {
    let fixture = Fixture::new();
    fixture.build();
    fixture.server.deny.store(true, Ordering::SeqCst);
    assert!(!fixture.cli("put").status().unwrap().success());
    assert!(
        fixture
            .server
            .writes
            .lock()
            .unwrap()
            .iter()
            .all(|s| s.contains("/a/"))
    );
    fixture.server.deny.store(false, Ordering::SeqCst);
    fixture.publish();
    let before = fixture.server.writes.lock().unwrap().len();
    fixture.server.incomplete.store(true, Ordering::SeqCst);
    assert!(!fixture.cli("put").status().unwrap().success());
    assert_eq!(fixture.server.writes.lock().unwrap().len(), before);
}

#[test]
fn untrusted_writers_and_download_urls_are_rejected_before_network_io() {
    let fixture = Fixture::new();
    for (event, reference, repository) in [
        ("pull_request", "refs/pull/1/merge", REPOSITORY),
        ("push", "refs/heads/topic", REPOSITORY),
        ("push", "refs/heads/main", "fork/proofs"),
    ] {
        let result = fixture
            .cli("put")
            .env("GITHUB_EVENT_NAME", event)
            .env("GITHUB_REF", reference)
            .env("GITHUB_REPOSITORY", repository)
            .output()
            .unwrap();
        assert!(!result.status.success());
        assert!(String::from_utf8_lossy(&result.stderr).contains("trusted main"));
    }
    for url in [
        "https://evil.invalid/r/test/0000000000000000000000000000000000000000.jsonl",
        "https://covalence-lake.invalid/r/test/../other",
        "https://covalence-lake.invalid/r/test/%2e%2e/escape",
    ] {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_cov-lake-cache"));
        fixture.env(&mut cmd);
        let output = cmd
            .env("COVALENCE_LAKE_SCOPE", "test")
            .args(["-s", "-L", "-w", "%{stderr}%{json}\n", "--retry", "3", url])
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(!String::from_utf8_lossy(&output.stderr).contains("test-secret"));
    }
    assert!(fixture.server.writes.lock().unwrap().is_empty());
}

#[test]
fn package_and_platform_identity_are_checked_and_missing_pointers_are_misses() {
    let fixture = Fixture::new();
    checked(&mut fixture.cli("get"));
    let vendor = fixture.root.join("vendor");
    fs::create_dir(&vendor).unwrap();
    fs::write(
        vendor.join("lakefile.toml"),
        "name = \"vendor\"\ndefaultTargets = [\"Vendor\"]\n[[lean_lib]]\nname = \"Vendor\"\n",
    )
    .unwrap();
    fs::write(
        vendor.join("Vendor.lean"),
        "module\npublic theorem fact : 1 = 1 := rfl\n",
    )
    .unwrap();
    let config = fixture.root.join("lakefile.toml");
    fs::write(
        &config,
        format!(
            "{}\n[[require]]\nname = \"vendor\"\npath = \"vendor\"\n",
            fs::read_to_string(&config).unwrap()
        ),
    )
    .unwrap();
    checked(fixture.lake().arg("update"));
    fixture.commit();
    checked(fixture.cli("build").args(["--package", "vendor"]));
    checked(fixture.cli("put").args(["--package", "vendor"]));
    let record = fixture.root.join("build/lake-cache/vendor");
    assert!(
        !fixture
            .cli("put")
            .arg("--record")
            .arg(&record)
            .status()
            .unwrap()
            .success()
    );
    let path = record.join("success.json");
    let mut receipt: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    receipt["identity"]["scope"] =
        Value::String("example/proofs/other-platform/other-pins/vendor".into());
    fs::write(path, serde_json::to_vec(&receipt).unwrap()).unwrap();
    let before = fixture.server.writes.lock().unwrap().len();
    assert!(
        !fixture
            .cli("put")
            .args(["--package", "vendor"])
            .status()
            .unwrap()
            .success()
    );
    assert_eq!(fixture.server.writes.lock().unwrap().len(), before);
}

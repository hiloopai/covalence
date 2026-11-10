mod support;

use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
    sync::atomic::Ordering,
};

use serde_json::Value;
use support::Server;

/// The toolchain on `PATH`, which CI sets per tested release.
fn toolchain() -> String {
    std::env::var("LAKE_TEST_TOOLCHAIN")
        .unwrap_or_else(|_| "leanprover/lean4:nightly-2026-10-30".into())
}
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
        fs::write(root.join("lean-toolchain"), format!("{}\n", toolchain())).unwrap();
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
            .env("ELAN_TOOLCHAIN", toolchain())
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
        self.root.join("build/lake-cache").join(name)
    }
    /// The publication report of the root package.
    fn publish(&self) -> Value {
        checked(&mut self.cli("put"));
        let report: Value =
            serde_json::from_slice(&fs::read(self.record("publication.json")).unwrap()).unwrap();
        report["packages"][0].clone()
    }
    fn scope(&self) -> String {
        let receipt: Value =
            serde_json::from_slice(&fs::read(self.record("success.json")).unwrap()).unwrap();
        receipt["identity"]["scope"].as_str().unwrap().to_owned()
    }
    /// The pool object holding the bytes of a local file.
    fn pool_object(&self, path: &Path) -> PathBuf {
        use sha2::Digest;
        let digest = sha2::Sha256::digest(fs::read(path).unwrap());
        let digest = data_encoding::BASE32_NOPAD
            .encode(&digest)
            .to_ascii_lowercase();
        self.temporary
            .path()
            .join("s3/bucket/lake")
            .join(self.scope())
            .join("cas/sha2-256")
            .join(digest)
            .join("bytes.zst")
    }
    fn revision(&self) -> String {
        let output = checked(
            Command::new("git")
                .current_dir(&self.root)
                .args(["rev-parse", "HEAD"]),
        );
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }
    fn reset_local(&self) {
        for directory in [
            self.root.join(".lake/build"),
            self.temporary.path().join("cache"),
        ] {
            if directory.exists() {
                fs::remove_dir_all(directory).unwrap();
            }
        }
    }
}

/// Asserts that Lake's artifact cache holds each of `expected`. A restore
/// leaves outputs there; with the cache writable and `restoreAllArtifacts`
/// unset, Lake reads them from the cache and copies only `.ilean` files into
/// `.lake/build`.
fn assert_restored(fixture: &Fixture, expected: &[Vec<u8>]) {
    let cached: Vec<Vec<u8>> = fs::read_dir(fixture.temporary.path().join("cache/artifacts"))
        .unwrap()
        .map(|entry| fs::read(entry.unwrap().path()).unwrap())
        .collect();
    for (index, bytes) in expected.iter().enumerate() {
        assert!(
            cached.contains(bytes),
            "olean {index} is not in the artifact cache"
        );
    }
}

fn oleans(root: &Path) -> Vec<Vec<u8>> {
    ["A", "B"]
        .into_iter()
        .flat_map(|name| {
            ["olean", "olean.server", "olean.private"].map(|extension| {
                fs::read(root.join(format!(".lake/build/lib/lean/Demo/{name}.{extension}")))
                    .unwrap()
            })
        })
        .collect()
}

#[test]
fn embedded_nul_strings_survive_publication_and_native_restore() {
    let fixture = Fixture::new();
    fs::write(
        fixture.root.join("Demo/A.lean"),
        "module\npublic def withNul : String := \"a\\x00b\"\n",
    )
    .unwrap();
    fixture.commit();
    fixture.build();
    let expected = oleans(&fixture.root);
    fixture.publish();
    fixture.reset_local();
    let restored = log(&checked(&mut fixture.cli("get")));
    assert!(restored.contains("placed 3 archives"), "{restored}");
    // The restore decoded every archive, so Lake runs no archiver.
    // `lake env lean` below reads `.lake/build`, which a cache hit fills only
    // when all artifacts are restored.
    let built = log(&checked(
        fixture
            .lake()
            .env("LAKE_RESTORE_ARTIFACTS", "true")
            .args(["build", "--wfail", "Demo"]),
    ));
    assert!(!built.contains("Unpacked"), "{built}");
    assert_restored(&fixture, &expected);
    fs::write(
        fixture.root.join("Check.lean"),
        "import Demo.A\n#eval withNul.toList.map Char.toNat\n",
    )
    .unwrap();
    let result = checked(fixture.lake().args(["env", "lean", "Check.lean"]));
    assert_eq!(
        String::from_utf8(result.stdout).unwrap().trim(),
        "[97, 0, 98]"
    );
}

#[cfg(unix)]
#[test]
fn changing_the_archive_tool_invalidates_receipts_and_remote_scope() {
    use std::os::unix::fs::symlink;

    let fixture = Fixture::new();
    fixture.build();
    fixture.publish();
    let prefix = checked(fixture.lake().args(["env", "lean", "--print-prefix"]));
    let prefix = PathBuf::from(String::from_utf8(prefix.stdout).unwrap().trim());
    let overlay = fixture.temporary.path().join("toolchain");
    fs::create_dir_all(overlay.join("bin")).unwrap();
    for entry in fs::read_dir(&prefix).unwrap() {
        let entry = entry.unwrap();
        if entry.file_name() != "bin" {
            symlink(entry.path(), overlay.join(entry.file_name())).unwrap();
        }
    }
    for entry in fs::read_dir(prefix.join("bin")).unwrap() {
        let entry = entry.unwrap();
        let destination = overlay.join("bin").join(entry.file_name());
        if entry.file_name() == "leantar" {
            fs::copy(entry.path(), &destination).unwrap();
            // ELF permits trailing data: change identity without changing behavior.
            let mut bytes = fs::read(&destination).unwrap();
            bytes.extend_from_slice(b"archive-tool-identity-test");
            fs::write(destination, bytes).unwrap();
        } else if matches!(entry.file_name().to_str(), Some("lean" | "lake")) {
            // Lake prefers the toolchain beside its resolved executable.
            fs::copy(entry.path(), destination).unwrap();
        } else {
            symlink(entry.path(), destination).unwrap();
        }
    }
    let writes = fixture.server.writes.lock().unwrap().len();
    let result = fixture
        .cli("put")
        .env("LAKE", overlay.join("bin/lake"))
        .output()
        .unwrap();
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("receipt does not match"));
    assert_eq!(fixture.server.writes.lock().unwrap().len(), writes);
    fixture.reset_local();
    let result = checked(fixture.cli("get").env("LAKE", overlay.join("bin/lake")));
    assert!(String::from_utf8_lossy(&result.stdout).contains("no published build"));
    assert!(
        !fixture
            .root
            .join(".lake/build/lib/lean/Demo/A.olean")
            .exists()
    );
}

#[test]
fn deep_extension_data_survives_publication_and_native_restore() {
    let fixture = Fixture::new();
    fs::write(
        fixture.root.join("Demo/A.lean"),
        r#"import Lean
open Lean
initialize listExt : SimplePersistentEnvExtension (List Nat) (Array (List Nat)) ←
  registerSimplePersistentEnvExtension {
    addEntryFn := Array.push
    addImportedFn := Array.flatten
  }
"#,
    )
    .unwrap();
    fs::write(
        fixture.root.join("Demo/B.lean"),
        "import Demo.A\nopen Lean\nrun_cmd modifyEnv (listExt.addEntry · (List.range 50000))\n",
    )
    .unwrap();
    fs::write(fixture.root.join("Demo.lean"), "import Demo.B\n").unwrap();
    fixture.commit();
    fixture.build();
    let path = fixture.root.join(".lake/build/lib/lean/Demo/B.olean");
    let expected = fs::read(&path).unwrap();
    fixture.publish();
    fixture.reset_local();
    checked(&mut fixture.cli("get"));
    let result = checked(
        fixture
            .lake()
            .env("LAKE_RESTORE_ARTIFACTS", "true")
            .args(["build", "--wfail", "Demo"]),
    );
    for log in [&result.stdout, &result.stderr] {
        let log = String::from_utf8_lossy(log);
        assert!(
            !log.contains("Built Demo"),
            "recompiled restored data: {log}"
        );
    }
    assert_restored(&fixture, std::slice::from_ref(&expected));
    fs::write(
        fixture.root.join("Check.lean"),
        r#"import Demo.B
open Lean
run_cmd unless listExt.getState (← getEnv) == #[List.range 50000] do
  throwError "restored extension data differs"
"#,
    )
    .unwrap();
    checked(fixture.lake().args(["env", "lean", "Check.lean"]));
}

fn log(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

#[test]
fn incremental_publish_sends_only_the_difference_and_restore_reuses_proofs() {
    let fixture = Fixture::new();
    fixture.build();
    let first = fixture.publish();
    assert!(first["uploaded"].as_u64().unwrap() > 2);
    let before = fixture.server.writes.lock().unwrap().len();
    let warm = fixture.publish();
    assert_eq!(warm["changed"], 0);
    assert_eq!(warm["uploaded"], 0);
    assert_eq!(warm["nodes"], 0);
    // Builds: only the revision pointer. Sources: the tree, record and pointer.
    let writes = fixture.server.writes.lock().unwrap();
    let pool = writes[before..]
        .iter()
        .filter(|key| !key.contains("/lake/src/"))
        .collect::<Vec<_>>();
    assert_eq!(pool.len(), 1, "{pool:?}");
    assert!(pool[0].contains("/rev/"));
    let sources = writes.len() - before - pool.len();
    assert_eq!(sources, 3, "{:?}", &writes[before..]);
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
    assert!(changed["parent"].is_string());
    assert!(changed["uploaded"].as_u64().unwrap() > 0);
    assert!(changed["changed"].as_u64().unwrap() < changed["entries"].as_u64().unwrap());
    assert!(changed["uploaded"].as_u64().unwrap() < first["uploaded"].as_u64().unwrap());
    let expected = oleans(&fixture.root);
    let mapping = fs::read_to_string(fixture.record("packages/demo.jsonl")).unwrap();
    let row: Value = serde_json::from_str(mapping.lines().nth(1).unwrap()).unwrap();
    let artifact = fixture
        .temporary
        .path()
        .join("cache/artifacts")
        .join(row[1].as_str().unwrap());
    let object = fixture.pool_object(&artifact);
    assert!(object.is_file());
    fixture.reset_local();
    checked(&mut fixture.cli("get"));
    let result = log(&checked(fixture.lake().args(["build", "--wfail", "Demo"])));
    assert!(
        !result.contains("Built Demo"),
        "recompiled restored proofs: {result}"
    );
    assert_restored(&fixture, &expected);
    // Same-size corruption of a stored artifact fails its CID check.
    let mut decoded = zstd::stream::decode_all(fs::read(&object).unwrap().as_slice()).unwrap();
    decoded[0] ^= 1;
    fs::write(
        &object,
        zstd::stream::encode_all(decoded.as_slice(), 1).unwrap(),
    )
    .unwrap();
    fixture.reset_local();
    assert!(!fixture.cli("get").output().unwrap().status.success());
}

/// Each file of Lake's artifact cache with its modification time.
fn artifact_listing(fixture: &Fixture) -> Vec<(PathBuf, std::time::SystemTime)> {
    let mut files: Vec<_> = fs::read_dir(fixture.temporary.path().join("cache/artifacts"))
        .unwrap()
        .map(|entry| {
            let entry = entry.unwrap();
            (entry.path(), entry.metadata().unwrap().modified().unwrap())
        })
        .collect();
    files.sort();
    files
}

#[test]
fn a_second_restore_into_a_warm_cache_succeeds_and_writes_nothing() {
    let fixture = Fixture::new();
    fixture.build();
    fixture.publish();
    fixture.reset_local();
    checked(&mut fixture.cli("dependencies"));
    checked(&mut fixture.cli("get"));
    let warm = artifact_listing(&fixture);
    assert!(!warm.is_empty());
    // Forget which roots this host installed, so the restore transfers every
    // entry again into a cache that already holds its read-only artifacts.
    fs::remove_dir_all(fixture.temporary.path().join("cache/covalence/installed")).unwrap();
    fs::remove_dir_all(fixture.temporary.path().join("cache/covalence/mapped")).unwrap();
    checked(&mut fixture.cli("dependencies"));
    let again = log(&checked(&mut fixture.cli("get")));
    assert!(!again.contains(" 0 changed entries"), "{again}");
    assert_eq!(artifact_listing(&fixture), warm);
    checked(fixture.lake().args(["build", "--wfail", "Demo"]));
}

#[test]
fn restore_uses_the_nearest_published_ancestor_and_worktrees_share_one_cache() {
    let fixture = Fixture::new();
    fixture.build();
    fixture.publish();
    let published = fixture.revision();
    fs::write(fixture.root.join("NOTES"), "unpublished\n").unwrap();
    fixture.commit();
    fixture.reset_local();
    let restored = log(&checked(&mut fixture.cli("get")));
    assert!(
        restored.contains(&format!("restoring {published}, 1 commits behind HEAD")),
        "{restored}"
    );
    let worktree = fixture.temporary.path().join("worktree");
    checked(
        Command::new("git")
            .current_dir(&fixture.root)
            .args(["worktree", "add", "-q", "--detach"])
            .arg(&worktree)
            .arg("HEAD"),
    );
    // The second checkout installs nothing: the host already holds this root.
    let reads = fixture.server.reads.lock().unwrap().len();
    let second = log(&checked(
        fixture
            .cli("get")
            .current_dir(&worktree)
            .arg("--root")
            .arg(&worktree),
    ));
    assert!(second.contains("0 changed entries"), "{second}");
    let built = log(&checked(
        fixture
            .lake()
            .current_dir(&worktree)
            .args(["build", "--wfail", "Demo"]),
    ));
    assert!(
        !built.contains("Built Demo"),
        "rebuilt in a worktree: {built}"
    );
    assert!(
        fixture.server.reads.lock().unwrap()[reads..]
            .iter()
            .all(|path| path.contains("/rev/"))
    );
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
    let mapping = fixture.record("packages/demo.jsonl");
    let original = fs::read(&mapping).unwrap();
    fs::write(
        &mapping,
        [original.as_slice(), b"[\"0000000000000000\",null]\n"].concat(),
    )
    .unwrap();
    assert!(!fixture.cli("put").status().unwrap().success());
    fs::write(&mapping, original).unwrap();
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
fn lean_reads_restored_dependency_oleans_from_their_build_folders() {
    let fixture = Fixture::new();
    let dependency = fixture.root.join("Dep");
    fs::create_dir(&dependency).unwrap();
    fs::write(
        dependency.join("lakefile.toml"),
        "name = \"dep\"\n[[lean_lib]]\nname = \"Good\"\n",
    )
    .unwrap();
    fs::write(
        dependency.join("Good.lean"),
        "module\npublic theorem good : True := trivial\n",
    )
    .unwrap();
    let config = fixture.root.join("lakefile.toml");
    fs::write(
        &config,
        format!(
            "{}\n[[require]]\nname = \"dep\"\npath = \"Dep\"\n",
            fs::read_to_string(&config).unwrap()
        ),
    )
    .unwrap();
    fs::write(
        fixture.root.join("Demo/A.lean"),
        "module\npublic import Good\npublic theorem fromDependency : True := good\n",
    )
    .unwrap();
    checked(fixture.lake().arg("update"));
    fixture.commit();
    fixture.build();
    fixture.publish();
    fixture.reset_local();
    for directory in [
        dependency.join(".lake/build"),
        fixture.root.join(".lake/build"),
    ] {
        if directory.exists() {
            fs::remove_dir_all(directory).unwrap();
        }
    }
    let check = "import Good\n#check good\n";
    // `lake env lean` runs `lean` on the packages' `.lake/build` folders, as
    // evidence scripts and checkers do, without Lake resolving the cache.
    let lean_reads = |root: &Path| {
        fs::write(root.join("Check.lean"), check).unwrap();
        checked(
            fixture
                .lake()
                .current_dir(root)
                .env_remove("LAKE_RESTORE_ARTIFACTS")
                .args(["env", "lean", "Check.lean"]),
        );
    };
    // Fresh: the restore's own build links every dependency output.
    checked(fixture.cli("get").args(["--", "Demo"]));
    assert!(dependency.join(".lake/build/lib/lean/Good.olean").is_file());
    lean_reads(&fixture.root);
    // Warm: a second worktree on the same host.
    let worktree = fixture.temporary.path().join("worktree");
    checked(
        Command::new("git")
            .current_dir(&fixture.root)
            .args(["worktree", "add", "-q", "--detach"])
            .arg(&worktree)
            .arg("HEAD"),
    );
    checked(
        fixture
            .cli("get")
            .current_dir(&worktree)
            .arg("--root")
            .arg(&worktree)
            .args(["--", "Demo"]),
    );
    assert!(
        worktree
            .join("Dep/.lake/build/lib/lean/Good.olean")
            .is_file()
    );
    lean_reads(&worktree);
}

#[test]
fn dependencies_are_recorded_for_the_build_targets_closure_only() {
    // A hyphenated name, which Lake quotes as `«path-dep»` in its manifest.
    let fixture = Fixture::new();
    let dependency = fixture.root.join("Dep");
    fs::create_dir(&dependency).unwrap();
    fs::write(
        dependency.join("lakefile.toml"),
        "name = \"path-dep\"\ndefaultTargets = [\"Unused\"]\n[[lean_lib]]\nname = \"Good\"\n[[lean_lib]]\nname = \"Unused\"\n",
    )
    .unwrap();
    fs::write(
        dependency.join("Good.lean"),
        "module\npublic theorem good : True := trivial\n",
    )
    .unwrap();
    fs::write(
        dependency.join("Unused.lean"),
        "module\npublic theorem impossible : False := by trivial\n",
    )
    .unwrap();
    let config = fixture.root.join("lakefile.toml");
    fs::write(
        &config,
        format!(
            "{}\n[[require]]\nname = \"path-dep\"\npath = \"Dep\"\n",
            fs::read_to_string(&config).unwrap()
        ),
    )
    .unwrap();
    checked(fixture.lake().arg("update"));
    fixture.commit();

    // No import needs this package yet; an empty snapshot is not published.
    fixture.build();
    checked(&mut fixture.cli("put"));
    let pointers = || {
        fixture
            .server
            .writes
            .lock()
            .unwrap()
            .iter()
            .filter(|key| key.contains("/rev/"))
            .cloned()
            .collect::<Vec<_>>()
    };
    assert!(pointers().iter().all(|key| !key.ends_with("/path-dep")));

    fs::write(
        fixture.root.join("Demo/A.lean"),
        "module\npublic import Good\npublic theorem fromDependency : True := good\n",
    )
    .unwrap();
    fixture.commit();
    fixture.build();
    checked(&mut fixture.cli("put"));
    assert!(pointers().iter().any(|key| key.ends_with("/path-dep")));
    assert!(
        !dependency
            .join(".lake/build/lib/lean/Unused.olean")
            .exists()
    );

    // Importing the invalid proof must fail even after a successful snapshot.
    let writes = fixture.server.writes.lock().unwrap().len();
    fs::write(
        fixture.root.join("Demo/A.lean"),
        "module\npublic import Unused\n",
    )
    .unwrap();
    fixture.commit();
    assert!(!fixture.cli("build").output().unwrap().status.success());
    assert!(!fixture.cli("put").output().unwrap().status.success());
    assert_eq!(fixture.server.writes.lock().unwrap().len(), writes);
}

#[test]
fn transfer_failure_never_publishes_a_pointer() {
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
            .all(|s| !s.contains("/rev/"))
    );
    fixture.server.deny.store(false, Ordering::SeqCst);
    fixture.publish();
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
        "https://covalence-lake.invalid/r/test/../other",
        "https://covalence-lake.invalid/r/test/%2e%2e/escape",
        "https://covalence-lake.invalid/a/test/0000000000000000.art.tmp",
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
fn other_services_reach_curl_without_cache_credentials() {
    let fixture = Fixture::new();
    let bin = fixture.temporary.path().join("bin");
    fs::create_dir(&bin).unwrap();
    let seen = fixture.temporary.path().join("curl-call");
    let curl = bin.join("curl");
    fs::write(
        &curl,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\nenv >> '{}'\nexit 7\n",
            seen.display(),
            seen.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&curl, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let url = "https://dependency.example/a0/scope/0000000000000001.art";
    let args = ["-s", "-S", "-f", "-o", "/tmp/unused.olean", "-L", url];
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_cov-lake-cache"));
    fixture.env(&mut cmd);
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap());
    let output = cmd
        .env("PATH", path)
        .env("COVALENCE_LAKE_SCOPE", "test")
        .args(args)
        .output()
        .unwrap();
    // curl's own status reaches Lake, which then falls back to building.
    assert_eq!(output.status.code(), Some(7));
    let call = fs::read_to_string(&seen).unwrap();
    assert!(call.starts_with(&format!("{}\n", args.join("\n"))));
    assert!(!call.contains("test-secret"));
    assert!(fixture.server.reads.lock().unwrap().is_empty());
}

#[test]
fn target_restore_fetches_only_the_target_closure_and_then_works_offline() {
    let fixture = Fixture::new();
    fixture.build();
    fixture.publish();
    let artifacts = |from: usize| {
        fixture.server.reads.lock().unwrap()[from..]
            .iter()
            .filter(|path| path.contains("/cas/"))
            .count()
    };
    let log = |output: Output| {
        format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    };
    fixture.reset_local();
    let start = fixture.server.reads.lock().unwrap().len();
    checked(&mut fixture.cli("get"));
    let everything = artifacts(start);
    fixture.reset_local();
    let start = fixture.server.reads.lock().unwrap().len();
    let restored = log(checked(fixture.cli("get").args(["--", "Demo.A"])));
    assert!(!restored.contains("Built Demo.A"), "rebuilt: {restored}");
    let fetched = artifacts(start);
    assert!(
        0 < fetched && fetched < everything,
        "{fetched} of {everything}"
    );
    let built = fixture.root.join(".lake/build/lib/lean/Demo");
    assert!(built.join("A.olean").exists());
    assert!(!built.join("B.olean").exists());
    // A second targeted restore installs no mappings again.
    let again = log(checked(fixture.cli("get").args(["--", "Demo.A"])));
    assert!(again.contains("demo: 0 changed entries"), "{again}");
    // Once local, a build needs no remote requests.
    let start = fixture.server.reads.lock().unwrap().len();
    let warm = log(checked(fixture.lake().args(["build", "--wfail", "Demo.A"])));
    assert!(!warm.contains("Built Demo.A"), "rebuilt: {warm}");
    assert_eq!(fixture.server.reads.lock().unwrap().len(), start);
    let path = fixture.root.join("Demo/A.lean");
    fs::write(
        &path,
        "module\npublic theorem a (n : Nat) : n + 0 = n := Nat.add_zero n\n",
    )
    .unwrap();
    let edited = log(checked(fixture.lake().args(["build", "--wfail", "Demo.A"])));
    assert!(edited.contains("Built Demo.A"), "{edited}");
    assert_eq!(fixture.server.reads.lock().unwrap().len(), start);
}

#[test]
fn a_seeded_fresh_host_fetches_no_artifacts_and_unpacks_nothing() {
    let fixture = Fixture::new();
    fixture.build();
    checked(&mut fixture.cli("put"));
    let report: Value =
        serde_json::from_slice(&fs::read(fixture.record("publication.json")).unwrap()).unwrap();
    assert!(report["seed"]["files"].as_u64().unwrap() > 0, "{report}");
    fixture.reset_local();
    let start = fixture.server.reads.lock().unwrap().len();
    let restored = log(&checked(fixture.cli("get").args(["--seed", "--", "Demo"])));
    assert!(restored.contains("Lake cache: seeded"), "{restored}");
    assert!(!restored.contains("Built Demo"), "rebuilt: {restored}");
    let reads = fixture.server.reads.lock().unwrap()[start..].to_vec();
    assert!(
        reads.iter().all(|path| !path.contains("/cas/sha2-256/")),
        "{reads:?}"
    );
    // The seed's outputs name unpacked files, so Lake needs no archives.
    let archives = fs::read_dir(fixture.temporary.path().join("cache/artifacts"))
        .unwrap()
        .filter(|entry| {
            entry
                .as_ref()
                .unwrap()
                .file_name()
                .to_string_lossy()
                .ends_with(".ltar")
        })
        .count();
    assert_eq!(archives, 0);
    // A warm host already holds the mappings and takes no seed.
    let again = log(&checked(fixture.cli("get").args(["--seed", "--", "Demo"])));
    assert!(!again.contains("Lake cache: seeded"), "{again}");
}

#[test]
fn toolchain_identity_is_checked_and_missing_pointers_are_misses() {
    let fixture = Fixture::new();
    let miss = log(&checked(&mut fixture.cli("get")));
    assert!(miss.contains("no published build"), "{miss}");
    fixture.build();
    let path = fixture.record("success.json");
    let mut receipt: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    receipt["identity"]["scope"] = Value::String("other-platform/other-toolchain".into());
    fs::write(path, serde_json::to_vec(&receipt).unwrap()).unwrap();
    assert!(!fixture.cli("put").status().unwrap().success());
    assert!(fixture.server.writes.lock().unwrap().is_empty());
}

#[test]
fn one_targeted_get_serves_every_package_of_the_workspace() {
    // A hyphenated name, which Lake quotes as `«git-dep»` in its manifest.
    let fixture = Fixture::new();
    let dependency = fixture.temporary.path().join("dep");
    fs::create_dir(&dependency).unwrap();
    fs::write(
        dependency.join("lakefile.toml"),
        "name = \"git-dep\"\ndefaultTargets = [\"Good\"]\n[[lean_lib]]\nname = \"Good\"\n[[lean_lib]]\nname = \"Better\"\n",
    )
    .unwrap();
    fs::write(
        dependency.join("Good.lean"),
        "module\npublic theorem good : True := trivial\n",
    )
    .unwrap();
    fs::write(
        dependency.join("Better.lean"),
        "module\npublic theorem better : True := trivial\n",
    )
    .unwrap();
    fs::write(dependency.join(".gitignore"), ".lake/\n").unwrap();
    let git = |directory: &Path, args: &[&str]| {
        checked(
            Command::new("git")
                .current_dir(directory)
                .args([
                    "-c",
                    "user.name=Test",
                    "-c",
                    "user.email=test@example.invalid",
                ])
                .args(args),
        )
    };
    git(&dependency, &["init", "-q"]);
    git(&dependency, &["add", "."]);
    git(
        &dependency,
        &["-c", "commit.gpgsign=false", "commit", "-qm", "dep"],
    );
    let config = fixture.root.join("lakefile.toml");
    fs::write(
        &config,
        format!(
            "{}\n[[require]]\nname = \"git-dep\"\ngit = \"{}\"\nrev = \"HEAD\"\n",
            fs::read_to_string(&config).unwrap(),
            dependency.display()
        ),
    )
    .unwrap();
    checked(fixture.lake().arg("update"));
    fs::write(
        fixture.root.join("Demo/A.lean"),
        "module\npublic import Good\npublic theorem a : True := good\n",
    )
    .unwrap();
    fixture.commit();
    fixture.build();
    checked(&mut fixture.cli("put"));
    let dep_entries = || {
        let report: Value =
            serde_json::from_slice(&fs::read(fixture.record("publication.json")).unwrap()).unwrap();
        report["packages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|package| package["package"] == "git-dep")
            .map(|package| package["entries"].as_u64().unwrap())
    };
    let first = dep_entries().expect("first publication records the dependency");
    // A second closure of the same pin adds to the pool's map.
    fs::write(
        fixture.root.join("Demo/A.lean"),
        "module\npublic import Good\npublic import Better\npublic theorem a : True := good\n",
    )
    .unwrap();
    fixture.commit();
    fixture.build();
    checked(&mut fixture.cli("put"));
    assert!(
        fixture
            .server
            .writes
            .lock()
            .unwrap()
            .iter()
            .any(|key| key.contains("/rev/") && key.ends_with("/git-dep"))
    );
    assert!(
        dep_entries().unwrap() > first,
        "the union lost the first closure"
    );
    fixture.reset_local();
    let packaged = fixture.root.join(".lake/packages/git-dep/.lake/build");
    if packaged.exists() {
        fs::remove_dir_all(&packaged).unwrap();
    }
    let restored = log(&checked(fixture.cli("get").args(["--", "Demo"])));
    assert!(restored.contains("Lake cache: git-dep: "), "{restored}");
    assert!(
        !restored.contains("Built Good"),
        "rebuilt the dependency: {restored}"
    );
    assert!(
        !restored.contains("Built Demo"),
        "rebuilt the root: {restored}"
    );
}

#[test]
fn native_objects_of_platform_independent_dependencies_are_published() {
    // Lake's `-o` drops platform-dependent outputs of such packages, as it
    // does for Mathlib and Aesop.
    let fixture = Fixture::new();
    let dependency = fixture.root.join("Dep");
    fs::create_dir(&dependency).unwrap();
    fs::write(
        dependency.join("lakefile.toml"),
        "name = \"dep\"\nplatformIndependent = true\n[[lean_lib]]\nname = \"Good\"\n",
    )
    .unwrap();
    fs::write(dependency.join("Good.lean"), "def good : Nat := 42\n").unwrap();
    fs::write(
        fixture.root.join("Main.lean"),
        "import Good\ndef main : IO Unit := IO.println good\n",
    )
    .unwrap();
    let config = fixture.root.join("lakefile.toml");
    fs::write(
        &config,
        format!(
            "{}\n[[lean_exe]]\nname = \"demo-exe\"\nroot = \"Main\"\n[[require]]\nname = \"dep\"\npath = \"Dep\"\n",
            fs::read_to_string(&config).unwrap()
        ),
    )
    .unwrap();
    checked(fixture.lake().arg("update"));
    fixture.commit();
    checked(fixture.cli("build").args(["--", "demo-exe"]));
    let recorded = fs::read_to_string(fixture.record("packages/dep.jsonl")).unwrap();
    assert!(recorded.contains(".o\"]"), "{recorded}");
    checked(&mut fixture.cli("put"));
    fixture.reset_local();
    let packaged = dependency.join(".lake/build");
    if packaged.exists() {
        fs::remove_dir_all(&packaged).unwrap();
    }
    let restored = log(&checked(fixture.cli("get").args(["--", "demo-exe"])));
    assert!(
        !restored.contains("Built Good:c.o"),
        "compiled the dependency's object: {restored}"
    );
}

#[test]
fn an_older_checkout_seeds_from_a_later_commit_and_takes_the_rest_from_the_pool() {
    let fixture = Fixture::new();
    let old = fixture.revision();
    fixture.build();
    checked(&mut fixture.cli("put"));
    // Two later publishes take both seed slots.
    for proof in ["Nat.zero_add n", "by omega"] {
        fs::write(
            fixture.root.join("Demo/B.lean"),
            format!("module\npublic theorem b (n : Nat) : 0 + n = n := {proof}\n"),
        )
        .unwrap();
        fixture.commit();
        fixture.build();
        checked(&mut fixture.cli("put"));
    }
    let newest = fixture.revision();
    checked(
        Command::new("git")
            .current_dir(&fixture.root)
            .args(["checkout", "-q", "--detach", &old]),
    );
    fixture.reset_local();
    let restored = log(&checked(fixture.cli("get").args(["--seed", "--", "Demo"])));
    assert!(
        restored.contains(&format!(
            "no seed for {old}; seeding from {newest} (not an ancestor)"
        )),
        "{restored}"
    );
    assert!(restored.contains("Lake cache: seeded"), "{restored}");
    assert!(!restored.contains("Built Demo"), "rebuilt: {restored}");
}

#[test]
fn module_traces_name_the_recorded_inputs_and_targeted_restores_prefetch_them() {
    let fixture = Fixture::new();
    fixture.build();
    // The input hash in each module's trace is the one Lake's `-o` records,
    // which the module index relies on.
    let recorded = fs::read_to_string(fixture.record("packages/demo.jsonl")).unwrap();
    let trace: Value = serde_json::from_slice(
        &fs::read(fixture.root.join(".lake/build/lib/lean/Demo/A.trace")).unwrap(),
    )
    .unwrap();
    let input = trace["depHash"].as_str().unwrap();
    assert!(
        recorded.contains(&format!("[\"{input}\",")),
        "{input} not in {recorded}"
    );
    let index: Value =
        serde_json::from_slice(&fs::read(fixture.record("modules/demo.json")).unwrap()).unwrap();
    assert_eq!(index["modules"]["Demo.A"], input);
    checked(&mut fixture.cli("put"));
    assert!(
        fixture
            .server
            .writes
            .lock()
            .unwrap()
            .iter()
            .any(|key| key.contains("/modules/") && key.ends_with("/demo"))
    );
    fixture.reset_local();
    let restored = log(&checked(fixture.cli("get").args(["--", "Demo.A"])));
    assert!(
        restored.contains("prefetching 1 of the 1 workspace modules"),
        "{restored}"
    );
    assert!(!restored.contains("Built Demo.A"), "rebuilt: {restored}");
    assert!(
        !restored.contains("Unpacked Demo.A"),
        "unpacked: {restored}"
    );
}

#[test]
fn recording_after_a_seeded_restore_reuses_the_pools_archives() {
    let fixture = Fixture::new();
    fixture.build();
    checked(&mut fixture.cli("put"));
    let published = fs::read_to_string(fixture.record("packages/demo.jsonl")).unwrap();
    fixture.reset_local();
    checked(fixture.cli("get").args(["--seed", "--", "Demo"]));
    // The seed holds unpacked files only; Lake would pack every archive.
    let recorded = log(&checked(fixture.cli("build").args(["--", "Demo"])));
    assert!(
        recorded.contains("placed the pool's archives of 3 unchanged modules"),
        "{recorded}"
    );
    assert_eq!(
        fs::read_to_string(fixture.record("packages/demo.jsonl")).unwrap(),
        published
    );
}

#[test]
fn restores_into_a_workspace_lake_has_not_configured_install_without_lock_failures() {
    // Several packages' mappings install at once; on a fresh checkout each
    // Lake run would otherwise reconfigure the workspace concurrently.
    let fixture = Fixture::new();
    let mut config = fs::read_to_string(fixture.root.join("lakefile.toml")).unwrap();
    for name in ["DepA", "DepB", "DepC"] {
        let dependency = fixture.root.join(name);
        fs::create_dir(&dependency).unwrap();
        fs::write(
            dependency.join("lakefile.toml"),
            format!(
                "name = \"{}\"\n[[lean_lib]]\nname = \"{name}\"\n",
                name.to_lowercase()
            ),
        )
        .unwrap();
        fs::write(
            dependency.join(format!("{name}.lean")),
            format!(
                "module\npublic theorem {} : True := trivial\n",
                name.to_lowercase()
            ),
        )
        .unwrap();
        config.push_str(&format!(
            "\n[[require]]\nname = \"{}\"\npath = \"{name}\"\n",
            name.to_lowercase()
        ));
    }
    fs::write(fixture.root.join("lakefile.toml"), config).unwrap();
    fs::write(
        fixture.root.join("Demo/A.lean"),
        "module\npublic import DepA\npublic import DepB\npublic import DepC\npublic theorem a (n : Nat) : n + 0 = n := by simp\n",
    )
    .unwrap();
    checked(fixture.lake().arg("update"));
    fixture.commit();
    fixture.build();
    checked(&mut fixture.cli("put"));
    for _ in 0..3 {
        fixture.reset_local();
        // Drop every package's configuration, as on a fresh checkout.
        for dir in [".lake", "DepA/.lake", "DepB/.lake", "DepC/.lake"] {
            let path = fixture.root.join(dir);
            if path.exists() {
                fs::remove_dir_all(path).unwrap();
            }
        }
        checked(fixture.cli("get").args(["--", "Demo"]));
    }
}

#[test]
fn a_dependency_the_pool_already_maps_is_recorded_without_a_replay() {
    let fixture = Fixture::new();
    let dependency = fixture.temporary.path().join("dep");
    fs::create_dir(&dependency).unwrap();
    fs::write(
        dependency.join("lakefile.toml"),
        "name = \"git-dep\"\n[[lean_lib]]\nname = \"Good\"\n",
    )
    .unwrap();
    fs::write(
        dependency.join("Good.lean"),
        "module\npublic theorem good : True := trivial\n",
    )
    .unwrap();
    fs::write(dependency.join(".gitignore"), ".lake/\n").unwrap();
    let git = |directory: &Path, args: &[&str]| {
        checked(
            Command::new("git")
                .current_dir(directory)
                .args([
                    "-c",
                    "user.name=Test",
                    "-c",
                    "user.email=test@example.invalid",
                ])
                .args(args),
        )
    };
    git(&dependency, &["init", "-q"]);
    git(&dependency, &["add", "."]);
    git(
        &dependency,
        &["-c", "commit.gpgsign=false", "commit", "-qm", "dep"],
    );
    let config = fixture.root.join("lakefile.toml");
    fs::write(
        &config,
        format!(
            "{}\n[[require]]\nname = \"git-dep\"\ngit = \"{}\"\nrev = \"HEAD\"\n",
            fs::read_to_string(&config).unwrap(),
            dependency.display()
        ),
    )
    .unwrap();
    checked(fixture.lake().arg("update"));
    fs::write(
        fixture.root.join("Demo/A.lean"),
        "module\npublic import Good\npublic theorem a : True := good\n",
    )
    .unwrap();
    fixture.commit();
    let first = log(&checked(fixture.cli("build").args(["--", "Demo"])));
    assert!(!first.contains("without replaying"), "{first}");
    checked(&mut fixture.cli("put"));
    let replayed = fs::read_to_string(fixture.record("packages/git-dep.jsonl")).unwrap();
    // A root-only change: the pin's map in the pool holds everything.
    fs::write(
        fixture.root.join("Demo/B.lean"),
        "module\npublic theorem b (n : Nat) : 0 + n = n := Nat.zero_add n\n",
    )
    .unwrap();
    fixture.commit();
    let second = log(&checked(fixture.cli("build").args(["--", "Demo"])));
    assert!(
        second.contains("recorded 1 dependencies from the pool's maps"),
        "{second}"
    );
    let rows = |text: &str| {
        let mut rows = text.lines().map(str::to_owned).collect::<Vec<_>>();
        rows.sort();
        rows
    };
    assert_eq!(
        rows(&fs::read_to_string(fixture.record("packages/git-dep.jsonl")).unwrap()),
        rows(&replayed)
    );
    checked(&mut fixture.cli("put"));
}

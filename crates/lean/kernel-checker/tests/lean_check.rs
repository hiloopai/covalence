use covalence_cas_sqlite::SqliteCas;
use covalence_cas_trait::{GetExt, Put};
use covalence_lib_cid::Cid;
use serde_json::{Value, json};
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::Path,
    process::{Command, Output},
};

fn executable(path: &Path, source: &str) {
    fs::write(path, source).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

fn fixture() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::create_dir_all(root.join("toolchain/bin")).unwrap();
    fs::create_dir_all(root.join("toolchain/lib/lean")).unwrap();
    fs::write(root.join("toolchain/lib/lean/Init.olean"), "core").unwrap();
    fs::write(root.join("Proof.lean"), "example : True := trivial\n").unwrap();
    fs::write(root.join("Dependency.olean"), "dependency").unwrap();
    fs::write(
        root.join("setup.json"),
        serde_json::to_vec(&json!({
            "name": "Proof", "isModule": false,
            "importArts": {"Dependency": [["Dependency.olean"]]},
            "dynlibs": [], "plugins": [], "options": {}
        }))
        .unwrap(),
    )
    .unwrap();
    fs::write(
        root.join("lean-toolchain"),
        "leanprover/lean4:v4.35.0-rc4\n",
    )
    .unwrap();
    executable(
        &root.join("lake"),
        r#"#!/bin/sh
if [ "$1" = env ]; then
  printf '%s/toolchain\n' "$PWD"
else
  cat setup.json
fi
"#,
    );
    executable(
        &root.join("toolchain/bin/lean"),
        r#"#!/bin/sh
echo check >> '@INVOCATIONS@'
if /usr/bin/grep -q FAIL; then
  echo 'contrived Lean failure' >&2
  exit 1
fi
exit 0
"#
        .replace("@INVOCATIONS@", root.join("invocations").to_str().unwrap())
        .as_str(),
    );
    dir
}

fn prepare(root: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_cov-kernel-checker"))
        .args([
            "lean-check",
            "prepare",
            "Proof.lean",
            "--cache",
            "cache.db",
            "--workspace",
            "artifacts",
        ])
        .env("PATH", format!("{}:/usr/bin:/bin", root.display()))
        .current_dir(root)
        .output()
        .unwrap()
}

fn prepared(root: &Path) -> String {
    let output = prepare(root);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice::<Value>(&output.stdout).unwrap()["environment"]
        .as_str()
        .unwrap()
        .to_owned()
}

fn run(root: &Path, environment: &str) -> Output {
    Command::new(env!("CARGO_BIN_EXE_cov-kernel-checker"))
        .args([
            "lean-check",
            "check",
            "Proof.lean",
            "--environment",
            environment,
            "--cache",
            "cache.db",
            "--workspace",
            "artifacts",
        ])
        .env("PATH", "/nonexistent")
        .current_dir(root)
        .output()
        .unwrap()
}

fn success(output: Output, status: &str) -> String {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["status"], status);
    result["receipt"].as_str().unwrap().to_owned()
}

#[test]
fn successful_check_is_reused_without_running_the_checker() {
    let dir = fixture();
    let root = dir.path();
    let environment = prepared(root);
    let first = success(run(root, &environment), "checked");
    assert_eq!(success(run(root, &environment), "hit"), first);
    assert_eq!(
        fs::read_to_string(root.join("invocations")).unwrap(),
        "check\n"
    );
}

#[test]
fn failed_checks_are_never_cached_and_diagnostics_survive() {
    let dir = fixture();
    let root = dir.path();
    let environment = prepared(root);
    fs::write(root.join("Proof.lean"), "FAIL").unwrap();
    for _ in 0..2 {
        let output = run(root, &environment);
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("contrived Lean failure"));
    }
    assert_eq!(
        fs::read_to_string(root.join("invocations")).unwrap(),
        "check\ncheck\n"
    );
    fs::write(root.join("Proof.lean"), "valid").unwrap();
    success(run(root, &environment), "checked");
    success(run(root, &environment), "hit");
}

#[test]
fn source_dependency_options_and_toolchain_changes_require_new_checks() {
    let dir = fixture();
    let root = dir.path();
    let environment = prepared(root);
    let first = success(run(root, &environment), "checked");
    let original_source = fs::read(root.join("Proof.lean")).unwrap();
    fs::write(root.join("Proof.lean"), "another proof").unwrap();
    assert_ne!(success(run(root, &environment), "checked"), first);
    fs::write(root.join("Proof.lean"), original_source).unwrap();
    assert_eq!(success(run(root, &environment), "hit"), first);

    fs::write(root.join("Dependency.olean"), "changed dependency").unwrap();
    assert_eq!(success(run(root, &environment), "hit"), first);
    let environment = prepared(root);
    let dependency = success(run(root, &environment), "checked");
    assert_ne!(dependency, first);
    let mut setup: Value =
        serde_json::from_slice(&fs::read(root.join("setup.json")).unwrap()).unwrap();
    setup["options"]["maxRecDepth"] = json!(2048);
    fs::write(root.join("setup.json"), serde_json::to_vec(&setup).unwrap()).unwrap();
    let environment = prepared(root);
    let options = success(run(root, &environment), "checked");
    assert_ne!(options, dependency);
    fs::write(root.join("toolchain/lib/lean/Init.olean"), "changed core").unwrap();
    let environment = prepared(root);
    let core = success(run(root, &environment), "checked");
    assert_ne!(core, options);
    let lean = root.join("toolchain/bin/lean");
    executable(
        &lean,
        &(fs::read_to_string(&lean).unwrap() + "\n# new checker build\n"),
    );
    let environment = prepared(root);
    assert_ne!(success(run(root, &environment), "checked"), core);
    assert_eq!(
        fs::read_to_string(root.join("invocations"))
            .unwrap()
            .lines()
            .count(),
        6
    );
}

#[test]
fn incomplete_or_unsupported_inputs_and_broken_storage_fail_closed() {
    let dir = fixture();
    let root = dir.path();
    let environment = prepared(root);
    success(run(root, &environment), "checked");
    let original_setup = fs::read(root.join("setup.json")).unwrap();
    let mut setup: Value = serde_json::from_slice(&original_setup).unwrap();
    setup["plugins"] = json!(["unknown.so"]);
    fs::write(root.join("setup.json"), serde_json::to_vec(&setup).unwrap()).unwrap();
    let output = prepare(root);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("not supported"));
    fs::write(root.join("setup.json"), original_setup).unwrap();
    fs::remove_file(root.join("Dependency.olean")).unwrap();
    assert!(!prepare(root).status.success());
    fs::write(root.join("Dependency.olean"), "dependency").unwrap();
    fs::write(root.join("cache.db"), "broken SQLite file").unwrap();
    assert!(!run(root, &environment).status.success());
    assert_eq!(
        fs::read_to_string(root.join("invocations")).unwrap(),
        "check\n"
    );
}

#[test]
fn checker_uses_the_hashed_toolchain_without_ambient_lean_overrides() {
    let dir = fixture();
    let root = dir.path();
    executable(
        &root.join("toolchain/bin/lean"),
        r#"#!/bin/sh
test -f "$LEAN_SYSROOT/lib/lean/Init.olean" || exit 2
test -z "$LEAN_PATH" || exit 3
test -z "$LD_PRELOAD" || exit 4
/bin/cat >/dev/null
"#,
    );
    let environment = prepared(root);
    let output = Command::new(env!("CARGO_BIN_EXE_cov-kernel-checker"))
        .args([
            "lean-check",
            "check",
            "Proof.lean",
            "--environment",
            &environment,
            "--cache",
            "cache.db",
            "--workspace",
            "artifacts",
        ])
        .env("PATH", format!("{}:/usr/bin:/bin", root.display()))
        .env("LEAN_SYSROOT", "/unrelated-toolchain")
        .env("LEAN_PATH", "/unrelated-imports")
        .current_dir(root)
        .output()
        .unwrap();
    success(output, "checked");
}

#[test]
fn prepared_checks_do_not_need_lake_or_the_original_environment() {
    let dir = fixture();
    let root = dir.path();
    let output = Command::new(env!("CARGO_BIN_EXE_cov-kernel-checker"))
        .args([
            "lean-check",
            "prepare",
            "Proof.lean",
            "--cache",
            "cache.db",
            "--workspace",
            "artifacts",
        ])
        .env("PATH", format!("{}:/usr/bin:/bin", root.display()))
        .current_dir(root)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let prepared: Value = serde_json::from_slice(&output.stdout).unwrap();
    let environment = prepared["environment"].as_str().unwrap();
    fs::remove_file(root.join("lake")).unwrap();
    fs::remove_dir_all(root.join("toolchain")).unwrap();
    fs::remove_file(root.join("Dependency.olean")).unwrap();
    let check = || {
        Command::new(env!("CARGO_BIN_EXE_cov-kernel-checker"))
            .args([
                "lean-check",
                "check",
                "Proof.lean",
                "--environment",
                environment,
                "--cache",
                "cache.db",
                "--workspace",
                "artifacts",
            ])
            .env("PATH", "/nonexistent")
            .current_dir(root)
            .output()
            .unwrap()
    };
    let receipt = success(check(), "checked");
    fs::rename(root.join("artifacts"), root.join("offline")).unwrap();
    assert_eq!(success(check(), "hit"), receipt);
    fs::write(root.join("Proof.lean"), "changed proof").unwrap();
    assert!(
        !check().status.success(),
        "a miss needs an explicitly restored environment"
    );
}

#[test]
fn fresh_worker_restores_exact_inputs_and_receipts_stay_small() {
    let dir = fixture();
    let root = dir.path();
    let environment = prepared(root);
    fs::remove_dir_all(root.join("artifacts")).unwrap();
    fs::remove_dir_all(root.join("toolchain")).unwrap();
    fs::remove_file(root.join("Dependency.olean")).unwrap();
    fs::remove_file(root.join("lake")).unwrap();
    let restore = Command::new(env!("CARGO_BIN_EXE_cov-kernel-checker"))
        .args([
            "lean-check",
            "restore",
            &environment,
            "--cache",
            "cache.db",
            "--workspace",
            "artifacts",
        ])
        .env("PATH", "/nonexistent")
        .current_dir(root)
        .output()
        .unwrap();
    assert!(
        restore.status.success(),
        "{}",
        String::from_utf8_lossy(&restore.stderr)
    );
    let receipt = success(run(root, &environment), "checked");
    let cas = SqliteCas::open(root.join("cache.db")).unwrap();
    let bytes = cas
        .get_fact(&receipt.parse::<Cid>().unwrap())
        .unwrap()
        .unwrap();
    assert!(
        bytes.bytes().len() < 1024,
        "a hit must fetch only a small success record"
    );
    assert_eq!(success(run(root, &environment), "hit"), receipt);
}

#[test]
fn restore_rejects_unsupported_setup_before_publishing_an_environment() {
    let dir = fixture();
    let root = dir.path();
    let environment = prepared(root);
    let cas = SqliteCas::open(root.join("cache.db")).unwrap();
    let original = cas
        .get_fact(&environment.parse::<Cid>().unwrap())
        .unwrap()
        .unwrap();
    let mut manifest: Value = serde_json::from_slice(original.bytes()).unwrap();
    let setup = serde_json::to_vec(&json!({"name":"Proof","isModule":false,
        "plugins":["untracked.so"],"dynlibs":[],"importArts":{},"options":{}}))
    .unwrap();
    let mut archive = tar::Builder::new(flate2::write::GzEncoder::new(
        Vec::new(),
        flate2::Compression::fast(),
    ));
    let mut header = tar::Header::new_gnu();
    header.set_size(setup.len() as u64);
    header.set_mode(0o444);
    header.set_cksum();
    archive
        .append_data(&mut header, "setup.json", setup.as_slice())
        .unwrap();
    let imports: Cid = cas
        .put(archive.into_inner().unwrap().finish().unwrap().into())
        .unwrap();
    manifest["imports"] = json!(imports.to_string());
    let malformed: Cid = cas
        .put(serde_json::to_vec(&manifest).unwrap().into())
        .unwrap();
    let restore = Command::new(env!("CARGO_BIN_EXE_cov-kernel-checker"))
        .args([
            "lean-check",
            "restore",
            &malformed.to_string(),
            "--cache",
            "cache.db",
            "--workspace",
            "artifacts",
        ])
        .current_dir(root)
        .output()
        .unwrap();
    assert!(!restore.status.success());
    assert!(String::from_utf8_lossy(&restore.stderr).contains("not supported"));
    assert!(
        !root
            .join(format!("artifacts/environments/{malformed}.json"))
            .exists()
    );
}

#[test]
fn restore_rejects_archive_links_without_publishing_them() {
    let dir = fixture();
    let root = dir.path();
    let environment = prepared(root);
    let cas = SqliteCas::open(root.join("cache.db")).unwrap();
    let original = cas
        .get_fact(&environment.parse::<Cid>().unwrap())
        .unwrap()
        .unwrap();
    let mut manifest: Value = serde_json::from_slice(original.bytes()).unwrap();
    let mut archive = tar::Builder::new(flate2::write::GzEncoder::new(
        Vec::new(),
        flate2::Compression::fast(),
    ));
    let mut header = tar::Header::new_gnu();
    header.set_entry_type(tar::EntryType::Symlink);
    header.set_size(0);
    header.set_mode(0o777);
    archive
        .append_link(&mut header, "setup.json", root.join("setup.json"))
        .unwrap();
    let imports: Cid = cas
        .put(archive.into_inner().unwrap().finish().unwrap().into())
        .unwrap();
    manifest["imports"] = json!(imports.to_string());
    let malformed: Cid = cas
        .put(serde_json::to_vec(&manifest).unwrap().into())
        .unwrap();
    let restore = Command::new(env!("CARGO_BIN_EXE_cov-kernel-checker"))
        .args([
            "lean-check",
            "restore",
            &malformed.to_string(),
            "--cache",
            "cache.db",
            "--workspace",
            "artifacts",
        ])
        .current_dir(root)
        .output()
        .unwrap();
    assert!(!restore.status.success());
    assert!(String::from_utf8_lossy(&restore.stderr).contains("only regular files"));
    assert!(!root.join(format!("artifacts/objects/{imports}")).exists());
    assert!(
        !root
            .join(format!("artifacts/environments/{malformed}.json"))
            .exists()
    );
}

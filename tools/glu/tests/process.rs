#![cfg(unix)]

use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::PathBuf,
    process::{Command, Output},
};

struct Fixture {
    dir: PathBuf,
}

impl Fixture {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("covalence-glu-{}-{name}", std::process::id()));
        fs::create_dir(&dir).unwrap();
        // Child processes report canonical paths (/tmp is a symlink on macOS).
        let dir = fs::canonicalize(&dir).unwrap();
        let cargo = dir.join("cargo");
        fs::write(
            &cargo,
            r#"#!/bin/sh
printf '%s\0' "$@" >> "$GLU_TEST_LOG"
printf '%s\n' "${0##*/}" >> "$GLU_TEST_CALLS"
pwd > "$GLU_TEST_CWD"
printf '%s\n' "${RUSTDOCFLAGS:-}" >> "$GLU_TEST_RUSTDOCFLAGS"
if [ "${GLU_TEST_FAIL_PROGRAM:-}" = "${0##*/}" ]; then exit 23; fi
if [ "${GLU_TEST_STATUS:-0}" != 0 ]; then exit "$GLU_TEST_STATUS"; fi
if [ "${3:-}" = pip ] && [ "${PIP_CONFIG_FILE:-}" != /dev/null ]; then exit 24; fi
case "${0##*/}" in
  maturin)
    while [ "$1" != --out ]; do shift; done
    mkdir -p "$2"
    touch "$2/covalence-test.whl"
    ;;
  python3)
    if [ "${1:-}" = -I ] && [ "${2:-}" = -m ] && [ "${3:-}" = venv ]; then
      mkdir -p "$4/bin"
      cp "$0" "$4/bin/python"
    fi
    ;;
esac
"#,
        )
        .unwrap();
        fs::set_permissions(&cargo, fs::Permissions::from_mode(0o755)).unwrap();
        fs::copy(&cargo, dir.join("maturin")).unwrap();
        fs::copy(&cargo, dir.join("python3")).unwrap();
        Self { dir }
    }

    fn command(&self, program: impl AsRef<std::ffi::OsStr>) -> Command {
        let mut command = Command::new(program);
        command
            .current_dir(&self.dir)
            .env("CARGO", self.dir.join("cargo"))
            .env("CARGO_TARGET_DIR", self.dir.join("target"))
            .env(
                "PATH",
                std::env::join_paths(
                    std::iter::once(self.dir.clone())
                        .chain(std::env::split_paths(&std::env::var_os("PATH").unwrap())),
                )
                .unwrap(),
            )
            .env("GLU_TEST_LOG", self.dir.join("args"))
            .env("GLU_TEST_CWD", self.dir.join("cwd"))
            .env("GLU_TEST_CALLS", self.dir.join("calls"))
            .env("GLU_TEST_RUSTDOCFLAGS", self.dir.join("rustdocflags"))
            .env_remove("RUSTDOCFLAGS");
        command
    }

    fn run(&self, args: &[&str]) -> Output {
        self.command(env!("CARGO_BIN_EXE_glu"))
            .args(args)
            .output()
            .unwrap()
    }

    fn args(&self) -> Vec<String> {
        fs::read(self.dir.join("args"))
            .unwrap()
            .split(|&byte| byte == 0)
            .filter(|arg| !arg.is_empty())
            .map(|arg| String::from_utf8(arg.to_vec()).unwrap())
            .collect()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap()
}

#[test]
fn forwards_arguments_and_runs_from_checkout() {
    let fixture = Fixture::new("forward");
    assert!(
        fixture
            .run(&["cargo", "test", "--", "a test"])
            .status
            .success()
    );
    assert_eq!(fixture.args(), ["test", "--", "a test"]);
    let cwd = fs::read_to_string(fixture.dir.join("cwd")).unwrap();
    assert_eq!(PathBuf::from(cwd.trim()), root());
}

#[test]
fn ci_stops_and_preserves_child_failure() {
    let fixture = Fixture::new("failure");
    let output = fixture
        .command(env!("CARGO_BIN_EXE_glu"))
        .arg("ci")
        .env("GLU_TEST_STATUS", "17")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(17));
    assert_eq!(fixture.args(), ["fmt", "--all", "--", "--check"]);
    assert!(String::from_utf8_lossy(&output.stderr).contains("Cargo failed"));
}

#[test]
fn build_selects_the_whole_workspace() {
    let fixture = Fixture::new("build");
    assert!(fixture.run(&["build", "--release"]).status.success());
    let args = fixture.args();
    assert_eq!(
        &args[..4],
        ["build", "--workspace", "--locked", "--release"]
    );
    assert_eq!(
        &args[4..11],
        [
            "build",
            "--locked",
            "--manifest-path",
            "crates/python/Cargo.toml",
            "--interpreter",
            "python3",
            "--out"
        ]
    );
    assert_eq!(args.last().unwrap(), "--release");
    assert!(args[11].starts_with(fixture.dir.to_str().unwrap()));
}

#[test]
fn launcher_compiles_only_glu() {
    let fixture = Fixture::new("launcher");
    let mut paths = vec![fixture.dir.clone()];
    paths.extend(std::env::split_paths(&std::env::var_os("PATH").unwrap()));
    let output = fixture
        .command(root().join("bin/glu"))
        .env("PATH", std::env::join_paths(paths).unwrap())
        .args(["cargo", "test", "--", "a test"])
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let cwd = fs::read_to_string(fixture.dir.join("cwd")).unwrap();
    assert_eq!(PathBuf::from(cwd.trim()), root());
    let args = fixture.args();
    assert!(
        args.windows(2)
            .any(|pair| pair == ["--package", "covalence-tools-glu"])
    );
    assert!(!args.iter().any(|arg| arg == "--workspace"));
    assert_eq!(&args[args.len() - 4..], ["cargo", "test", "--", "a test"]);
}

#[test]
fn ci_builds_one_wheel_and_tests_an_isolated_install() {
    let fixture = Fixture::new("pipeline");
    let old = fixture.dir.join("precious");
    fs::create_dir(&old).unwrap();
    fs::write(old.join("sentinel"), "preserve").unwrap();
    fs::create_dir_all(fixture.dir.join("target/glu")).unwrap();
    std::os::unix::fs::symlink(&old, fixture.dir.join("target/glu/python-test")).unwrap();
    let output = fixture.run(&["ci"]);
    assert!(output.status.success(), "{output:?}");
    assert!(old.join("sentinel").exists());
    let calls = fs::read_to_string(fixture.dir.join("calls")).unwrap();
    assert_eq!(
        calls.lines().collect::<Vec<_>>(),
        [
            "cargo", "cargo", "python3", "cargo", "cargo", "cargo", "maturin", "cargo", "python3",
            "python", "python"
        ]
    );
    let args = fixture.args();
    assert_eq!(args.iter().filter(|arg| *arg == "--isolated").count(), 1);
    assert_eq!(args.iter().filter(|arg| *arg == "-I").count(), 4);
    assert!(!args.iter().any(|arg| arg == "--clear"));
    let cwd = fs::read_to_string(fixture.dir.join("cwd")).unwrap();
    assert_eq!(
        PathBuf::from(cwd.trim()),
        fixture.dir.join("target/glu/python-test")
    );
}

#[test]
fn doc_denies_rustdoc_warnings_for_the_workspace() {
    let fixture = Fixture::new("doc");
    let output = fixture.run(&["doc"]);
    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        fixture.args(),
        ["doc", "--no-deps", "--workspace", "--locked"]
    );
    let flags = fs::read_to_string(fixture.dir.join("rustdocflags")).unwrap();
    assert_eq!(
        flags.trim(),
        "-D warnings -D rustdoc::broken_intra_doc_links -D rustdoc::private_intra_doc_links"
    );
}

#[test]
fn package_dry_runs_publishing_the_workspace() {
    let fixture = Fixture::new("package");
    let output = fixture.run(&["package"]);
    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        fixture.args(),
        [
            "publish",
            "--workspace",
            "--dry-run",
            "--locked",
            "--allow-dirty"
        ]
    );
}

#[test]
fn build_propagates_python_packaging_failure() {
    let fixture = Fixture::new("package_failure");
    let output = fixture
        .command(env!("CARGO_BIN_EXE_glu"))
        .arg("build")
        .env("GLU_TEST_FAIL_PROGRAM", "maturin")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(23));
}

#[test]
fn check_propagates_changelog_failure() {
    let fixture = Fixture::new("changelog_failure");
    let output = fixture
        .command(env!("CARGO_BIN_EXE_glu"))
        .arg("check")
        .env("GLU_TEST_FAIL_PROGRAM", "python3")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(23));
    assert!(String::from_utf8_lossy(&output.stderr).contains("Changelog check failed"));
    assert!(
        fixture
            .args()
            .iter()
            .any(|arg| arg.ends_with("tools/glu/check_changelog.py"))
    );
}

#[test]
fn changelog_checker_regression_cases() {
    let output = Command::new("python3")
        .args([
            "-I",
            "-B",
            "-m",
            "unittest",
            "discover",
            "-s",
            "tools/glu",
            "-p",
            "test_changelog.py",
        ])
        .current_dir(root())
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
}

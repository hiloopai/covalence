use std::{
    ffi::{OsStr, OsString},
    fs, io,
    path::{Path, PathBuf},
    process::{Command, ExitCode, ExitStatus},
};

use covalence_lib_cli::clap::{self, Parser, Subcommand};
use covalence_lib_error::{miette, snafu};
use snafu::ResultExt;

#[derive(Parser)]
#[command(version, about = "Covalence development commands")]
struct Cli {
    #[command(subcommand)]
    task: Task,
}

#[derive(Subcommand)]
enum Task {
    /// Build the Rust workspace, Python wheel and JavaScript package.
    Build {
        #[arg(long)]
        release: bool,
    },
    /// Run all project tests.
    Test,
    /// Check the changelog, formatting and Rust workspace lints.
    Check,
    /// Format Rust sources.
    Fmt,
    /// Build the Rust workspace's documentation, denying rustdoc warnings.
    Doc,
    /// Package and verify the crates published to crates.io, without uploading.
    Package,
    /// Run the same checks, build, and tests as CI.
    Ci,
    /// Forward arguments directly to Cargo.
    #[command(trailing_var_arg = true)]
    Cargo {
        #[arg(required = true, allow_hyphen_values = true)]
        args: Vec<OsString>,
    },
}

#[derive(Debug, snafu::Snafu, miette::Diagnostic)]
#[snafu(crate_root(covalence_lib_error::snafu))]
enum Error {
    #[snafu(display("could not start command: {source}"))]
    Spawn { source: io::Error },
    #[snafu(display("project artifact operation failed: {source}"))]
    Filesystem { source: io::Error },
    #[snafu(display("expected one built Python wheel, found {count}"))]
    WheelCount { count: usize },
    #[snafu(display("Cargo.lock does not pin one {package} version"))]
    Locked { package: &'static str },
    #[snafu(display("{program} failed with {status}"))]
    Failed {
        program: &'static str,
        status: ExitStatus,
    },
}

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn subprocess(
    program: &'static str,
    executable: impl AsRef<OsStr>,
    args: impl IntoIterator<Item = impl AsRef<OsStr>>,
    cwd: &Path,
) -> Result<(), Error> {
    let mut command = Command::new(executable);
    if program == "pip" {
        command.env("PIP_CONFIG_FILE", "/dev/null");
    }
    command.args(args).current_dir(cwd);
    run_command(program, &mut command)
}

fn run_command(program: &'static str, command: &mut Command) -> Result<(), Error> {
    let status = command.status().context(SpawnSnafu)?;
    snafu::ensure!(status.success(), FailedSnafu { program, status });
    Ok(())
}

fn cargo_command() -> Command {
    let mut command = Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()));
    command.current_dir(root());
    command
}

fn cargo(args: impl IntoIterator<Item = impl AsRef<OsStr>>) -> Result<(), Error> {
    run_command("Cargo", cargo_command().args(args))
}

/// Cargo's target directory: `CARGO_TARGET_DIR`, or `target` in the checkout.
fn target_dir() -> PathBuf {
    root().join(std::env::var_os("CARGO_TARGET_DIR").unwrap_or_else(|| "target".into()))
}

fn artifacts() -> PathBuf {
    target_dir().join("glu")
}

fn artifact_lock() -> Result<fs::File, Error> {
    fs::create_dir_all(artifacts()).context(FilesystemSnafu)?;
    let lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(artifacts().join("task.lock"))
        .context(FilesystemSnafu)?;
    lock.lock().context(FilesystemSnafu)?;
    Ok(lock)
}

fn build_python(release: bool) -> Result<(), Error> {
    let wheels = artifacts().join("wheels");
    if wheels.exists() {
        fs::remove_dir_all(&wheels).context(FilesystemSnafu)?;
    }
    fs::create_dir_all(&wheels).context(FilesystemSnafu)?;
    let mut args: Vec<OsString> = [
        "build",
        "--locked",
        "--manifest-path",
        "crates/python/Cargo.toml",
        "--interpreter",
        "python3",
        "--out",
    ]
    .into_iter()
    .map(Into::into)
    .collect();
    args.push(wheels.into_os_string());
    if release {
        args.push("--release".into());
    }
    subprocess("maturin", "maturin", args, &root())
}

fn test_python() -> Result<(), Error> {
    let wheels = fs::read_dir(artifacts().join("wheels"))
        .context(FilesystemSnafu)?
        .collect::<Result<Vec<_>, _>>()
        .context(FilesystemSnafu)?
        .into_iter()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|s| s == "whl"))
        .collect::<Vec<_>>();
    snafu::ensure!(
        wheels.len() == 1,
        WheelCountSnafu {
            count: wheels.len()
        }
    );
    let venv = artifacts().join("python-test");
    // Rust removes a final symlink itself; Python's --clear can follow it.
    if venv.symlink_metadata().is_ok() {
        fs::remove_dir_all(&venv).context(FilesystemSnafu)?;
    }
    subprocess(
        "Python",
        "python3",
        [
            OsStr::new("-I"),
            OsStr::new("-m"),
            OsStr::new("venv"),
            venv.as_os_str(),
        ],
        &root(),
    )?;
    let python = venv.join("bin/python");
    subprocess(
        "pip",
        &python,
        [
            OsStr::new("-I"),
            OsStr::new("-m"),
            OsStr::new("pip"),
            OsStr::new("--isolated"),
            OsStr::new("--disable-pip-version-check"),
            OsStr::new("install"),
            OsStr::new("--no-index"),
            OsStr::new("--no-deps"),
            wheels[0].as_os_str(),
        ],
        &venv,
    )?;
    subprocess(
        "Python tests",
        &python,
        [
            OsStr::new("-I"),
            OsStr::new("-m"),
            OsStr::new("unittest"),
            OsStr::new("discover"),
            OsStr::new("-v"),
            OsStr::new("-s"),
            root().join("crates/python/tests").as_os_str(),
        ],
        &venv,
    )
}

/// The target the JavaScript package compiles for.
const WASM: &str = "wasm32-unknown-unknown";

/// The one version of `package` in `Cargo.lock`.
fn locked_version(package: &'static str) -> Result<String, Error> {
    let lock = fs::read_to_string(root().join("Cargo.lock")).context(FilesystemSnafu)?;
    version_in(&lock, package)
}

/// The one version of `package` in the text of a `Cargo.lock`, whatever its
/// line endings.
fn version_in(lock: &str, package: &'static str) -> Result<String, Error> {
    let name = format!("name = \"{package}\"");
    let mut versions = lock.split("[[package]]").filter_map(|entry| {
        let mut lines = entry.lines().map(str::trim).filter(|line| !line.is_empty());
        if lines.next()? != name {
            return None;
        }
        let version = lines
            .next()?
            .strip_prefix("version = \"")?
            .strip_suffix('"')?;
        Some(version.to_owned())
    });
    match (versions.next(), versions.next()) {
        (Some(version), None) => Ok(version),
        _ => LockedSnafu { package }.fail(),
    }
}

/// The wasm-bindgen command at the version `Cargo.lock` pins, installed
/// under the artifacts directory on first use.
fn wasm_bindgen() -> Result<PathBuf, Error> {
    let version = locked_version("wasm-bindgen")?;
    let tools = artifacts().join("tools");
    let command = tools
        .join("bin")
        .join(format!("wasm-bindgen{}", std::env::consts::EXE_SUFFIX));
    let installed = Command::new(&command)
        .arg("--version")
        .output()
        .is_ok_and(|output| {
            output.status.success()
                && String::from_utf8_lossy(&output.stdout)
                    .split_whitespace()
                    .nth(1)
                    == Some(version.as_str())
        });
    if !installed {
        cargo([
            OsStr::new("install"),
            OsStr::new("--locked"),
            OsStr::new("wasm-bindgen-cli"),
            OsStr::new("--version"),
            OsStr::new(&format!("={version}")),
            OsStr::new("--root"),
            tools.as_os_str(),
        ])?;
    }
    Ok(command)
}

fn build_js(release: bool) -> Result<(), Error> {
    let mut args = vec!["build", "--locked", "-p", "covalence-js", "--target", WASM];
    if release {
        args.push("--release");
    }
    cargo(args)?;
    let wasm = target_dir()
        .join(WASM)
        .join(if release { "release" } else { "debug" })
        .join("covalence_js.wasm");
    let package = artifacts().join("js");
    if package.exists() {
        fs::remove_dir_all(&package).context(FilesystemSnafu)?;
    }
    subprocess(
        "wasm-bindgen",
        wasm_bindgen()?,
        [
            OsStr::new("--target"),
            OsStr::new("web"),
            OsStr::new("--out-dir"),
            package.as_os_str(),
            wasm.as_os_str(),
        ],
        &root(),
    )?;
    fs::copy(
        root().join("crates/js/package.json"),
        package.join("package.json"),
    )
    .context(FilesystemSnafu)?;
    Ok(())
}

/// Every `*.test.mjs` file in `crates/js/tests`, in name order.
fn js_tests() -> Result<Vec<PathBuf>, Error> {
    let mut tests = fs::read_dir(root().join("crates/js/tests"))
        .context(FilesystemSnafu)?
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<Result<Vec<_>, _>>()
        .context(FilesystemSnafu)?;
    tests.retain(|path| {
        path.file_name()
            .and_then(OsStr::to_str)
            .is_some_and(|name| name.ends_with(".test.mjs"))
    });
    tests.sort();
    Ok(tests)
}

fn test_js() -> Result<(), Error> {
    run_command(
        "JavaScript tests",
        Command::new("node")
            .arg("--test")
            .args(js_tests()?)
            .env("COVALENCE_JS", artifacts().join("js"))
            .current_dir(root()),
    )
}

fn build(release: bool) -> Result<(), Error> {
    let mut args = vec!["build", "--workspace", "--locked"];
    if release {
        args.push("--release");
    }
    cargo(args)?;
    build_python(release)?;
    build_js(release)
}

fn check() -> Result<(), Error> {
    cargo(["fmt", "--all", "--", "--check"])?;
    cargo([
        "clippy",
        "--workspace",
        "--all-targets",
        "--locked",
        "--",
        "-D",
        "warnings",
    ])?;
    cargo([
        "clippy",
        "-p",
        "covalence-js",
        "--target",
        WASM,
        "--locked",
        "--",
        "-D",
        "warnings",
    ])?;
    subprocess(
        "Changelog check",
        "python3",
        [
            OsStr::new("-I"),
            OsStr::new("-B"),
            root().join("tools/glu/check_changelog.py").as_os_str(),
        ],
        &root(),
    )
}

fn doc() -> Result<(), Error> {
    let flags =
        "-D warnings -D rustdoc::broken_intra_doc_links -D rustdoc::private_intra_doc_links";
    run_command(
        "Cargo",
        cargo_command()
            .args(["doc", "--no-deps", "--workspace", "--locked"])
            .env("RUSTDOCFLAGS", flags),
    )?;
    run_command(
        "Cargo",
        cargo_command()
            .args(["doc", "--no-deps", "-p", "covalence-js", "--locked"])
            .args(["--target", WASM])
            .env("RUSTDOCFLAGS", flags),
    )
}

// Members with `publish = false` are skipped. `--allow-dirty` lets this run
// before changes are committed.
fn package() -> Result<(), Error> {
    cargo([
        "publish",
        "--workspace",
        "--dry-run",
        "--locked",
        "--allow-dirty",
    ])
}

fn test_rust() -> Result<(), Error> {
    cargo(["test", "--workspace", "--locked"])?;
    let minimal = [
        "test",
        "-p",
        "covalence-cas-kv",
        "--locked",
        "--no-default-features",
    ];
    eprintln!("Testing KvCas with identity storage only");
    cargo(minimal)?;
    for codec in ["zstd", "deflate", "brotli"] {
        eprintln!("Testing KvCas with only the {codec} codec feature");
        cargo(minimal.into_iter().chain(["--features", codec]))?;
    }
    eprintln!("Testing covalence-blob-verify without Tokio");
    cargo([
        "test",
        "-p",
        "covalence-blob-verify",
        "--locked",
        "--no-default-features",
    ])
}
fn test() -> Result<(), Error> {
    test_rust()?;
    build_python(false)?;
    test_python()?;
    build_js(false)?;
    test_js()
}

fn run(task: Task) -> Result<(), Error> {
    // Coordinate the wheel, the JavaScript package, the installed tools and
    // the test environment across simultaneous glu tasks.
    let _lock = if matches!(&task, Task::Build { .. } | Task::Test | Task::Ci) {
        Some(artifact_lock()?)
    } else {
        None
    };
    match task {
        Task::Build { release } => build(release),
        Task::Test => test(),
        Task::Check => check(),
        Task::Fmt => cargo(["fmt", "--all"]),
        Task::Doc => doc(),
        Task::Package => package(),
        Task::Ci => {
            check()?;
            doc()?;
            package()?;
            build(false)?;
            test_rust()?;
            test_python()?;
            test_js()
        }
        Task::Cargo { args } => cargo(args),
    }
}

fn failure_code(error: &Error) -> u8 {
    if let Error::Failed { status, .. } = error {
        if let Some(code) = status.code() {
            return u8::try_from(code).unwrap_or(1);
        }
        #[cfg(unix)]
        {
            use std::os::unix::process::ExitStatusExt;
            if let Some(signal) = status.signal() {
                return u8::try_from(128 + signal).unwrap_or(1);
            }
        }
    }
    1
}

fn main() -> ExitCode {
    match run(Cli::parse().task) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            let code = failure_code(&error);
            eprintln!("{:?}", miette::Report::new(error));
            ExitCode::from(code)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn command_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn lock_versions_parse_with_either_line_ending() {
        let lock = "[[package]]\nname = \"a\"\nversion = \"1.2.3\"\n\n[[package]]\nname = \"ab\"\nversion = \"9.9.9\"\n";
        assert_eq!(version_in(lock, "a").unwrap(), "1.2.3");
        assert_eq!(
            version_in(&lock.replace('\n', "\r\n"), "a").unwrap(),
            "1.2.3"
        );
        let twice = format!("{lock}[[package]]\nname = \"a\"\nversion = \"2.0.0\"\n");
        assert!(matches!(version_in(&twice, "a"), Err(Error::Locked { .. })));
    }

    #[test]
    fn locked_versions_come_from_cargo_lock() {
        let version = locked_version("wasm-bindgen").unwrap();
        assert!(version.starts_with("0.2."), "{version}");
        assert!(matches!(
            locked_version("no-such-package"),
            Err(Error::Locked { .. })
        ));
    }

    #[test]
    fn cargo_preserves_flags_spaces_and_separator() {
        let cli = Cli::try_parse_from([
            "glu",
            "cargo",
            "test",
            "--package",
            "two words",
            "--",
            "--exact",
            "a test",
        ])
        .unwrap();
        let Task::Cargo { args } = cli.task else {
            panic!("expected Cargo task");
        };
        assert_eq!(
            args,
            ["test", "--package", "two words", "--", "--exact", "a test"]
        );
    }
}

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
    /// Build the Rust workspace and Python wheel.
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

fn artifacts() -> PathBuf {
    root()
        .join(std::env::var_os("CARGO_TARGET_DIR").unwrap_or_else(|| "target".into()))
        .join("glu")
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

fn build(release: bool) -> Result<(), Error> {
    let mut args = vec!["build", "--workspace", "--locked"];
    if release {
        args.push("--release");
    }
    cargo(args)?;
    build_python(release)
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
    run_command(
        "Cargo",
        cargo_command()
            .args(["doc", "--no-deps", "--workspace", "--locked"])
            .env(
                "RUSTDOCFLAGS",
                "-D warnings -D rustdoc::broken_intra_doc_links -D rustdoc::private_intra_doc_links",
            ),
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
    Ok(())
}
fn test() -> Result<(), Error> {
    test_rust()?;
    build_python(false)?;
    test_python()
}

fn run(task: Task) -> Result<(), Error> {
    // Coordinate the wheel and test environment across simultaneous glu tasks.
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
            test_python()
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

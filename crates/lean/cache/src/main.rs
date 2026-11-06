#![doc = include_str!("../README.md")]
#![deny(missing_docs)]

mod artifact;
mod dependencies;
mod error;
mod mapping;
mod project;
mod restore;
mod storage;

use std::{env, path::PathBuf, process::ExitCode};

use covalence_lib_cli::clap::{self, Parser, ValueEnum};
use covalence_lib_error::miette;

use error::{IoResult, Result, invalid};
use project::Project;

#[derive(Clone, ValueEnum)]
enum Action {
    Dependencies,
    Build,
    Put,
    Get,
}

#[derive(Parser)]
#[command(
    version,
    about = "Record successful Lake builds and share their artifacts over S3"
)]
struct Cli {
    #[arg(value_enum)]
    action: Action,
    #[arg(long, default_value = ".")]
    root: PathBuf,
    #[arg(long)]
    repository: Option<String>,
    /// Record this package's outputs; build targets retain Lake's defaults.
    #[arg(long, default_value = "")]
    package: String,
    #[arg(long)]
    record: Option<PathBuf>,
    #[arg(last = true, allow_hyphen_values = true)]
    targets: Vec<String>,
}

fn execute(cli: Cli) -> Result<()> {
    let root = cli.root.canonicalize().operation("locate project root")?;
    let record = cli.record.unwrap_or_else(|| {
        root.join("build/lake-cache")
            .join(if cli.package.is_empty() {
                "root"
            } else {
                &cli.package
            })
    });
    let project = Project {
        root,
        record: std::path::absolute(record).operation("resolve build record path")?,
        repository: cli
            .repository
            .or_else(|| env::var("GITHUB_REPOSITORY").ok())
            .unwrap_or_default(),
        package: cli.package,
    };
    match cli.action {
        Action::Dependencies => dependencies::fetch(&project, &cli.targets),
        Action::Build => project.build(&cli.targets),
        Action::Put | Action::Get => {
            if matches!(cli.action, Action::Put) && !cli.targets.is_empty() {
                return Err(invalid(
                    "put publishes a recorded build and accepts no targets",
                ));
            }
            if matches!(cli.action, Action::Put)
                && !(env::var("GITHUB_ACTIONS").as_deref() == Ok("true")
                    && env::var("GITHUB_REPOSITORY").as_deref() == Ok(&project.repository)
                    && env::var("GITHUB_REF").as_deref() == Ok("refs/heads/main")
                    && matches!(
                        env::var("GITHUB_EVENT_NAME").as_deref(),
                        Ok("push" | "workflow_dispatch")
                    ))
            {
                return Err(invalid("publication requires a trusted main CI run"));
            }
            let store = storage::store()?;
            match cli.action {
                Action::Put => storage::publish(&project, &store),
                _ => restore::restore(&project, &store, &cli.targets),
            }
        }
    }
}

fn main() -> ExitCode {
    let result = if env::var_os("COVALENCE_LAKE_SCOPE").is_some() {
        restore::download(&env::args().skip(1).collect::<Vec<_>>())
    } else {
        execute(Cli::parse()).map(|()| ExitCode::SUCCESS)
    };
    match result {
        Ok(code) => code,
        Err(error) => {
            eprintln!("{:?}", miette::Report::new(error));
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn command_preserves_lake_target_flags() {
        Cli::command().debug_assert();
        let cli = Cli::try_parse_from([
            "cov-lake-cache",
            "build",
            "--repository",
            "example/proofs",
            "--",
            "--jobs=2",
            "Demo",
        ])
        .unwrap();
        assert_eq!(cli.targets, ["--jobs=2", "Demo"]);
    }
}

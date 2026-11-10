#![doc = include_str!("../README.md")]
#![deny(missing_docs)]

mod accelerate;
mod archives;
mod blobs;
mod dependencies;
pub mod error;
mod gc;
mod local;
mod mapping;
mod modules;
pub mod pool;
mod project;
mod restore;
mod seed;
mod sources;
mod sources_tree;
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
    /// Delete what only scopes unused for `COVALENCE_LAKE_GC_DAYS` hold.
    Gc,
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
    #[arg(long)]
    record: Option<PathBuf>,
    /// With `get`, fill a host that never installed this repository's
    /// mappings from the restored commit's seed first.
    #[arg(long)]
    seed: bool,
    #[arg(last = true, allow_hyphen_values = true)]
    targets: Vec<String>,
}

fn execute(cli: Cli) -> Result<()> {
    let started = std::time::Instant::now();
    let root = cli.root.canonicalize().operation("locate project root")?;
    let record = cli.record.unwrap_or_else(|| root.join("build/lake-cache"));
    let project = Project {
        root,
        record: std::path::absolute(record).operation("resolve build record path")?,
        repository: cli
            .repository
            .or_else(|| env::var("GITHUB_REPOSITORY").ok())
            .unwrap_or_default(),
    };
    if matches!(cli.action, Action::Gc) && !cli.targets.is_empty() {
        return Err(invalid("gc accepts no targets"));
    }
    if cli.seed && !matches!(cli.action, Action::Get) {
        return Err(invalid("--seed applies to get"));
    }
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
    if project.repository.is_empty() && !matches!(cli.action, Action::Dependencies | Action::Gc) {
        return Err(invalid("--repository or GITHUB_REPOSITORY is required"));
    }
    let cache = project.cache_dir()?;
    // gc itself takes the lock exclusively.
    let in_use = (!matches!(cli.action, Action::Gc))
        .then(|| gc::in_use(&cache))
        .transpose()?;
    let store = || {
        let kv = storage::store()?;
        // A restore checks the dependencies out while its seed downloads.
        if !matches!(cli.action, Action::Put | Action::Get) {
            sources::materialize(&project)?;
        }
        Ok::<_, error::Error>(kv)
    };
    match cli.action {
        Action::Dependencies => dependencies::fetch(&project, &store()?, &cli.targets),
        Action::Build => store().and_then(|_| project.build(&cli.targets)),
        Action::Put => storage::publish(&project, &store()?),
        Action::Get => {
            restore::restore(&project, &store()?, &cli.targets, cli.seed, started)?;
            drop(in_use);
            gc::after_restore(&cache)
        }
        Action::Gc => gc::collect(&cache),
    }
}

/// Run the command line, or, under Lake as its `CURL`, a download.
pub fn main() -> ExitCode {
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

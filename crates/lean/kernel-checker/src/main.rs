#![doc = include_str!("../README.md")]
#![deny(missing_docs)]

use covalence_lib_cli::clap::{self, Parser, Subcommand};
use covalence_lib_error::miette::Result;

mod lean_check;
mod receipt_cache;

#[derive(Parser)]
#[command(version)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Transfer successful replay receipts between local SQLite and a trusted S3 prefix
    ReceiptCache(receipt_cache::Args),
    /// Reuse or run a successful Lean source check (trusted projects and cache only)
    #[command(subcommand)]
    LeanCheck(lean_check::Options),
}

fn main() -> Result<()> {
    match Cli::parse().command {
        Command::ReceiptCache(args) => receipt_cache::run(args),
        Command::LeanCheck(options) => lean_check::run(options),
    }
}

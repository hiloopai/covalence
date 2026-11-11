//! Errors from cache operations, with the errors they inherited.

use std::{io, process::ExitStatus};

use covalence_lib_error::{miette, snafu::Snafu};

/// A failed cache operation, with the error it inherited.
#[derive(Debug, Snafu, miette::Diagnostic)]
#[snafu(crate_root(covalence_lib_error::snafu))]
#[allow(missing_docs)]
pub enum Error {
    #[snafu(display("{operation}: {source}"))]
    Io {
        operation: String,
        source: io::Error,
    },
    #[snafu(display("{operation}: {source}"))]
    Json {
        operation: String,
        source: serde_json::Error,
    },
    #[snafu(display("{operation}: {source}"))]
    Store {
        operation: String,
        source: covalence_kv_trait::Error,
    },
    #[snafu(display("{source}"))]
    S3 { source: covalence_kv_s3::Error },
    #[snafu(display("{operation}: {source}"))]
    Cas {
        operation: String,
        source: covalence_cas_trait::Error,
    },
    #[snafu(display("{operation}: {source}"))]
    Map {
        operation: String,
        source: covalence_cas_prolly::Error,
    },
    #[snafu(display("{operation}: {source}"))]
    Dasl {
        operation: String,
        source: covalence_lib_dasl::Error,
    },
    #[snafu(display("{operation}: {source}"))]
    Cid {
        operation: String,
        source: covalence_lib_cid::Error,
    },
    #[snafu(display("unpack archive {archive}: {source}"))]
    Archive {
        archive: String,
        source: covalence_lean_ltar::Error,
    },
    #[snafu(display("{program} failed with {status}"))]
    Process {
        program: &'static str,
        status: ExitStatus,
    },
    /// Every Lake run waits for one install of the toolchain, so each shares
    /// its failure.
    #[snafu(display("install the Lean toolchain: {source}"))]
    Toolchain { source: std::sync::Arc<Error> },
    #[snafu(display("{message}"))]
    Invalid { message: String },
    #[snafu(display("a cache transfer worker panicked"))]
    Worker,
}

/// A result with the cache's [`Error`].
pub type Result<T> = std::result::Result<T, Error>;

pub(crate) fn invalid(message: impl Into<String>) -> Error {
    Error::Invalid {
        message: message.into(),
    }
}

pub(crate) trait IoResult<T> {
    fn operation(self, operation: impl Into<String>) -> Result<T>;
}

impl<T> IoResult<T> for io::Result<T> {
    fn operation(self, operation: impl Into<String>) -> Result<T> {
        self.map_err(|source| Error::Io {
            operation: operation.into(),
            source,
        })
    }
}

pub(crate) fn json<T: covalence_lib_serde::serde::de::DeserializeOwned>(bytes: &[u8]) -> Result<T> {
    serde_json::from_slice(bytes).map_err(|source| Error::Json {
        operation: "read cache metadata".into(),
        source,
    })
}

pub(crate) fn encode(value: &impl covalence_lib_serde::Serialize) -> Result<Vec<u8>> {
    serde_json::to_vec(value).map_err(|source| Error::Json {
        operation: "write cache metadata".into(),
        source,
    })
}

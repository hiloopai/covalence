//! Randomness, through [`rand`].
//!
//! An internal facade (a crate that re-exports other crates) for Covalence's
//! own crates. Its API may change without notice.
//!
//! - The root re-exports all of `rand`: `covalence_lib_rand::rng()` works as
//!   `rand::rng()` does.
//! - On `wasm32-unknown-unknown`, system entropy comes from the JavaScript host.
#![deny(missing_docs)]

pub use rand;
pub use rand::*;

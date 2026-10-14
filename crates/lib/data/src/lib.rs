//! Shared byte buffers and containers.
//!
//! An internal facade (a crate that re-exports other crates) for Covalence's
//! own crates. Its API may change without notice.
//!
//! - Re-exports [`bytes`], [`hashbrown`], [`indexmap`], [`smallvec`],
//!   [`smol_str`] and [`arrayvec`], with their main types at the root.
//! - Adds the [`Symbol`] alias.
//! - `Bytes::copy_from_slice` establishes immutable ownership at an untrusted
//!   boundary. Clones and slices share that owned storage.
//! - An `IndexMap` keeps insertion order. That is not canonical
//!   [CBOR](https://www.rfc-editor.org/rfc/rfc8949) wire ordering.
//! - Serialization lives in `covalence-lib-serde`.
#![deny(missing_docs)]

pub use arrayvec::{ArrayString, ArrayVec};
pub use bytes::{Buf, BufMut, Bytes, BytesMut};
pub use hashbrown::HashTable;
pub use indexmap::{IndexMap, IndexSet};
pub use smallvec::SmallVec;
pub use smol_str::SmolStr;
pub use {arrayvec, bytes, hashbrown, indexmap, smallvec, smol_str};

/// An owned symbol name, stored inline when short and shared when long.
pub type Symbol = SmolStr;

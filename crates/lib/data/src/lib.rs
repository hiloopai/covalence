//! Shared byte buffers and containers.
//!
//! An internal facade (a crate that re-exports other crates) for Covalence's
//! own crates. Its API may change without notice.
//!
//! - Re-exports [`bytes`], [`hashbrown`], [`indexmap`], [`smallvec`],
//!   [`smol_str`], [`arrayvec`] and [`futures_core`], with their main types at
//!   the root.
//! - Adds the [`Symbol`] alias.
//! - Adds [`ByteStream`], [`MaybeSend`] and [`MaybeSync`] for async APIs
//!   that must also build for `wasm32`.
//! - `Bytes::copy_from_slice` establishes immutable ownership at an untrusted
//!   boundary. Clones and slices share that owned storage.
//! - An `IndexMap` keeps insertion order. That is not canonical
//!   [CBOR](https://www.rfc-editor.org/rfc/rfc8949) wire ordering.
//! - Serialization lives in `covalence-lib-serde`.
#![deny(missing_docs)]

use std::{io, pin::Pin};

pub use arrayvec::{ArrayString, ArrayVec};
pub use bytes::{Buf, BufMut, Bytes, BytesMut};
pub use futures_core::Stream;
pub use hashbrown::HashTable;
pub use indexmap::{IndexMap, IndexSet};
pub use smallvec::SmallVec;
pub use smol_str::SmolStr;
pub use {arrayvec, bytes, futures_core, hashbrown, indexmap, smallvec, smol_str};

/// An owned symbol name, stored inline when short and shared when long.
pub type Symbol = SmolStr;

/// A boxed stream whose items are byte chunks or I/O errors.
///
/// `Send` except on `wasm32`, where streams backed by JavaScript are not.
#[cfg(not(target_arch = "wasm32"))]
pub type ByteStream<'a> = Pin<Box<dyn Stream<Item = io::Result<Bytes>> + Send + 'a>>;

/// A boxed stream whose items are byte chunks or I/O errors.
///
/// `Send` except on `wasm32`, where streams backed by JavaScript are not.
#[cfg(target_arch = "wasm32")]
pub type ByteStream<'a> = Pin<Box<dyn Stream<Item = io::Result<Bytes>> + 'a>>;

/// `Send` except on `wasm32`, where every type has it.
///
/// Async traits use it in place of `Send`, so that types holding JavaScript
/// values, which are not `Send`, can implement them on `wasm32`.
#[cfg(not(target_arch = "wasm32"))]
pub trait MaybeSend: Send {}

#[cfg(not(target_arch = "wasm32"))]
impl<T: Send + ?Sized> MaybeSend for T {}

/// `Send` except on `wasm32`, where every type has it.
///
/// Async traits use it in place of `Send`, so that types holding JavaScript
/// values, which are not `Send`, can implement them on `wasm32`.
#[cfg(target_arch = "wasm32")]
pub trait MaybeSend {}

#[cfg(target_arch = "wasm32")]
impl<T: ?Sized> MaybeSend for T {}

/// `Sync` except on `wasm32`, where every type has it.
///
/// The `Sync` counterpart of [`MaybeSend`].
#[cfg(not(target_arch = "wasm32"))]
pub trait MaybeSync: Sync {}

#[cfg(not(target_arch = "wasm32"))]
impl<T: Sync + ?Sized> MaybeSync for T {}

/// `Sync` except on `wasm32`, where every type has it.
///
/// The `Sync` counterpart of [`MaybeSend`].
#[cfg(target_arch = "wasm32")]
pub trait MaybeSync {}

#[cfg(target_arch = "wasm32")]
impl<T: ?Sized> MaybeSync for T {}

// Copied from digama0/leangz `src/lgz.rs` at commit
// 33570f263222fe8aa4e071f4a30b1b69bfc0f4e3, merged with the head of
// https://github.com/digama0/leangz/pull/22 (b15180b547e17bda933b7c7706db1a1ca611fcc8);
// the merged tree is df689da9d52269452b562ef19b25bdcb7de7b47d.
// SPDX-License-Identifier: Apache-2.0
// Copyright: upstream states no copyright line beyond its licence; authors are
// Mario Carneiro and the leangz contributors. Upstream ships no NOTICE file.
// Divergence: tracked. Thomas reads leangz releases and merged PRs on each Lean
// toolchain bump and ports decoder fixes; the pin is the commit above.
// Modified by hiloop: decode path only (`decompress`, `LgzDecompressor` and the
// hashes it recomputes); `LGZ2` streams only; safe Rust without zerocopy;
// malformed input is an error instead of a panic; strings are written straight
// from the input instead of through a scratch buffer.
// License: Apache-2.0, full text in LICENSE-leangz

//! The `lgz` object codec for `.olean` files.
//!
//! An `.olean` is a compacted region of Lean objects. `lgz` writes each object
//! once, in an order its fields can be rebuilt from, and drops the hashes that
//! names, levels and expressions cache, which the decoder recomputes.

mod decode;
mod encode;

pub(crate) use decode::decompress;
pub(crate) use encode::compress;

use crate::{Result, malformed};

const MAGIC: &[u8; 4] = b"LGZ2";
/// Reserve at most this much output up front; the declared size is untrusted.
const RESERVE_LIMIT: u64 = 1 << 28;

// Valid in normal and exprish mode.
const UINT0: u8 = 0xd0;
const UINT0_END: u8 = 0xef;
const BACKREF0: u8 = 0xf0;
const BACKREF0_END: u8 = 0xf7;
const INT1: u8 = 0xf8;
const INT2: u8 = 0xf9;
const INT4: u8 = 0xfa;
const INT8: u8 = 0xfb;
const BACKREF1: u8 = 0xfc;
const BACKREF2: u8 = 0xfd;
const BACKREF4: u8 = 0xfe;
const SAVE: u8 = 0xff;

// Valid in normal mode only.
const ARRAY: u8 = 0x00;
const BIG_CTOR: u8 = 0x10;
const BYTE_ARRAY: u8 = 0x20;
const STRING: u8 = 0x30;
const MPZ: u8 = 0x40;
const THUNK: u8 = 0x50;
const TASK: u8 = 0x60;
const REF: u8 = 0x70;
const EXPRISH: u8 = 0x80;
const PROMISE: u8 = 0x90;
/// Added by leangz#12 and #15; stock leantar 0.1.21 reads it as a constructor.
const SCALAR_ARRAY: u8 = 0xa0;
/// A string with embedded NULs; stock leantar 0.1.21 reads it as a constructor.
const SIZED_STRING: u8 = 0xb0;

/// Lean object tags.
mod tag {
    pub(crate) const PROMISE: u8 = 244;
    pub(crate) const ARRAY: u8 = 246;
    pub(crate) const SCALAR_ARRAY: u8 = 248;
    pub(crate) const STRING: u8 = 249;
    pub(crate) const MPZ: u8 = 250;
    pub(crate) const THUNK: u8 = 251;
    pub(crate) const TASK: u8 = 252;
    pub(crate) const REF: u8 = 253;
}

/// Opcodes and cached data of names, levels and expressions.
mod exprish {
    use super::mix_hash;

    pub(crate) const NORMAL: u8 = 0x20;

    pub(crate) const NAME_STR: u8 = 0x00;
    pub(crate) const NAME_NUM: u8 = 0x01;

    pub(crate) const LEVEL_SUCC: u8 = 0x02;
    pub(crate) const LEVEL_MAX: u8 = 0x03;
    pub(crate) const LEVEL_IMAX: u8 = 0x04;
    pub(crate) const LEVEL_PARAM: u8 = 0x05;
    pub(crate) const LEVEL_MVAR: u8 = 0x06;

    const fn pack_level_data(h: u32, depth: u32, bits: u8) -> u64 {
        (h as u64) | ((bits as u64) << 32) | ((depth as u64) << 40)
    }
    pub(crate) const fn unpack_level_data(data: u64) -> (u32, u32, u8) {
        (data as u32, (data >> 40) as u32, (data >> 32) as u8)
    }

    pub(crate) const LEVEL_ZERO_DATA: u64 = pack_level_data(2221, 0, 0);
    pub(crate) const fn level_succ_data(data: u64) -> u64 {
        let (h, depth, bits) = unpack_level_data(data);
        pack_level_data(mix_hash(2243, h as u64) as u32, depth + 1, bits)
    }
    /// Lean's cached data of a binary level, whose constructor hash is `ctor`.
    fn level_data_binary(ctor: u64, data1: u64, data2: u64) -> u64 {
        let (h1, depth1, bits1) = unpack_level_data(data1);
        let (h2, depth2, bits2) = unpack_level_data(data2);
        let h = mix_hash(ctor, mix_hash(h1 as u64, h2 as u64)) as u32;
        pack_level_data(h, depth1.max(depth2) + 1, bits1 | bits2)
    }
    pub(crate) fn level_max_data(data1: u64, data2: u64) -> u64 {
        level_data_binary(2251, data1, data2)
    }
    pub(crate) fn level_imax_data(data1: u64, data2: u64) -> u64 {
        level_data_binary(2267, data1, data2)
    }
    pub(crate) const fn level_param_data(hash: u64) -> u64 {
        pack_level_data(mix_hash(2239, hash) as u32, 0, 2)
    }
    pub(crate) const fn level_mvar_data(hash: u64) -> u64 {
        pack_level_data(mix_hash(2237, hash) as u32, 0, 1)
    }

    pub(crate) const EXPR_BVAR: u8 = 0x07;
    pub(crate) const EXPR_FVAR: u8 = 0x08;
    pub(crate) const EXPR_MVAR: u8 = 0x09;
    pub(crate) const EXPR_SORT: u8 = 0x0a;
    pub(crate) const EXPR_LIT: u8 = 0x0b;
    pub(crate) const EXPR_MDATA: u8 = 0x0c;
    pub(crate) const EXPR_PROJ: u8 = 0x0d;
    pub(crate) const EXPR_LET: u8 = 0x0e;
    pub(crate) const EXPR_LET_END: u8 = 0x0f;
    pub(crate) const EXPR_LAMBDA: u8 = 0x18;
    pub(crate) const EXPR_FORALL_END: u8 = 0x1f;
    /// `0x20 + n` applies to `n` arguments; `0x20` itself is [`NORMAL`].
    pub(crate) const EXPR_APP: u8 = 0x20;
    pub(crate) const EXPR_APP_END: u8 = EXPR_APP + 0x1f;
    pub(crate) const EXPR_CONST_APP: u8 = 0x40;
    pub(crate) const EXPR_CONST_APP_END: u8 = EXPR_CONST_APP + 0x1f;

    const fn pack_expr_data(h: u64, bvars: u32, depth: u8, bits: u8) -> u64 {
        (h as u32 as u64) | ((depth as u64) << 32) | ((bits as u64) << 40) | ((bvars as u64) << 44)
    }
    const fn unpack_expr_data(data: u64) -> (u64, u32, u8, u8) {
        (
            data as u32 as u64,
            (data >> 44) as u32,
            (data >> 32) as u8,
            ((data >> 40) & 15) as u8,
        )
    }

    pub(crate) const fn expr_const_data(hash1: u64, hash2: u64, bits2: u8) -> u64 {
        pack_expr_data(mix_hash(5, mix_hash(hash1, hash2)), 0, 0, bits2 << 2)
    }
    pub(crate) const fn expr_bvar_data(n: u64) -> u64 {
        pack_expr_data(mix_hash(7, n), n as u32 + 1, 0, 0)
    }
    pub(crate) const fn expr_sort_data(data: u64) -> u64 {
        let (h, _, bits) = unpack_level_data(data);
        pack_expr_data(mix_hash(11, h as u64), 0, 0, bits << 2)
    }
    pub(crate) const fn expr_fvar_data(hash: u64) -> u64 {
        pack_expr_data(mix_hash(13, mix_hash(0, hash)), 0, 0, 1)
    }
    pub(crate) const fn expr_mvar_data(hash: u64) -> u64 {
        pack_expr_data(mix_hash(17, mix_hash(0, hash)), 0, 0, 2)
    }
    pub(crate) const fn expr_mdata_data(data: u64) -> u64 {
        let (h, bvars, depth, bits) = unpack_expr_data(data);
        let h = mix_hash(depth as u64 + 1, h);
        pack_expr_data(h, bvars, depth.saturating_add(1), bits)
    }
    pub(crate) const fn expr_proj_data(hash1: u64, num2: u64, data3: u64) -> u64 {
        let (h, bvars, depth, bits) = unpack_expr_data(data3);
        let h = mix_hash(depth as u64 + 1, mix_hash(hash1, mix_hash(num2, h)));
        pack_expr_data(h, bvars, depth.saturating_add(1), bits)
    }
    pub(crate) fn expr_app_data(data1: u64, data2: u64) -> u64 {
        let (_, bvars1, depth1, bits1) = unpack_expr_data(data1);
        let (_, bvars2, depth2, bits2) = unpack_expr_data(data2);
        let depth = depth1.max(depth2).saturating_add(1);
        pack_expr_data(
            mix_hash(data1, data2),
            bvars1.max(bvars2),
            depth,
            bits1 | bits2,
        )
    }
    pub(crate) fn expr_binder_data(data1: u64, data2: u64) -> u64 {
        let (h1, bvars1, depth1, bits1) = unpack_expr_data(data1);
        let (h2, bvars2, depth2, bits2) = unpack_expr_data(data2);
        let depth = depth1.max(depth2);
        let h = mix_hash(depth as u64 + 1, mix_hash(h1, h2));
        let bvars = bvars1.max(bvars2.saturating_sub(1));
        pack_expr_data(h, bvars, depth.saturating_add(1), bits1 | bits2)
    }
    pub(crate) fn expr_let_data(data1: u64, data2: u64, data3: u64) -> u64 {
        let (h1, bvars1, depth1, bits1) = unpack_expr_data(data1);
        let (h2, bvars2, depth2, bits2) = unpack_expr_data(data2);
        let (h3, bvars3, depth3, bits3) = unpack_expr_data(data3);
        let depth = depth1.max(depth2).max(depth3);
        let h = mix_hash(depth as u64 + 1, mix_hash(h1, mix_hash(h2, h3)));
        let bvars = bvars1.max(bvars2).max(bvars3.saturating_sub(1));
        pack_expr_data(h, bvars, depth.saturating_add(1), bits1 | bits2 | bits3)
    }
    pub(crate) fn expr_lit_data(hash: u64) -> u64 {
        pack_expr_data(mix_hash(3, hash), 0, 0, 0)
    }
}

const NAME_ANON_HASH: u64 = 1723;

const fn unpack_ctor(tag: u8) -> (u8, u16) {
    let n = tag & 15;
    (n >> 1, (n & 1) as u16)
}

const fn mix_hash(mut h: u64, mut k: u64) -> u64 {
    let m: u64 = 0xc6a4a7935bd1e995;
    let r: u64 = 47;
    k = k.wrapping_mul(m);
    k ^= k >> r;
    k ^= m;
    h ^= k;
    h = h.wrapping_mul(m);
    h
}

/// Lean's `String.hash` (MurmurHash64A with seed 11).
fn str_hash(bytes: &[u8]) -> u64 {
    const M: u64 = 0xc6a4a7935bd1e995;
    const R: u64 = 47;
    let mut h: u64 = 11 ^ (bytes.len() as u64).wrapping_mul(M);
    let (words, rest) = bytes.as_chunks::<8>();
    for word in words {
        let mut k = u64::from_le_bytes(*word);
        k = k.wrapping_mul(M);
        k ^= k >> R;
        k = k.wrapping_mul(M);
        h ^= k;
        h = h.wrapping_mul(M);
    }
    if !rest.is_empty() {
        let mut buf = [0; 8];
        buf[..rest.len()].copy_from_slice(rest);
        h ^= u64::from_le_bytes(buf);
        h = h.wrapping_mul(M);
    }
    h ^= h >> R;
    h = h.wrapping_mul(M);
    h ^= h >> R;
    h
}

/// The padding after `pos` to a multiple of `n`, a power of two, and the
/// padded position.
fn pad_to(pos: usize, n: u8) -> (usize, usize) {
    let i = (n.wrapping_sub(pos as u8) & (n - 1)).into();
    (i, pos + i)
}

fn lean_version(major: u8, minor: u8, patch: u8, rc_m1: u8) -> String {
    if rc_m1 == u8::MAX {
        format!("{major}.{minor}.{patch}")
    } else {
        format!("{major}.{minor}.{patch}-rc{}", u16::from(rc_m1) + 1)
    }
}

/// Decodes an `LGZ2` stream into its `parts` `.olean` files: one, or the
/// `.olean`, `.olean.server` and `.olean.private` of a module.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Normal,
    Exprish,
}

/// The header of an object already written.
struct Header {
    cs_sz: u16,
    num_fields: u8,
    tag: u8,
}

impl Header {
    fn sfields(&self) -> u16 {
        (self.cs_sz >> 3).saturating_sub(u16::from(self.num_fields) + 1)
    }
}

/// Read access to compacted objects: the bytes of `buf` start at address
/// `offset`.
#[derive(Clone, Copy)]
struct Objects<'a> {
    buf: &'a [u8],
    offset: u64,
    gmp: bool,
}

impl Objects<'_> {
    /// The header of the object at `ptr` and the position of its first field.
    fn header(&self, ptr: u64) -> Result<(Header, usize)> {
        let at = ptr
            .checked_sub(self.offset)
            .and_then(|at| usize::try_from(at).ok())
            .ok_or_else(|| malformed("pointer outside the file"))?;
        let bytes = self
            .buf
            .get(at..)
            .and_then(|rest| rest.get(..8))
            .ok_or_else(|| malformed("pointer past the objects written"))?;
        let header = Header {
            cs_sz: u16::from_le_bytes([bytes[4], bytes[5]]),
            num_fields: bytes[6],
            tag: bytes[7],
        };
        Ok((header, at + 8))
    }

    fn word(&self, pos: usize) -> Result<u64> {
        let bytes = self
            .buf
            .get(pos..)
            .and_then(|rest| rest.get(..8))
            .ok_or_else(|| malformed("field past the objects written"))?;
        Ok(u64::from_le_bytes(bytes.try_into().expect("eight bytes")))
    }

    /// The scalar field after the pointer fields, where names, levels and
    /// expressions cache their hash.
    fn cached(&self, ptr: u64) -> Result<Option<u64>> {
        let (header, pos) = self.header(ptr)?;
        if header.sfields() == 0 {
            return Ok(None);
        }
        self.word(pos + 8 * usize::from(header.num_fields))
            .map(Some)
    }

    fn try_name_hash(&self, ptr: u64) -> Result<Option<u64>> {
        match ptr {
            1 => Ok(Some(NAME_ANON_HASH)),
            _ if ptr & 1 == 1 => Ok(None),
            _ => self.cached(ptr),
        }
    }

    fn name_hash(&self, ptr: u64) -> Result<u64> {
        self.try_name_hash(ptr)?
            .ok_or_else(|| malformed("expected a name"))
    }

    fn try_level_data(&self, ptr: u64) -> Result<Option<u64>> {
        match ptr {
            1 => Ok(Some(exprish::LEVEL_ZERO_DATA)),
            _ if ptr & 1 == 1 => Ok(None),
            _ => self.cached(ptr),
        }
    }

    fn try_expr_data(&self, ptr: u64) -> Result<Option<u64>> {
        if ptr & 1 == 1 {
            return Ok(None);
        }
        self.cached(ptr)
    }

    fn expr_data(&self, ptr: u64) -> Result<u64> {
        self.try_expr_data(ptr)?
            .ok_or_else(|| malformed("expected an expression"))
    }

    fn level_data(&self, ptr: u64) -> Result<u64> {
        self.try_level_data(ptr)?
            .ok_or_else(|| malformed("expected a level"))
    }

    fn try_num_hash(&self, ptr: u64) -> Result<Option<u64>> {
        if ptr & 1 == 1 {
            return Ok(Some(ptr >> 1));
        }
        let (header, pos) = self.header(ptr)?;
        if header.tag != tag::MPZ {
            return Ok(None);
        }
        // GMP: capacity and signed size as u32s, then the limb pointer;
        // otherwise sign, size and limb pointer as u64s.
        let (negative, limbs) = if self.gmp {
            (((self.word(pos)? >> 32) as i32) < 0, pos + 16)
        } else {
            (self.word(pos)? != 0, pos + 24)
        };
        let lo = self.word(limbs)?;
        Ok(Some(if negative { lo.wrapping_neg() } else { lo }))
    }

    fn num_hash(&self, ptr: u64) -> Result<u64> {
        self.try_num_hash(ptr)?
            .ok_or_else(|| malformed("expected a natural number"))
    }

    fn try_str_hash(&self, ptr: u64) -> Result<Option<u64>> {
        if ptr & 1 == 1 {
            return Ok(None);
        }
        let (header, pos) = self.header(ptr)?;
        if header.tag != tag::STRING {
            return Ok(None);
        }
        let size = self.word(pos)?;
        let bytes = size
            .checked_sub(1)
            .and_then(|len| self.buf.get(pos + 24..)?.get(..to_usize(len).ok()?))
            .ok_or_else(|| malformed("string past the objects written"))?;
        Ok(Some(str_hash(bytes)))
    }

    fn str_hash(&self, ptr: u64) -> Result<u64> {
        self.try_str_hash(ptr)?
            .ok_or_else(|| malformed("expected a string"))
    }

    fn try_lit_hash(&self, ptr: u64) -> Result<Option<u64>> {
        if ptr & 1 == 1 {
            return Ok(None);
        }
        let (header, pos) = self.header(ptr)?;
        match (header.num_fields, header.tag) {
            // Not a `Literal`, and there is no field to read.
            (0, _) => Ok(None),
            (_, 0) => self.try_num_hash(self.word(pos)?),
            (_, 1) => self.try_str_hash(self.word(pos)?),
            _ => Ok(None),
        }
    }

    fn lit_hash(&self, ptr: u64) -> Result<u64> {
        self.try_lit_hash(ptr)?
            .ok_or_else(|| malformed("expected a literal"))
    }

    fn list_level_data(&self, mut ptr: u64) -> Result<(u64, u8)> {
        let (mut hash, mut bits) = (7, 0);
        while ptr & 1 == 0 {
            let (header, pos) = self.header(ptr)?;
            if header.tag != 1 || header.num_fields < 2 {
                break;
            }
            let (head, tail) = (self.word(pos)?, self.word(pos + 8)?);
            if tail >= ptr {
                // Lean writes an object after the objects it points to, so the
                // tail of a list comes before it; stop rather than go round a
                // cycle.
                break;
            }
            let Some(data) = self.try_level_data(head)? else {
                break;
            };
            let (h, _, level_bits) = exprish::unpack_level_data(data);
            hash = mix_hash(hash, h.into());
            bits |= level_bits;
            ptr = tail;
        }
        Ok((hash, bits))
    }
}

fn to_usize(n: u64) -> Result<usize> {
    usize::try_from(n).map_err(|_| malformed("size too large"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lean_versions_encode_release_candidates() {
        assert_eq!(lean_version(4, 37, 0, u8::MAX), "4.37.0");
        assert_eq!(lean_version(4, 9, 0, 0), "4.9.0-rc1");
    }
}

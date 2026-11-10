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

use std::ops::Range;

use crate::{Input, Result, malformed, unsupported};

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
pub(crate) fn decompress(stream: &[u8], parts: usize) -> Result<Vec<Vec<u8>>> {
    let mut input = Input::new(stream);
    let magic = input.array::<4>()?;
    if &magic != MAGIC {
        return Err(unsupported("lgz magic", u32::from_be_bytes(magic).into()));
    }
    let base = u64::from(input.u32()?) << 16;
    let declared = input.u64()?;
    let githash = input.take(40)?;
    let gmp = input.u8()? & 1 != 0;
    let mut buf = Vec::with_capacity(declared.min(RESERVE_LIMIT) as usize);
    buf.extend_from_slice(b"olean");
    buf.push(2);
    buf.push(u8::from(gmp));
    let mut version = [0; 33];
    let major = input.u8()?;
    if major == 0 {
        version.copy_from_slice(input.take(33)?);
    } else {
        let encoded = lean_version(major, input.u8()?, input.u8()?, input.u8()?);
        version
            .get_mut(..encoded.len())
            .ok_or_else(|| malformed("Lean version is too long"))?
            .copy_from_slice(encoded.as_bytes());
    }
    buf.extend_from_slice(&version);
    buf.extend_from_slice(githash);
    let base_pos = buf.len();
    buf.extend_from_slice(&base.to_le_bytes());
    let fixup_pos = buf.len();
    // The root pointer, written once its object is.
    buf.extend_from_slice(&0u64.to_le_bytes());
    let header_size = buf.len();
    let mut decoder = Decoder {
        gmp,
        input,
        buf,
        offset: base,
        backrefs: Vec::new(),
        stack: Vec::new(),
        frames: Vec::new(),
    };
    let mut ranges: Vec<Range<usize>> = Vec::with_capacity(parts);
    let mut extra = 0;
    loop {
        let root = decoder.write_obj()?;
        decoder.buf[extra + fixup_pos..][..8].copy_from_slice(&root.to_le_bytes());
        ranges.push(extra..decoder.buf.len());
        if ranges.len() >= parts {
            break;
        }
        let buf = &mut decoder.buf;
        buf.resize(buf.len().next_multiple_of(1 << 16), 0);
        extra = buf.len();
        buf.extend_from_within(0..header_size);
        buf[extra + base_pos..][..8].copy_from_slice(&(base + extra as u64).to_le_bytes());
    }
    let mut buf = decoder.buf;
    if let [only] = &ranges[..] {
        buf.truncate(only.end);
        return Ok(vec![buf]);
    }
    Ok(ranges
        .into_iter()
        .map(|range| buf[range].to_vec())
        .collect())
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Normal,
    Exprish,
}

/// What `Decoder::write_obj` does after starting to read an object.
enum Step {
    /// Read an object in the mode: the first field of the object, which has
    /// pushed a frame, or the object itself, in the other mode.
    Read(Mode),
    /// Pass the value of the object, now complete (the pointer to it, or the
    /// boxed scalar), to the innermost frame.
    Done(u64),
}

/// An object whose fields the decoder is reading. An object is written after
/// its fields, so the objects waiting for them are kept on an explicit stack
/// instead of the call stack, which long chains would overflow.
#[derive(Clone, Copy)]
enum Frame {
    /// After `SAVE`: the object read next is also the next backref.
    Save,
    /// A constructor, whose fields read so far are on the stack from `start`.
    Ctor {
        ctor: u8,
        num_fields: u8,
        sfields: u16,
        start: usize,
    },
    /// An array of `size` elements, of which those read so far are on the
    /// stack from `start`.
    Array { size: u64, start: usize },
    /// A thunk, task, ref or promise, by its object tag, waiting for its value.
    Boxed(u8),
    /// An exprish object, whose fields are read in the modes `modes`; those
    /// read so far are on the stack from `start`.
    Exprish {
        tag: u8,
        modes: &'static [Mode],
        start: usize,
    },
    /// An application of `pos`, whose expression data is `data`, to `n` more
    /// arguments.
    App { pos: u64, data: u64, n: u8 },
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

struct Decoder<'a> {
    gmp: bool,
    input: Input<'a>,
    buf: Vec<u8>,
    /// The address of `buf[0]`.
    offset: u64,
    backrefs: Vec<u64>,
    /// Fields read for the objects in `frames`.
    stack: Vec<u64>,
    /// The objects waiting for their fields, the innermost last.
    frames: Vec<Frame>,
}

impl Decoder<'_> {
    fn pos(&self) -> u64 {
        self.offset + self.buf.len() as u64
    }

    fn write_u32(&mut self, n: u32) {
        self.buf.extend_from_slice(&n.to_le_bytes());
    }

    fn write_u64(&mut self, n: u64) {
        self.buf.extend_from_slice(&n.to_le_bytes());
    }

    fn write_header(&mut self, tag: u8, cs_sz: u16, num_fields: u8) -> u64 {
        let pos = self.pos();
        // The reference count of a compacted object is zero.
        self.write_u32(0);
        self.buf.extend_from_slice(&cs_sz.to_le_bytes());
        self.buf.extend_from_slice(&[num_fields, tag]);
        pos
    }

    /// Copies `size` input bytes, zero-padded to `padded`.
    fn copy(&mut self, size: usize, padded: usize) -> Result<()> {
        let bytes = self.input.take(size)?;
        self.buf.extend_from_slice(bytes);
        self.buf.resize(self.buf.len() + (padded - size), 0);
        Ok(())
    }

    /// Writes the fields on the stack from `start`.
    fn pop(&mut self, start: usize) {
        for field in self.stack.drain(start..) {
            self.buf.extend_from_slice(&field.to_le_bytes());
        }
    }

    fn read_i64(&mut self, tag: u8) -> Result<i64> {
        Ok(match tag {
            UINT0..=UINT0_END => (tag - UINT0).into(),
            INT1 => self.input.u8()? as i8 as i64,
            INT2 => self.input.u16()? as i16 as i64,
            INT4 => self.input.u32()? as i32 as i64,
            INT8 => self.input.u64()? as i64,
            _ => return Err(unsupported("integer opcode", tag.into())),
        })
    }

    fn read_size(&mut self) -> Result<u64> {
        let tag = self.input.u8()?;
        u64::try_from(self.read_i64(tag)?).map_err(|_| malformed("negative size"))
    }

    fn backref(&mut self, tag: u8) -> Result<u64> {
        let index: usize = match tag {
            BACKREF0..=BACKREF0_END => (tag - BACKREF0).into(),
            BACKREF1 => self.input.u8()?.into(),
            BACKREF2 => self.input.u16()?.into(),
            BACKREF4 => self.input.u32()? as usize,
            _ => return Err(unsupported("backref opcode", tag.into())),
        };
        self.backrefs
            .get(index)
            .copied()
            .ok_or_else(|| malformed("backref to an object not yet saved"))
    }

    fn write_ctor_header(&mut self, ctor: u8, num_fields: u8, sfields: u16) -> u64 {
        let cs_sz = u16::from(num_fields).wrapping_add(1).wrapping_add(sfields) << 3;
        self.write_header(ctor, cs_sz, num_fields)
    }

    /// Writes the constructor, whose fields are on the stack from `start`.
    fn write_ctor(&mut self, ctor: u8, num_fields: u8, sfields: u16, start: usize) -> Result<u64> {
        let pos = self.write_ctor_header(ctor, num_fields, sfields);
        self.pop(start);
        let size = usize::from(sfields << 3);
        self.copy(size, size)?;
        Ok(pos)
    }

    /// Writes a string object of `len` characters with `bytes`, which exclude
    /// the terminating NUL.
    fn write_str_obj(&mut self, bytes: &[u8], len: u64) -> u64 {
        let pos = self.write_header(tag::STRING, 1, 0);
        let size = bytes.len() as u64 + 1;
        self.write_u64(size);
        self.write_u64(size);
        self.write_u64(len);
        self.buf.extend_from_slice(bytes);
        let (padding, _) = pad_to(bytes.len() + 1, 8);
        self.buf.resize(self.buf.len() + 1 + padding, 0);
        pos
    }

    fn chars(bytes: &[u8]) -> u64 {
        bytes.iter().filter(|&&c| c & 0xC0 != 0x80).count() as u64
    }

    fn write_str(&mut self) -> Result<u64> {
        let bytes = self.input.cstr()?;
        Ok(self.write_str_obj(bytes, Self::chars(bytes)))
    }

    fn write_sized_str(&mut self) -> Result<u64> {
        let size = self.read_size()?;
        let bytes = self.input.take(to_usize(size)?)?;
        Ok(self.write_str_obj(bytes, Self::chars(bytes)))
    }

    /// Takes the `N` fields on the stack from `start`.
    fn take_fields<const N: usize>(&mut self, start: usize) -> Result<[u64; N]> {
        let fields: [u64; N] = self.stack[start..]
            .try_into()
            .map_err(|_| malformed("wrong number of fields"))?;
        self.stack.truncate(start);
        Ok(fields)
    }

    /// Reads an object and writes it with everything it points to, returning
    /// the pointer to it (or the boxed scalar).
    fn write_obj(&mut self) -> Result<u64> {
        let mut mode = Mode::Normal;
        loop {
            let step = match mode {
                Mode::Normal => self.start_obj()?,
                Mode::Exprish => self.start_exprish()?,
            };
            let mut value = match step {
                Step::Read(next) => {
                    mode = next;
                    continue;
                }
                Step::Done(value) => value,
            };
            // Pass the value to the innermost waiting object, and so on
            // outwards for each object that this completes.
            mode = loop {
                match self.frames.last() {
                    None => return Ok(value),
                    Some(&Frame::Save) => self.backrefs.push(value),
                    Some(&Frame::Ctor {
                        ctor,
                        num_fields,
                        sfields,
                        start,
                    }) => {
                        self.stack.push(value);
                        if self.stack.len() - start < num_fields.into() {
                            break Mode::Normal;
                        }
                        value = self.write_ctor(ctor, num_fields, sfields, start)?;
                    }
                    Some(&Frame::Array { size, start }) => {
                        self.stack.push(value);
                        if ((self.stack.len() - start) as u64) < size {
                            break Mode::Normal;
                        }
                        value = self.write_array(size, start);
                    }
                    Some(&Frame::Boxed(tag)) => value = self.write_boxed(tag, value),
                    Some(&Frame::Exprish { tag, modes, start }) => {
                        self.stack.push(value);
                        if let Some(&mode) = modes.get(self.stack.len() - start) {
                            break mode;
                        }
                        self.frames.pop();
                        match self.write_exprish(tag, start)? {
                            Step::Read(mode) => break mode,
                            Step::Done(pos) => {
                                value = pos;
                                continue;
                            }
                        }
                    }
                    Some(&Frame::App { pos, data, n }) => {
                        let (pos, data) = self.write_app(pos, data, value)?;
                        if n > 1 {
                            if let Some(frame) = self.frames.last_mut() {
                                *frame = Frame::App {
                                    pos,
                                    data,
                                    n: n - 1,
                                };
                            }
                            break Mode::Exprish;
                        }
                        value = pos;
                    }
                }
                self.frames.pop();
            }
        }
    }

    /// Writes the array of `size` elements, which are on the stack from `start`.
    fn write_array(&mut self, size: u64, start: usize) -> u64 {
        let pos = self.write_header(tag::ARRAY, 1, 0);
        self.write_u64(size);
        self.write_u64(size);
        self.pop(start);
        pos
    }

    /// Writes the thunk, task, ref or promise with object tag `tag`.
    fn write_boxed(&mut self, tag: u8, value: u64) -> u64 {
        let pos;
        if let tag::THUNK | tag::TASK = tag {
            pos = self.write_header(tag, 3 << 3, 0);
            self.write_u64(value);
            self.write_u64(0);
        } else {
            pos = self.write_header(tag, 2 << 3, 0);
            self.write_u64(value);
        }
        pos
    }

    /// Writes the application of `pos`, whose expression data is `data`, to
    /// `arg`, and returns the pointer to it and its expression data.
    fn write_app(&mut self, pos: u64, data: u64, arg: u64) -> Result<(u64, u64)> {
        let app = self.write_ctor_header(5, 2, 1);
        let data = exprish::expr_app_data(data, self.expr_data(arg)?);
        self.write_u64(pos);
        self.write_u64(arg);
        self.write_u64(data);
        Ok((app, data))
    }

    /// Applies `pos`, whose expression data is `data`, to the next `n`
    /// arguments.
    fn apply(&mut self, pos: u64, data: u64, n: u8) -> Step {
        if n == 0 {
            return Step::Done(pos);
        }
        self.frames.push(Frame::App { pos, data, n });
        Step::Read(Mode::Exprish)
    }

    /// Starts reading the `num_fields` fields of a constructor.
    fn start_ctor(&mut self, ctor: u8, num_fields: u8, sfields: u16) -> Result<Step> {
        let start = self.stack.len();
        if num_fields == 0 {
            return Ok(Step::Done(
                self.write_ctor(ctor, num_fields, sfields, start)?,
            ));
        }
        self.frames.push(Frame::Ctor {
            ctor,
            num_fields,
            sfields,
            start,
        });
        Ok(Step::Read(Mode::Normal))
    }

    fn start_exprish(&mut self) -> Result<Step> {
        use Mode::{Exprish as E, Normal as N};
        let tag = self.input.u8()?;
        let modes: &'static [Mode] = match tag {
            exprish::NORMAL => return Ok(Step::Read(N)),
            exprish::NAME_STR | exprish::NAME_NUM => &[E, N],
            exprish::LEVEL_SUCC | exprish::LEVEL_PARAM | exprish::LEVEL_MVAR => &[E],
            exprish::LEVEL_MAX | exprish::LEVEL_IMAX => &[E, E],
            exprish::EXPR_BVAR | exprish::EXPR_LIT => &[N],
            exprish::EXPR_FVAR | exprish::EXPR_MVAR | exprish::EXPR_SORT => &[E],
            exprish::EXPR_MDATA => &[N, E],
            exprish::EXPR_PROJ => &[E, N, E],
            exprish::EXPR_LET..=exprish::EXPR_LET_END => &[E, E, E, E],
            exprish::EXPR_LAMBDA..=exprish::EXPR_FORALL_END => &[E, E, E],
            0x21..=exprish::EXPR_APP_END => &[E],
            exprish::EXPR_CONST_APP..=exprish::EXPR_CONST_APP_END => &[E, N],
            UINT0..=UINT0_END | INT1 | INT2 | INT4 | INT8 => {
                return Ok(Step::Done(((self.read_i64(tag)? << 1) | 1) as u64));
            }
            SAVE => {
                self.frames.push(Frame::Save);
                return Ok(Step::Read(E));
            }
            BACKREF0..=BACKREF0_END | BACKREF1 | BACKREF2 | BACKREF4 => {
                return Ok(Step::Done(self.backref(tag)?));
            }
            _ => return Err(unsupported("exprish opcode", tag.into())),
        };
        self.frames.push(Frame::Exprish {
            tag,
            modes,
            start: self.stack.len(),
        });
        Ok(Step::Read(modes[0]))
    }

    /// Writes a constructor with `fields` and one scalar field, `data`.
    fn write_cached(&mut self, ctor: u8, fields: &[u64], data: u64) -> u64 {
        let pos = self.write_ctor_header(ctor, fields.len() as u8, 1);
        for &field in fields {
            self.write_u64(field);
        }
        self.write_u64(data);
        pos
    }

    /// Writes the exprish object `tag`, whose fields are on the stack from
    /// `start`.
    fn write_exprish(&mut self, tag: u8, start: usize) -> Result<Step> {
        let pos = match tag {
            exprish::NAME_STR => {
                let [prefix, string] = self.take_fields(start)?;
                let hash = mix_hash(self.name_hash(prefix)?, self.str_hash(string)?);
                self.write_cached(1, &[prefix, string], hash)
            }
            exprish::NAME_NUM => {
                let [prefix, num] = self.take_fields(start)?;
                let k = if num & 1 == 1 { num >> 1 } else { 17 };
                let hash = mix_hash(self.name_hash(prefix)?, k);
                self.write_cached(2, &[prefix, num], hash)
            }
            exprish::LEVEL_SUCC => {
                let [level] = self.take_fields(start)?;
                let data = exprish::level_succ_data(self.level_data(level)?);
                self.write_cached(1, &[level], data)
            }
            exprish::LEVEL_MAX | exprish::LEVEL_IMAX => {
                let [lhs, rhs] = self.take_fields(start)?;
                let (lhs_data, rhs_data) = (self.level_data(lhs)?, self.level_data(rhs)?);
                let (ctor, data) = if tag == exprish::LEVEL_MAX {
                    (2, exprish::level_max_data(lhs_data, rhs_data))
                } else {
                    (3, exprish::level_imax_data(lhs_data, rhs_data))
                };
                self.write_cached(ctor, &[lhs, rhs], data)
            }
            exprish::LEVEL_PARAM => {
                let [name] = self.take_fields(start)?;
                let data = exprish::level_param_data(self.name_hash(name)?);
                self.write_cached(4, &[name], data)
            }
            exprish::LEVEL_MVAR => {
                let [name] = self.take_fields(start)?;
                let data = exprish::level_mvar_data(self.name_hash(name)?);
                self.write_cached(5, &[name], data)
            }
            exprish::EXPR_BVAR => {
                let [index] = self.take_fields(start)?;
                let data = exprish::expr_bvar_data(self.num_hash(index)?);
                self.write_cached(0, &[index], data)
            }
            exprish::EXPR_FVAR => {
                let [id] = self.take_fields(start)?;
                let data = exprish::expr_fvar_data(self.name_hash(id)?);
                self.write_cached(1, &[id], data)
            }
            exprish::EXPR_MVAR => {
                let [id] = self.take_fields(start)?;
                let data = exprish::expr_mvar_data(self.name_hash(id)?);
                self.write_cached(2, &[id], data)
            }
            exprish::EXPR_SORT => {
                let [level] = self.take_fields(start)?;
                let data = exprish::expr_sort_data(self.level_data(level)?);
                self.write_cached(3, &[level], data)
            }
            exprish::EXPR_LIT => {
                let [lit] = self.take_fields(start)?;
                let data = exprish::expr_lit_data(self.lit_hash(lit)?);
                self.write_cached(9, &[lit], data)
            }
            exprish::EXPR_MDATA => {
                let [mdata, expr] = self.take_fields(start)?;
                let data = exprish::expr_mdata_data(self.expr_data(expr)?);
                self.write_cached(10, &[mdata, expr], data)
            }
            exprish::EXPR_PROJ => {
                let [name, index, expr] = self.take_fields(start)?;
                let data = exprish::expr_proj_data(
                    self.name_hash(name)?,
                    self.num_hash(index)?,
                    self.expr_data(expr)?,
                );
                self.write_cached(11, &[name, index, expr], data)
            }
            exprish::EXPR_LET..=exprish::EXPR_LET_END => {
                let [name, ty, value, body] = self.take_fields(start)?;
                let data = exprish::expr_let_data(
                    self.expr_data(ty)?,
                    self.expr_data(value)?,
                    self.expr_data(body)?,
                );
                let pos = self.write_ctor_header(8, 4, 2);
                for field in [name, ty, value, body, data, (tag & 1).into()] {
                    self.write_u64(field);
                }
                pos
            }
            exprish::EXPR_LAMBDA..=exprish::EXPR_FORALL_END => {
                let [name, ty, body] = self.take_fields(start)?;
                let data = exprish::expr_binder_data(self.expr_data(ty)?, self.expr_data(body)?);
                let pos = self.write_ctor_header(tag >> 2, 3, 2);
                for field in [name, ty, body, data, (tag & 3).into()] {
                    self.write_u64(field);
                }
                pos
            }
            0x21..=exprish::EXPR_APP_END => {
                let [function] = self.take_fields(start)?;
                let data = self.expr_data(function)?;
                return Ok(self.apply(function, data, tag & 0x1f));
            }
            exprish::EXPR_CONST_APP..=exprish::EXPR_CONST_APP_END => {
                let [name, levels] = self.take_fields(start)?;
                let hash = self.name_hash(name)?;
                let (levels_hash, bits) = self.list_level_data(levels)?;
                let data = exprish::expr_const_data(hash, levels_hash, bits);
                let pos = self.write_cached(4, &[name, levels], data);
                return Ok(self.apply(pos, data, tag & 0x1f));
            }
            _ => return Err(unsupported("exprish opcode", tag.into())),
        };
        Ok(Step::Done(pos))
    }

    fn start_obj(&mut self) -> Result<Step> {
        let tag = self.input.u8()?;
        let pos = match tag {
            BIG_CTOR => {
                let ctor = self.input.u8()?;
                let num_fields = self.input.u8()?;
                let sfields = self.input.u16()?;
                return self.start_ctor(ctor, num_fields, sfields);
            }
            SAVE => {
                self.frames.push(Frame::Save);
                return Ok(Step::Read(Mode::Normal));
            }
            BACKREF0..=BACKREF0_END | BACKREF1 | BACKREF2 | BACKREF4 => self.backref(tag)?,
            ARRAY => {
                let size = self.read_size()?;
                let start = self.stack.len();
                if size != 0 {
                    self.frames.push(Frame::Array { size, start });
                    return Ok(Step::Read(Mode::Normal));
                }
                self.write_array(size, start)
            }
            BYTE_ARRAY | SCALAR_ARRAY => {
                let elem_size = if tag == BYTE_ARRAY {
                    1
                } else {
                    self.input.u8()?
                };
                let size = self.read_size()?;
                let pos = self.write_header(tag::SCALAR_ARRAY, 1, elem_size);
                self.write_u64(size);
                self.write_u64(size);
                let bytes = to_usize(size)?
                    .checked_mul(elem_size.into())
                    .ok_or_else(|| malformed("scalar array too large"))?;
                self.copy(bytes, pad_to(bytes, 8).1)?;
                pos
            }
            STRING => self.write_str()?,
            SIZED_STRING => self.write_sized_str()?,
            MPZ => self.write_mpz()?,
            THUNK | TASK | REF | PROMISE => {
                let tag = match tag {
                    THUNK => tag::THUNK,
                    TASK => tag::TASK,
                    REF => tag::REF,
                    _ => tag::PROMISE,
                };
                self.frames.push(Frame::Boxed(tag));
                return Ok(Step::Read(Mode::Normal));
            }
            EXPRISH => return Ok(Step::Read(Mode::Exprish)),
            UINT0..=UINT0_END | INT1 | INT2 | INT4 | INT8 => {
                ((self.read_i64(tag)? << 1) | 1) as u64
            }
            // A small constructor: the index in the high nibble, the field
            // counts in the low one. A zero low nibble is an opcode above.
            _ if tag & 15 != 0 => {
                let (num_fields, sfields) = unpack_ctor(tag);
                return self.start_ctor(tag >> 4, num_fields, sfields);
            }
            _ => return Err(unsupported("opcode", tag.into())),
        };
        Ok(Step::Done(pos))
    }

    fn write_mpz(&mut self) -> Result<u64> {
        let tag = self.input.u8()?;
        let sign_size = self.read_i64(tag)? as i32;
        let capacity = sign_size as u32 & 0x7FFFFFFF;
        let size = (capacity as usize) << 3;
        if self.gmp {
            let pos = self.write_header(tag::MPZ, ((capacity + 3) as u16) << 3, 0);
            self.write_u32(capacity);
            self.write_u32(sign_size as u32);
            self.write_u64(self.pos() + 8);
            self.copy(size, size)?;
            return Ok(pos);
        }
        let limbs = self.input.take(size)?;
        if size == 0 {
            return Err(malformed("empty natural number"));
        }
        // The input has 64-bit limbs; drop a zero top half of the last one.
        let mut size2 = size >> 2;
        if limbs[size - 4..] == [0; 4] {
            size2 -= 1;
        }
        let pos = self.write_header(tag::MPZ, ((size2 + 8) as u16) << 2, 0);
        self.write_u64(u64::from(sign_size < 0));
        self.write_u64(size2 as u64);
        self.write_u64(self.pos() + 8);
        self.buf.extend_from_slice(limbs);
        Ok(pos)
    }

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

    fn name_hash(&self, ptr: u64) -> Result<u64> {
        let hash = match ptr {
            1 => Some(NAME_ANON_HASH),
            _ if ptr & 1 == 1 => None,
            _ => self.cached(ptr)?,
        };
        hash.ok_or_else(|| malformed("expected a name"))
    }

    fn try_level_data(&self, ptr: u64) -> Result<Option<u64>> {
        match ptr {
            1 => Ok(Some(exprish::LEVEL_ZERO_DATA)),
            _ if ptr & 1 == 1 => Ok(None),
            _ => self.cached(ptr),
        }
    }

    fn expr_data(&self, ptr: u64) -> Result<u64> {
        let data = if ptr & 1 == 0 {
            self.cached(ptr)?
        } else {
            None
        };
        data.ok_or_else(|| malformed("expected an expression"))
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

    fn lit_hash(&self, ptr: u64) -> Result<u64> {
        let hash = if ptr & 1 == 1 {
            None
        } else {
            let (header, pos) = self.header(ptr)?;
            match (header.num_fields, header.tag) {
                // Not a `Literal`, and there is no field to read.
                (0, _) => None,
                (_, 0) => self.try_num_hash(self.word(pos)?)?,
                (_, 1) => self.try_str_hash(self.word(pos)?)?,
                _ => None,
            }
        };
        hash.ok_or_else(|| malformed("expected a literal"))
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

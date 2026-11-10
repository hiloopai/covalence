//! Encoding `.olean` files as `LGZ2` streams.
//!
//! Copied from digama0/leangz `src/lgz.rs` (`compress`, `LgzWriter`,
//! `on_subobjs`, `pack_ctor`) at the commit `lgz.rs` records.
//! Modified by hiloop: olean v2 only; errors instead of panics; the output is
//! a buffer instead of a writer; a Lean version is coded compactly only when
//! decoding the code gives back its exact bytes; std `HashMap` instead of
//! hashbrown.
//! License: Apache-2.0, full text in LICENSE-leangz

use std::{borrow::Cow, collections::HashMap};

use super::{
    ARRAY, BACKREF0, BACKREF1, BACKREF2, BACKREF4, BIG_CTOR, BYTE_ARRAY, EXPRISH, INT1, INT2, INT4,
    INT8, MAGIC, MPZ, Mode, Objects, PROMISE, REF, SAVE, SCALAR_ARRAY, SIZED_STRING, STRING, TASK,
    THUNK, UINT0, exprish, lean_version, mix_hash, pad_to, tag, to_usize,
};
use crate::{Result, malformed, unsupported};

/// The size of an olean v2 header: magic, version, flags, Lean version,
/// githash, base address and root pointer.
const HEADER: usize = 96;

/// The packed opcode of a small constructor, if it has one.
const fn pack_ctor(ctor: u8, num_fields: u8, sfields: u16) -> Option<u8> {
    match (ctor, num_fields, sfields) {
        // Unused anyway: these constructors are scalars.
        (_, 0, 0) => None,
        (..=12, ..=7, ..=1) => Some((ctor << 4) | (num_fields << 1) | sfields as u8),
        _ => None,
    }
}

const _: () = {
    let mut tag = 0;
    while tag < 0xff {
        let (num_fields, sfields) = super::unpack_ctor(tag);
        if let Some(packed) = pack_ctor(tag >> 4, num_fields, sfields) {
            assert!(packed == tag);
        }
        tag += 1;
    }
};

struct Olean<'a> {
    gmp: bool,
    lean_version: &'a [u8],
    githash: &'a [u8],
    base: u64,
    root: u64,
    objects: &'a [u8],
}

fn word(bytes: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(bytes[at..at + 8].try_into().expect("eight bytes"))
}

impl<'a> Olean<'a> {
    fn parse(olean: &'a [u8]) -> Result<Self> {
        if olean.len() < HEADER || &olean[..5] != b"olean" {
            return Err(malformed("not an olean"));
        }
        if olean[5] != 2 {
            return Err(unsupported("olean version", olean[5].into()));
        }
        if olean[6] & !1 != 0 {
            return Err(malformed("unknown olean flags"));
        }
        Ok(Self {
            gmp: olean[6] != 0,
            lean_version: &olean[7..40],
            githash: &olean[40..80],
            base: word(olean, 80),
            root: word(olean, 88),
            objects: &olean[HEADER..],
        })
    }
}

/// The compact code of a Lean version, `4.<minor>.<patch>[-rc<n>]`, when
/// decoding it gives back exactly `version`.
fn version_code(version: &[u8]) -> Option<[u8; 4]> {
    let s = version.strip_prefix(b"4.")?;
    let end = s.iter().position(|&c| c == b'.')?;
    let minor: u8 = std::str::from_utf8(&s[..end]).ok()?.parse().ok()?;
    let s = &s[end + 1..];
    let s = &s[..s.iter().position(|&c| c == 0).unwrap_or(s.len())];
    let end = s.iter().position(|&c| c == b'-').unwrap_or(s.len());
    let patch: u8 = std::str::from_utf8(&s[..end]).ok()?.parse().ok()?;
    let rc: u8 = match s[end..].strip_prefix(b"-rc") {
        Some(rest) => std::str::from_utf8(rest).ok()?.parse().ok()?,
        None if s[end..].is_empty() => 0,
        None => return None,
    };
    let rc_m1 = rc.wrapping_sub(1);
    let decoded = lean_version(4, minor, patch, rc_m1);
    let mut padded = [0; 33];
    padded
        .get_mut(..decoded.len())?
        .copy_from_slice(decoded.as_bytes());
    (padded[..] == *version).then_some([4, minor, patch, rc_m1])
}

/// Encodes an `.olean`, or the `.olean`, `.olean.server` and
/// `.olean.private` of a module, as one `LGZ2` stream.
pub(crate) fn compress(oleans: &[&[u8]]) -> Result<Vec<u8>> {
    let (first, rest) = oleans
        .split_first()
        .ok_or_else(|| malformed("no olean to compress"))?;
    let head = Olean::parse(first)?;
    if head.base & !(u64::from(u32::MAX) << 16) != 0 {
        return Err(malformed("olean base is not 64 KiB aligned"));
    }
    let offset = head.base + HEADER as u64;
    let mut end = head.base + first.len() as u64;
    // The address of each part's objects relative to the first's, and its root.
    let mut parts = vec![(0, head.objects, head.root)];
    for olean in rest {
        let part = Olean::parse(olean)?;
        if (part.gmp, part.lean_version, part.githash)
            != (head.gmp, head.lean_version, head.githash)
        {
            return Err(malformed("module oleans disagree on their header"));
        }
        end = end.next_multiple_of(1 << 16);
        if part.base != end {
            return Err(malformed("module oleans are not consecutive"));
        }
        end += olean.len() as u64;
        parts.push((to_usize(part.base - head.base)?, part.objects, part.root));
    }
    let (last, last_objects, _) = parts[parts.len() - 1];
    let len = last + last_objects.len();
    let buf: Cow<'_, [u8]> = if rest.is_empty() {
        Cow::Borrowed(head.objects)
    } else {
        let mut buf = vec![0; len];
        for (at, objects, _) in &parts {
            buf[*at..][..objects.len()].copy_from_slice(objects);
        }
        Cow::Owned(buf)
    };
    let objects = Objects {
        buf: &buf,
        offset,
        gmp: head.gmp,
    };
    let refs = count_refs(objects, &parts)?;
    let mut out = Vec::with_capacity(len / 4);
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&((head.base >> 16) as u32).to_le_bytes());
    out.extend_from_slice(&(end - head.base).to_le_bytes());
    out.extend_from_slice(head.githash);
    out.push(u8::from(head.gmp));
    match version_code(head.lean_version) {
        Some(code) => out.extend_from_slice(&code),
        None => {
            out.push(0);
            out.extend_from_slice(head.lean_version);
        }
    }
    let mut encoder = Encoder {
        objects,
        budget: refs.len(),
        refs: &refs,
        out,
        backrefs: HashMap::new(),
        tasks: Vec::new(),
    };
    for (_, _, root) in &parts {
        encoder.write_obj(*root, Mode::Normal)?;
    }
    Ok(encoder.out)
}

/// How many times each object is pointed to (saturating), by 8-byte word.
fn count_refs(objects: Objects<'_>, parts: &[(usize, &[u8], u64)]) -> Result<Vec<u8>> {
    let mut refs = vec![0u8; objects.buf.len() >> 3];
    let mut count = |ptr: u64| -> Result<()> {
        if ptr & 1 == 1 {
            return Ok(());
        }
        let slot = ptr
            .checked_sub(objects.offset)
            .and_then(|at| refs.get_mut(usize::try_from(at >> 3).ok()?))
            .ok_or_else(|| malformed("pointer outside the file"))?;
        *slot = slot.saturating_add(1);
        Ok(())
    };
    for &(at, part, root) in parts {
        let mut pos = at;
        while pos < at + part.len() {
            pos = pad_to(on_subobjs(objects, pos, &mut count)?, 8).1;
        }
        count(root)?;
    }
    Ok(refs)
}

/// Calls `f` on each pointer field of the object at index `pos`, and returns
/// the index after it.
fn on_subobjs(
    objects: Objects<'_>,
    pos: usize,
    mut f: impl FnMut(u64) -> Result<()>,
) -> Result<usize> {
    let ptr = objects.offset + pos as u64;
    let (header, at) = objects.header(ptr)?;
    let check = |ok: bool| {
        ok.then_some(())
            .ok_or_else(|| malformed("inconsistent object header"))
    };
    check(objects.word(pos)? as u32 == 0)?;
    let cs_sz = usize::from(header.cs_sz);
    let fields = |n: u64, at: usize, f: &mut dyn FnMut(u64) -> Result<()>| -> Result<usize> {
        for i in 0..to_usize(n)? {
            f(objects.word(at + 8 * i)?)?;
        }
        Ok(at + 8 * to_usize(n)?)
    };
    Ok(match header.tag {
        tag::ARRAY => {
            let size = objects.word(at)?;
            check(size == objects.word(at + 8)? && cs_sz == 1 && header.num_fields == 0)?;
            fields(size, at + 16, &mut f)?
        }
        tag::SCALAR_ARRAY => {
            let size = objects.word(at)?;
            check(cs_sz == 1 && header.num_fields != 0 && size == objects.word(at + 8)?)?;
            at + 16 + usize::from(header.num_fields) * to_usize(size)?
        }
        tag::STRING => {
            let size = objects.word(at)?;
            check(cs_sz == 1 && header.num_fields == 0 && size == objects.word(at + 8)?)?;
            at + 24 + to_usize(size)?
        }
        tag::MPZ if objects.gmp => {
            let sizes = objects.word(at)?;
            let (capacity, size) = (sizes as u32, (sizes >> 32) as i32);
            check(
                cs_sz == usize::from((capacity.wrapping_add(3) as u16) << 3)
                    && header.num_fields == 0
                    && size.unsigned_abs() == capacity
                    && capacity != 0,
            )?;
            at + 16 + 8 * capacity as usize
        }
        tag::MPZ => {
            let (sign, size) = (objects.word(at)?, objects.word(at + 8)?);
            check(
                cs_sz == usize::from(((size + 8) as u16) << 2)
                    && header.num_fields == 0
                    && sign < 2
                    && size != 0,
            )?;
            at + 24 + 4 * to_usize(size)?
        }
        tag::THUNK | tag::TASK => {
            check(cs_sz == 3 << 3 && header.num_fields == 0 && objects.word(at + 8)? == 0)?;
            fields(1, at, &mut f)?;
            at + 16
        }
        tag::REF | tag::PROMISE => {
            check(cs_sz == 2 << 3 && header.num_fields == 0)?;
            fields(1, at, &mut f)?
        }
        object if object >= tag::PROMISE => return Err(unsupported("object tag", object.into())),
        _ => {
            check(8 + 8 * usize::from(header.num_fields) <= cs_sz && cs_sz & 7 == 0)?;
            fields(header.num_fields.into(), at, &mut f)?;
            pos + cs_sz
        }
    })
}

/// A step of `Encoder::write_obj`. Objects can point to each other in chains
/// as long as the data, such as the cells of a list, so the encoder keeps
/// what it has yet to write on an explicit stack instead of recursing.
enum Task {
    /// Write the object `ptr` (or the scalar, if `ptr` is one) in the mode.
    Obj(u64, Mode),
    /// Write the `n` pointers at index `pos`, in normal mode.
    Fields(usize, u64),
    /// Copy `len` bytes at index `pos`.
    Raw(usize, usize),
    /// Record the backref of `ptr`, whose encoding is now complete.
    Save(u64),
}

struct Encoder<'a> {
    objects: Objects<'a>,
    refs: &'a [u8],
    out: Vec<u8>,
    backrefs: HashMap<u64, u32>,
    /// What remains to be written, the next step last.
    tasks: Vec<Task>,
    /// How many more objects there is room for; see `write_one`.
    budget: usize,
}

impl<'a> Encoder<'a> {
    fn write_op(&mut self, current: Mode, mode: Mode, op: u8) {
        match (current, mode) {
            (Mode::Normal, Mode::Exprish) => self.out.extend_from_slice(&[EXPRISH, op]),
            (Mode::Exprish, Mode::Normal) => self.out.extend_from_slice(&[exprish::NORMAL, op]),
            _ => self.out.push(op),
        }
    }

    fn write_i64(&mut self, mode: Mode, n: i64) {
        if (0..32).contains(&n) {
            self.write_op(mode, mode, UINT0 + n as u8);
        } else if let Ok(n) = i8::try_from(n) {
            self.write_op(mode, mode, INT1);
            self.out.extend_from_slice(&n.to_le_bytes());
        } else if let Ok(n) = i16::try_from(n) {
            self.write_op(mode, mode, INT2);
            self.out.extend_from_slice(&n.to_le_bytes());
        } else if let Ok(n) = i32::try_from(n) {
            self.write_op(mode, mode, INT4);
            self.out.extend_from_slice(&n.to_le_bytes());
        } else {
            self.write_op(mode, mode, INT8);
            self.out.extend_from_slice(&n.to_le_bytes());
        }
    }

    fn write_size(&mut self, size: u64) -> Result<()> {
        let size = i64::try_from(size).map_err(|_| malformed("size too large"))?;
        self.write_i64(Mode::Normal, size);
        Ok(())
    }

    fn write_backref(&mut self, mode: Mode, n: u32) {
        if n < 8 {
            self.write_op(mode, mode, BACKREF0 + n as u8);
        } else if let Ok(n) = u8::try_from(n) {
            self.write_op(mode, mode, BACKREF1);
            self.out.push(n);
        } else if let Ok(n) = u16::try_from(n) {
            self.write_op(mode, mode, BACKREF2);
            self.out.extend_from_slice(&n.to_le_bytes());
        } else {
            self.write_op(mode, mode, BACKREF4);
            self.out.extend_from_slice(&n.to_le_bytes());
        }
    }

    fn is_reused(&self, ptr: u64) -> Result<bool> {
        let count = ptr
            .checked_sub(self.objects.offset)
            .and_then(|at| self.refs.get(usize::try_from(at >> 3).ok()?))
            .ok_or_else(|| malformed("pointer outside the file"))?;
        Ok(*count > 1)
    }

    fn bytes(&self, at: usize, len: usize) -> Result<&'a [u8]> {
        self.objects
            .buf
            .get(at..)
            .and_then(|rest| rest.get(..len))
            .ok_or_else(|| malformed("object past the end of the file"))
    }

    fn defer(&mut self, task: Task) {
        self.tasks.push(task);
    }

    fn write_obj(&mut self, ptr: u64, mode: Mode) -> Result<()> {
        self.start(ptr, mode)?;
        while let Some(task) = self.tasks.pop() {
            match task {
                Task::Obj(ptr, mode) => self.start(ptr, mode)?,
                Task::Fields(pos, n) => {
                    let ptr = self.objects.word(pos)?;
                    if n > 1 {
                        self.tasks.push(Task::Fields(pos + 8, n - 1));
                    }
                    self.start(ptr, Mode::Normal)?;
                }
                Task::Raw(pos, len) => {
                    let bytes = self.bytes(pos, len)?;
                    self.out.extend_from_slice(bytes);
                }
                Task::Save(ptr) => {
                    let index = u32::try_from(self.backrefs.len())
                        .map_err(|_| malformed("too many shared objects"))?;
                    self.backrefs.insert(ptr, index);
                }
            }
        }
        Ok(())
    }

    /// Writes the object `ptr` up to its first field and puts the rest on
    /// the stack of tasks.
    fn start(&mut self, ptr: u64, mode: Mode) -> Result<()> {
        let mark = self.tasks.len();
        self.write_one(ptr, mode)?;
        self.tasks[mark..].reverse();
        Ok(())
    }

    /// Writes the object `ptr` up to its first field and defers the rest, in
    /// order.
    fn write_one(&mut self, ptr: u64, mode: Mode) -> Result<()> {
        if ptr & 1 == 1 {
            self.write_i64(mode, ptr as i64 >> 1);
            return Ok(());
        }
        if let Some(&index) = self.backrefs.get(&ptr) {
            self.write_backref(mode, index);
            return Ok(());
        }
        // Each object is written once, then only referred to, unless the
        // objects point to each other in a cycle, which Lean never writes.
        // There are no more objects than 8-byte words.
        self.budget = self
            .budget
            .checked_sub(1)
            .ok_or_else(|| malformed("the objects point to each other in a cycle"))?;
        let save = self.is_reused(ptr)?;
        if save {
            self.write_op(mode, mode, SAVE);
        }
        let (header, pos) = self.objects.header(ptr)?;
        match header.tag {
            tag::ARRAY => {
                self.write_op(mode, Mode::Normal, ARRAY);
                let size = self.objects.word(pos)?;
                self.write_size(size)?;
                if size != 0 {
                    self.defer(Task::Fields(pos + 16, size));
                }
            }
            tag::SCALAR_ARRAY => {
                let elem_size = header.num_fields;
                if elem_size == 1 {
                    self.write_op(mode, Mode::Normal, BYTE_ARRAY);
                } else {
                    self.write_op(mode, Mode::Normal, SCALAR_ARRAY);
                    self.out.push(elem_size);
                }
                let size = self.objects.word(pos)?;
                self.write_size(size)?;
                let len = to_usize(size)?
                    .checked_mul(elem_size.into())
                    .ok_or_else(|| malformed("scalar array too large"))?;
                let bytes = self.bytes(pos + 16, len)?;
                self.out.extend_from_slice(bytes);
            }
            tag::STRING => {
                let size = to_usize(self.objects.word(pos)?)?;
                let bytes = self.bytes(pos + 24, size)?;
                let Some((0, chars)) = bytes.split_last() else {
                    return Err(malformed("string is not NUL-terminated"));
                };
                if chars.contains(&0) {
                    // `STRING` ends at the first NUL.
                    self.write_op(mode, Mode::Normal, SIZED_STRING);
                    self.write_size(chars.len() as u64)?;
                    self.out.extend_from_slice(chars);
                } else {
                    self.write_op(mode, Mode::Normal, STRING);
                    self.out.extend_from_slice(bytes);
                }
            }
            tag::MPZ => {
                let (capacity, sign_size, limbs) = if self.objects.gmp {
                    let sizes = self.objects.word(pos)?;
                    (sizes as u32 as usize, (sizes >> 32) as u32, pos + 16)
                } else {
                    let (sign, size) = (self.objects.word(pos)?, self.objects.word(pos + 8)?);
                    let capacity = (size + 1) >> 1;
                    let capacity = i32::try_from(capacity)
                        .map_err(|_| malformed("natural number too large"))?;
                    let sign_size = (if sign != 0 { -capacity } else { capacity }) as u32;
                    let capacity = capacity as usize;
                    if size & 1 != 0 && self.bytes(pos + 24 + 8 * capacity - 4, 4)? != [0; 4] {
                        return Err(malformed("natural number has a nonzero pad"));
                    }
                    (capacity, sign_size, pos + 24)
                };
                self.write_op(mode, Mode::Normal, MPZ);
                self.write_i64(Mode::Normal, sign_size.into());
                let bytes = self.bytes(limbs, 8 * capacity)?;
                self.out.extend_from_slice(bytes);
            }
            tag::THUNK | tag::TASK | tag::REF | tag::PROMISE => {
                let op = match header.tag {
                    tag::THUNK => THUNK,
                    tag::TASK => TASK,
                    tag::REF => REF,
                    _ => PROMISE,
                };
                self.write_op(mode, Mode::Normal, op);
                let value = self.objects.word(pos)?;
                self.defer(Task::Obj(value, Mode::Normal));
            }
            object if object >= tag::PROMISE => {
                return Err(unsupported("object tag", object.into()));
            }
            ctor => {
                let sfields = (header.cs_sz >> 3)
                    .checked_sub(1 + u16::from(header.num_fields))
                    .ok_or_else(|| malformed("constructor smaller than its fields"))?;
                if !self.try_exprish(pos, mode, ctor, header.num_fields, sfields)? {
                    match pack_ctor(ctor, header.num_fields, sfields) {
                        Some(packed) => self.write_op(mode, Mode::Normal, packed),
                        None => {
                            self.write_op(mode, Mode::Normal, BIG_CTOR);
                            self.out.extend_from_slice(&[ctor, header.num_fields]);
                            self.out.extend_from_slice(&sfields.to_le_bytes());
                        }
                    }
                    if header.num_fields != 0 {
                        self.defer(Task::Fields(pos, header.num_fields.into()));
                    }
                    if sfields != 0 {
                        self.defer(Task::Raw(
                            pos + 8 * usize::from(header.num_fields),
                            8 * usize::from(sfields),
                        ));
                    }
                }
            }
        }
        if save {
            self.defer(Task::Save(ptr));
        }
        Ok(())
    }

    /// Writes the exprish opcode `op` and defers `fields` in their modes.
    fn exprish(&mut self, mode: Mode, op: u8, fields: &[(u64, Mode)]) -> bool {
        self.write_op(mode, Mode::Exprish, op);
        for &(ptr, mode) in fields {
            self.defer(Task::Obj(ptr, mode));
        }
        true
    }

    /// Writes the name, level or expression at index `pos` as exprish, which
    /// drops its cached data, if the data is what the decoder recomputes.
    fn try_exprish(
        &mut self,
        pos: usize,
        mode: Mode,
        ctor: u8,
        num_fields: u8,
        sfields: u16,
    ) -> Result<bool> {
        use Mode::{Exprish as E, Normal as N};
        let o = self.objects;
        let field = |i: usize| o.word(pos + 8 * i);
        let data = || o.word(pos + 8 * usize::from(num_fields));
        Ok(match (ctor, num_fields, sfields) {
            (1, 2, 1) => {
                // Name.str
                let (prefix, string) = (field(0)?, field(1)?);
                match (o.try_name_hash(prefix)?, o.try_str_hash(string)?) {
                    (Some(h1), Some(h2)) if mix_hash(h1, h2) == data()? => {
                        self.exprish(mode, exprish::NAME_STR, &[(prefix, E), (string, N)])
                    }
                    _ => false,
                }
            }
            (1, 1, 1) => {
                // Level.succ, else Expr.fvar
                let ptr = field(0)?;
                let data = data()?;
                if o.try_level_data(ptr)?
                    .is_some_and(|level| exprish::level_succ_data(level) == data)
                {
                    self.exprish(mode, exprish::LEVEL_SUCC, &[(ptr, E)])
                } else if o
                    .try_name_hash(ptr)?
                    .is_some_and(|hash| exprish::expr_fvar_data(hash) == data)
                {
                    self.exprish(mode, exprish::EXPR_FVAR, &[(ptr, E)])
                } else {
                    false
                }
            }
            (2, 2, 1) => {
                // Level.max, else Name.num
                let (lhs, rhs) = (field(0)?, field(1)?);
                let data = data()?;
                let max = match (o.try_level_data(lhs)?, o.try_level_data(rhs)?) {
                    (Some(l), Some(r)) => exprish::level_max_data(l, r) == data,
                    _ => false,
                };
                if max {
                    self.exprish(mode, exprish::LEVEL_MAX, &[(lhs, E), (rhs, E)])
                } else if o.try_name_hash(lhs)?.is_some_and(|h1| {
                    mix_hash(h1, if rhs & 1 == 1 { rhs >> 1 } else { 17 }) == data
                }) {
                    self.exprish(mode, exprish::NAME_NUM, &[(lhs, E), (rhs, N)])
                } else {
                    false
                }
            }
            (3, 2, 1) => {
                // Level.imax
                let (lhs, rhs) = (field(0)?, field(1)?);
                match (o.try_level_data(lhs)?, o.try_level_data(rhs)?) {
                    (Some(l), Some(r)) if exprish::level_imax_data(l, r) == data()? => {
                        self.exprish(mode, exprish::LEVEL_IMAX, &[(lhs, E), (rhs, E)])
                    }
                    _ => false,
                }
            }
            (4 | 5, 1, 1) => {
                // Level.param, Level.mvar
                let ptr = field(0)?;
                let (op, f): (u8, fn(u64) -> u64) = if ctor == 4 {
                    (exprish::LEVEL_PARAM, exprish::level_param_data)
                } else {
                    (exprish::LEVEL_MVAR, exprish::level_mvar_data)
                };
                let data = data()?;
                if o.try_name_hash(ptr)?.is_some_and(|hash| f(hash) == data) {
                    self.exprish(mode, op, &[(ptr, E)])
                } else {
                    false
                }
            }
            (0, 1, 1) => {
                // Expr.bvar
                let ptr = field(0)?;
                match o.try_num_hash(ptr)? {
                    Some(n) if exprish::expr_bvar_data(n) == data()? => {
                        self.exprish(mode, exprish::EXPR_BVAR, &[(ptr, N)])
                    }
                    _ => false,
                }
            }
            (2, 1, 1) => {
                // Expr.mvar
                let ptr = field(0)?;
                match o.try_name_hash(ptr)? {
                    Some(hash) if exprish::expr_mvar_data(hash) == data()? => {
                        self.exprish(mode, exprish::EXPR_MVAR, &[(ptr, E)])
                    }
                    _ => false,
                }
            }
            (3, 1, 1) => {
                // Expr.sort
                let ptr = field(0)?;
                match o.try_level_data(ptr)? {
                    Some(level) if exprish::expr_sort_data(level) == data()? => {
                        self.exprish(mode, exprish::EXPR_SORT, &[(ptr, E)])
                    }
                    _ => false,
                }
            }
            // Expr.const, Expr.app
            (4 | 5, 2, 1) => self.try_nary_app(pos, mode, ctor, num_fields, sfields)?,
            (6 | 7, 3, 2) => {
                // Expr.lam, Expr.forallE
                let (name, ty, body) = (field(0)?, field(1)?, field(2)?);
                let (data, binder) = (field(3)?, field(4)?);
                match (o.try_expr_data(ty)?, o.try_expr_data(body)?) {
                    (Some(d1), Some(d2))
                        if exprish::expr_binder_data(d1, d2) == data && binder & !3 == 0 =>
                    {
                        let op = (ctor << 2) + binder as u8;
                        self.exprish(mode, op, &[(name, E), (ty, E), (body, E)])
                    }
                    _ => false,
                }
            }
            (8, 4, 2) => {
                // Expr.letE
                let (name, ty, value, body) = (field(0)?, field(1)?, field(2)?, field(3)?);
                let (data, non_dep) = (field(4)?, field(5)?);
                let datas = (
                    o.try_expr_data(ty)?,
                    o.try_expr_data(value)?,
                    o.try_expr_data(body)?,
                );
                match datas {
                    (Some(d1), Some(d2), Some(d3))
                        if exprish::expr_let_data(d1, d2, d3) == data && non_dep & !1 == 0 =>
                    {
                        let op = exprish::EXPR_LET + non_dep as u8;
                        self.exprish(mode, op, &[(name, E), (ty, E), (value, E), (body, E)])
                    }
                    _ => false,
                }
            }
            (9, 1, 1) => {
                // Expr.lit
                let ptr = field(0)?;
                match o.try_lit_hash(ptr)? {
                    Some(hash) if exprish::expr_lit_data(hash) == data()? => {
                        self.exprish(mode, exprish::EXPR_LIT, &[(ptr, N)])
                    }
                    _ => false,
                }
            }
            (10, 2, 1) => {
                // Expr.mdata
                let (mdata, expr) = (field(0)?, field(1)?);
                match o.try_expr_data(expr)? {
                    Some(d) if exprish::expr_mdata_data(d) == data()? => {
                        self.exprish(mode, exprish::EXPR_MDATA, &[(mdata, N), (expr, E)])
                    }
                    _ => false,
                }
            }
            (11, 3, 1) => {
                // Expr.proj
                let (name, index, expr) = (field(0)?, field(1)?, field(2)?);
                let parts = (
                    o.try_name_hash(name)?,
                    o.try_num_hash(index)?,
                    o.try_expr_data(expr)?,
                );
                match parts {
                    (Some(h), Some(n), Some(d)) if exprish::expr_proj_data(h, n, d) == data()? => {
                        self.exprish(
                            mode,
                            exprish::EXPR_PROJ,
                            &[(name, E), (index, N), (expr, E)],
                        )
                    }
                    _ => false,
                }
            }
            _ => false,
        })
    }

    /// Writes a constant applied to arguments, or a chain of up to 31
    /// applications whose inner applications are not shared, as one opcode.
    fn try_nary_app(
        &mut self,
        pos: usize,
        mode: Mode,
        ctor: u8,
        num_fields: u8,
        sfields: u16,
    ) -> Result<bool> {
        let o = self.objects;
        let mut args = Vec::new();
        let mut function = None;
        let mut state = (pos, ctor, num_fields, sfields);
        loop {
            match state {
                (pos, 4, 2, 1) => {
                    // Expr.const
                    let (name, levels) = (o.word(pos)?, o.word(pos + 8)?);
                    let Some(hash) = o.try_name_hash(name)? else {
                        break;
                    };
                    let (levels_hash, bits) = o.list_level_data(levels)?;
                    if exprish::expr_const_data(hash, levels_hash, bits) == o.word(pos + 16)? {
                        self.write_op(
                            mode,
                            Mode::Exprish,
                            exprish::EXPR_CONST_APP + args.len() as u8,
                        );
                        self.defer(Task::Obj(name, Mode::Exprish));
                        self.defer(Task::Obj(levels, Mode::Normal));
                        for arg in args.into_iter().rev() {
                            self.defer(Task::Obj(arg, Mode::Exprish));
                        }
                        return Ok(true);
                    }
                }
                (pos, 5, 2, 1) if args.len() < 0x1f => {
                    // Expr.app
                    let (f, arg) = (o.word(pos)?, o.word(pos + 8)?);
                    let (Some(d1), Some(d2)) = (o.try_expr_data(f)?, o.try_expr_data(arg)?) else {
                        break;
                    };
                    if exprish::expr_app_data(d1, d2) == o.word(pos + 16)? {
                        function = Some(f);
                        args.push(arg);
                        if f & 1 == 0 && !self.is_reused(f)? {
                            let (header, pos) = o.header(f)?;
                            state = (pos, header.tag, header.num_fields, header.sfields());
                            continue;
                        }
                    }
                }
                _ => {}
            }
            break;
        }
        let Some(function) = function else {
            return Ok(false);
        };
        self.write_op(mode, Mode::Exprish, exprish::EXPR_APP + args.len() as u8);
        self.defer(Task::Obj(function, Mode::Exprish));
        for arg in args.into_iter().rev() {
            self.defer(Task::Obj(arg, Mode::Exprish));
        }
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_code_only_when_lossless() {
        let pad = |s: &[u8]| {
            let mut v = [0; 33];
            v[..s.len()].copy_from_slice(s);
            v
        };
        assert_eq!(version_code(&pad(b"4.37.0")), Some([4, 37, 0, 0xff]));
        assert_eq!(version_code(&pad(b"4.9.0-rc2")), Some([4, 9, 0, 1]));
        assert_eq!(version_code(&pad(b"4.037.0")), None);
        assert_eq!(version_code(&pad(b"4.37.0-nightly-2026-11-05")), None);
    }
}

//! Decoding `LGZ2` streams back into `.olean` files.

use std::ops::Range;

use super::{
    ARRAY, BACKREF0, BACKREF0_END, BACKREF1, BACKREF2, BACKREF4, BIG_CTOR, BYTE_ARRAY, EXPRISH,
    INT1, INT2, INT4, INT8, MAGIC, MPZ, Mode, Objects, PROMISE, REF, RESERVE_LIMIT, SAVE,
    SCALAR_ARRAY, SIZED_STRING, STRING, TASK, THUNK, UINT0, UINT0_END, exprish, lean_version,
    mix_hash, pad_to, tag, to_usize, unpack_ctor,
};
use crate::{Input, Result, malformed, unsupported};

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
    fn objects(&self) -> Objects<'_> {
        Objects {
            buf: &self.buf,
            offset: self.offset,
            gmp: self.gmp,
        }
    }

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
        let data = exprish::expr_app_data(data, self.objects().expr_data(arg)?);
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
                let hash = mix_hash(
                    self.objects().name_hash(prefix)?,
                    self.objects().str_hash(string)?,
                );
                self.write_cached(1, &[prefix, string], hash)
            }
            exprish::NAME_NUM => {
                let [prefix, num] = self.take_fields(start)?;
                let k = if num & 1 == 1 { num >> 1 } else { 17 };
                let hash = mix_hash(self.objects().name_hash(prefix)?, k);
                self.write_cached(2, &[prefix, num], hash)
            }
            exprish::LEVEL_SUCC => {
                let [level] = self.take_fields(start)?;
                let data = exprish::level_succ_data(self.objects().level_data(level)?);
                self.write_cached(1, &[level], data)
            }
            exprish::LEVEL_MAX | exprish::LEVEL_IMAX => {
                let [lhs, rhs] = self.take_fields(start)?;
                let (lhs_data, rhs_data) = (
                    self.objects().level_data(lhs)?,
                    self.objects().level_data(rhs)?,
                );
                let (ctor, data) = if tag == exprish::LEVEL_MAX {
                    (2, exprish::level_max_data(lhs_data, rhs_data))
                } else {
                    (3, exprish::level_imax_data(lhs_data, rhs_data))
                };
                self.write_cached(ctor, &[lhs, rhs], data)
            }
            exprish::LEVEL_PARAM => {
                let [name] = self.take_fields(start)?;
                let data = exprish::level_param_data(self.objects().name_hash(name)?);
                self.write_cached(4, &[name], data)
            }
            exprish::LEVEL_MVAR => {
                let [name] = self.take_fields(start)?;
                let data = exprish::level_mvar_data(self.objects().name_hash(name)?);
                self.write_cached(5, &[name], data)
            }
            exprish::EXPR_BVAR => {
                let [index] = self.take_fields(start)?;
                let data = exprish::expr_bvar_data(self.objects().num_hash(index)?);
                self.write_cached(0, &[index], data)
            }
            exprish::EXPR_FVAR => {
                let [id] = self.take_fields(start)?;
                let data = exprish::expr_fvar_data(self.objects().name_hash(id)?);
                self.write_cached(1, &[id], data)
            }
            exprish::EXPR_MVAR => {
                let [id] = self.take_fields(start)?;
                let data = exprish::expr_mvar_data(self.objects().name_hash(id)?);
                self.write_cached(2, &[id], data)
            }
            exprish::EXPR_SORT => {
                let [level] = self.take_fields(start)?;
                let data = exprish::expr_sort_data(self.objects().level_data(level)?);
                self.write_cached(3, &[level], data)
            }
            exprish::EXPR_LIT => {
                let [lit] = self.take_fields(start)?;
                let data = exprish::expr_lit_data(self.objects().lit_hash(lit)?);
                self.write_cached(9, &[lit], data)
            }
            exprish::EXPR_MDATA => {
                let [mdata, expr] = self.take_fields(start)?;
                let data = exprish::expr_mdata_data(self.objects().expr_data(expr)?);
                self.write_cached(10, &[mdata, expr], data)
            }
            exprish::EXPR_PROJ => {
                let [name, index, expr] = self.take_fields(start)?;
                let data = exprish::expr_proj_data(
                    self.objects().name_hash(name)?,
                    self.objects().num_hash(index)?,
                    self.objects().expr_data(expr)?,
                );
                self.write_cached(11, &[name, index, expr], data)
            }
            exprish::EXPR_LET..=exprish::EXPR_LET_END => {
                let [name, ty, value, body] = self.take_fields(start)?;
                let data = exprish::expr_let_data(
                    self.objects().expr_data(ty)?,
                    self.objects().expr_data(value)?,
                    self.objects().expr_data(body)?,
                );
                let pos = self.write_ctor_header(8, 4, 2);
                for field in [name, ty, value, body, data, (tag & 1).into()] {
                    self.write_u64(field);
                }
                pos
            }
            exprish::EXPR_LAMBDA..=exprish::EXPR_FORALL_END => {
                let [name, ty, body] = self.take_fields(start)?;
                let data = exprish::expr_binder_data(
                    self.objects().expr_data(ty)?,
                    self.objects().expr_data(body)?,
                );
                let pos = self.write_ctor_header(tag >> 2, 3, 2);
                for field in [name, ty, body, data, (tag & 3).into()] {
                    self.write_u64(field);
                }
                pos
            }
            0x21..=exprish::EXPR_APP_END => {
                let [function] = self.take_fields(start)?;
                let data = self.objects().expr_data(function)?;
                return Ok(self.apply(function, data, tag & 0x1f));
            }
            exprish::EXPR_CONST_APP..=exprish::EXPR_CONST_APP_END => {
                let [name, levels] = self.take_fields(start)?;
                let hash = self.objects().name_hash(name)?;
                let (levels_hash, bits) = self.objects().list_level_data(levels)?;
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
}

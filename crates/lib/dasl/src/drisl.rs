//! DRISL bytes to and from events.

use std::io::{self, Write};

use covalence_lib_cid::{Bdasl, Cid};
use covalence_lib_error::snafu::{OptionExt, ensure};

use crate::{Error, FloatSnafu, MalformedSnafu, event::Event};

const CID_TAG: u64 = 42;

pub(crate) fn write<W: Write>(out: &mut W, event: Event<'_>) -> io::Result<()> {
    match event {
        Event::Null => out.write_all(&[0xf6]),
        Event::Bool(value) => out.write_all(&[0xf4 | u8::from(value)]),
        Event::Integer(value @ 0..) => head(out, 0, value as u64),
        Event::Integer(value) => head(out, 1, !value as u64),
        Event::Bytes(value) => {
            head(out, 2, value.len() as u64)?;
            out.write_all(value)
        }
        Event::String(value) => {
            head(out, 3, value.len() as u64)?;
            out.write_all(value.as_bytes())
        }
        Event::List(len) => head(out, 4, len as u64),
        Event::Map(len) => head(out, 5, len as u64),
        Event::Link(cid) => {
            let cid = cid.to_bytes();
            head(out, 6, CID_TAG)?;
            head(out, 2, cid.len() as u64 + 1)?;
            out.write_all(&[0])?;
            out.write_all(&cid)
        }
    }
}

fn head<W: Write>(out: &mut W, major: u8, argument: u64) -> io::Result<()> {
    let major = major << 5;
    match argument {
        ..24 => out.write_all(&[major | argument as u8]),
        24..=0xff => out.write_all(&[major | 24, argument as u8]),
        0x100..=0xffff => {
            out.write_all(&[major | 25])?;
            out.write_all(&(argument as u16).to_be_bytes())
        }
        0x1_0000..=0xffff_ffff => {
            out.write_all(&[major | 26])?;
            out.write_all(&(argument as u32).to_be_bytes())
        }
        _ => {
            out.write_all(&[major | 27])?;
            out.write_all(&argument.to_be_bytes())
        }
    }
}

/// Reads minimal DRISL heads as events. Map key order is checked by the builder.
pub(crate) struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    pub(crate) fn new(bytes: &'a [u8]) -> Self {
        Reader(bytes)
    }

    fn event(&mut self) -> Result<Event<'a>, Error> {
        let [byte] = self.array()?;
        let (major, info) = (byte >> 5, byte & 0x1f);
        if major == 7 {
            return match info {
                20 => Ok(Event::Bool(false)),
                21 => Ok(Event::Bool(true)),
                22 => Ok(Event::Null),
                25..=27 => FloatSnafu.fail(),
                _ => malformed("unsupported simple value"),
            };
        }
        let argument = self.argument(info)?;
        Ok(match major {
            0 => Event::Integer(integer(argument)?),
            1 => Event::Integer(!integer(argument)?),
            2 => Event::Bytes(self.take(argument)?),
            3 => Event::String(str::from_utf8(self.take(argument)?).ok().context(
                MalformedSnafu {
                    message: "string is not UTF-8",
                },
            )?),
            4 => Event::List(length(argument)?),
            5 => Event::Map(length(argument)?),
            _ => Event::Link(self.link(argument)?),
        })
    }

    /// The CID after tag `tag`: a byte string holding a zero byte and a binary CID.
    fn link(&mut self, tag: u64) -> Result<Cid<Bdasl>, Error> {
        ensure!(
            tag == CID_TAG,
            MalformedSnafu {
                message: "unsupported tag"
            }
        );
        let [byte] = self.array()?;
        ensure!(
            byte >> 5 == 2,
            MalformedSnafu {
                message: "a CID must be a byte string"
            }
        );
        let argument = self.argument(byte & 0x1f)?;
        let [0, cid @ ..] = self.take(argument)? else {
            return malformed("a CID must start with a zero byte");
        };
        Ok(Cid::from_bytes(cid)?)
    }

    fn argument(&mut self, info: u8) -> Result<u64, Error> {
        let (argument, least) = match info {
            ..24 => return Ok(info.into()),
            24 => (u8::from_be_bytes(self.array()?).into(), 24),
            25 => (u16::from_be_bytes(self.array()?).into(), 0x100),
            26 => (u32::from_be_bytes(self.array()?).into(), 0x1_0000),
            27 => (u64::from_be_bytes(self.array()?), 0x1_0000_0000),
            _ => return malformed("indefinite length or reserved head"),
        };
        ensure!(
            argument >= least,
            MalformedSnafu {
                message: "head is not minimal"
            }
        );
        Ok(argument)
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N], Error> {
        Ok(self.take(N as u64)?.try_into().expect("took N bytes"))
    }

    fn take(&mut self, len: u64) -> Result<&'a [u8], Error> {
        let (taken, rest) = usize::try_from(len)
            .ok()
            .and_then(|len| self.0.split_at_checked(len))
            .context(MalformedSnafu {
                message: "unexpected end of input",
            })?;
        self.0 = rest;
        Ok(taken)
    }
}

impl<'a> Iterator for Reader<'a> {
    type Item = Result<Event<'a>, Error>;

    fn next(&mut self) -> Option<Self::Item> {
        (!self.0.is_empty()).then(|| self.event())
    }
}

fn integer(argument: u64) -> Result<i64, Error> {
    i64::try_from(argument).ok().context(MalformedSnafu {
        message: "integer is outside i64",
    })
}

fn length(argument: u64) -> Result<usize, Error> {
    usize::try_from(argument).ok().context(MalformedSnafu {
        message: "length does not fit in memory",
    })
}

fn malformed<T>(message: &'static str) -> Result<T, Error> {
    MalformedSnafu { message }.fail()
}

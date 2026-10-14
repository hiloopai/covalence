use std::{
    collections::BTreeMap,
    io::{self, Write},
    mem::take,
};

use covalence_lib_cid::{Bdasl, Cid};
use covalence_lib_data::Bytes;
use covalence_lib_error::snafu::{OptionExt, ensure};

use crate::{Error, MAX_DEPTH, MalformedSnafu, TooDeepSnafu, Value, drisl};

/// One step of a value, in the order [`Value::encode`] writes it.
///
/// A list's items follow its event. A map's keys and values follow its event,
/// alternating, each key a [`Event::String`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Event<'a> {
    /// `null`.
    Null,
    /// `true` or `false`.
    Bool(bool),
    /// An integer.
    Integer(i64),
    /// A byte string.
    Bytes(&'a [u8]),
    /// A string, or a map key.
    String(&'a str),
    /// The next n values are items.
    List(usize),
    /// The next 2n events are keys and values.
    Map(usize),
    /// A link.
    Link(Cid<Bdasl>),
}

impl Event<'_> {
    /// This event's DRISL head, then its bytes, string or CID payload.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        self.encode_to(&mut out)
            .expect("writing to a Vec cannot fail");
        out
    }

    /// Write this event's DRISL head, then its payload, to `out`.
    pub fn encode_to(&self, mut out: impl Write) -> io::Result<()> {
        drisl::write(&mut out, *self)
    }
}

impl Value {
    /// This value's events. Depth is not checked; [`Value::encode`] checks it.
    pub fn events(&self) -> Events<'_> {
        Events {
            stack: vec![Node::Value(self)],
        }
    }

    /// The links in this value, in the order [`Value::encode`] writes them.
    pub fn links(&self) -> Links<Events<'_>> {
        Links::new(self.events())
    }
}

enum Node<'a> {
    Key(&'a str),
    Value(&'a Value),
}

/// Iterator returned by [`Value::events`].
///
/// Each map's entries are sorted when the map is reached, since DRISL key
/// order (length first) is not `BTreeMap` order.
pub struct Events<'a> {
    stack: Vec<Node<'a>>,
}

impl<'a> Events<'a> {
    fn node(&mut self) -> Option<Node<'a>> {
        let node = self.stack.pop()?;
        match node {
            Node::Value(Value::List(items)) => {
                self.stack.extend(items.iter().rev().map(Node::Value))
            }
            Node::Value(Value::Map(map)) => {
                let mut entries: Vec<_> = map.iter().collect();
                entries.sort_by_key(|(key, _)| (key.len(), *key));
                for (key, value) in entries.into_iter().rev() {
                    self.stack.extend([Node::Value(value), Node::Key(key)]);
                }
            }
            _ => {}
        }
        Some(node)
    }
}

impl<'a> Iterator for Events<'a> {
    type Item = Event<'a>;

    fn next(&mut self) -> Option<Event<'a>> {
        Some(match self.node()? {
            Node::Key(key) => Event::String(key),
            Node::Value(value) => match value {
                Value::Null => Event::Null,
                Value::Bool(value) => Event::Bool(*value),
                Value::Integer(value) => Event::Integer(*value),
                Value::Bytes(value) => Event::Bytes(value),
                Value::String(value) => Event::String(value),
                Value::List(items) => Event::List(items.len()),
                Value::Map(map) => Event::Map(map.len()),
                Value::Link(cid) => Event::Link(*cid),
            },
        })
    }
}

/// The links in a sequence of events, in order, as [`Value::links`] lists them.
///
/// ```
/// use covalence_lib_cid::{Bdasl, Cid, Codec, HashFn};
/// use covalence_lib_dasl::{Event, Links};
/// let cid = Cid::<Bdasl>::of(Codec::Raw, HashFn::Sha256, b"");
/// let events = [Event::List(2), Event::Null, Event::Link(cid)];
/// assert!(Links::new(events).eq([cid]));
/// ```
pub struct Links<I>(I);

impl<I: Iterator> Links<I> {
    /// List the links in `events`.
    pub fn new(events: impl IntoIterator<IntoIter = I>) -> Links<I> {
        Links(events.into_iter())
    }
}

impl<'a, I: Iterator<Item = Event<'a>>> Iterator for Links<I> {
    type Item = Cid<Bdasl>;

    fn next(&mut self) -> Option<Cid<Bdasl>> {
        self.0.find_map(|event| match event {
            Event::Link(cid) => Some(cid),
            _ => None,
        })
    }
}

enum Frame {
    List(usize, Vec<Value>),
    Map(usize, Vec<(String, Value)>, Option<String>),
}

/// Builds a value from events pushed one at a time, as [`Value::decode`] does.
///
/// Rejects map keys that are not strings or not in DRISL order, nesting past
/// [`MAX_DEPTH`], and too few or too many events.
///
/// ```
/// use covalence_lib_dasl::{Builder, Event, Value};
/// let mut builder = Builder::default();
/// for event in [Event::List(2), Event::Null, Event::Integer(1)] {
///     builder.push(event)?;
/// }
/// assert_eq!(builder.finish()?, Value::List(vec![Value::Null, Value::Integer(1)]));
/// # Ok::<(), covalence_lib_dasl::Error>(())
/// ```
#[derive(Default)]
pub struct Builder {
    stack: Vec<Frame>,
    root: Option<Value>,
}

impl Builder {
    /// Add the next event.
    pub fn push(&mut self, event: Event<'_>) -> Result<(), Error> {
        ensure!(
            self.root.is_none(),
            MalformedSnafu {
                message: "more than one value"
            }
        );
        if let Some(Frame::Map(_, entries, key @ None)) = self.stack.last_mut() {
            let Event::String(next) = event else {
                return MalformedSnafu {
                    message: "map key is not a string",
                }
                .fail();
            };
            ensure!(
                entries
                    .last()
                    .is_none_or(|(last, _)| (last.len(), last.as_str()) < (next.len(), next)),
                MalformedSnafu {
                    message: "map keys are not in DRISL order"
                }
            );
            *key = Some(next.to_owned());
            return Ok(());
        }
        let value = match event {
            Event::Null => Value::Null,
            Event::Bool(value) => Value::Bool(value),
            Event::Integer(value) => Value::Integer(value),
            Event::Bytes(value) => Value::Bytes(Bytes::copy_from_slice(value)),
            Event::String(value) => Value::String(value.to_owned()),
            Event::Link(cid) => Value::Link(cid),
            Event::List(_) | Event::Map(_) if self.stack.len() >= MAX_DEPTH => {
                return TooDeepSnafu.fail();
            }
            Event::List(0) => Value::List(Vec::new()),
            Event::Map(0) => Value::Map(BTreeMap::new()),
            Event::List(len) => {
                self.stack.push(Frame::List(len, Vec::new()));
                return Ok(());
            }
            Event::Map(len) => {
                self.stack.push(Frame::Map(len, Vec::new(), None));
                return Ok(());
            }
        };
        self.complete(value);
        Ok(())
    }

    /// The value, once every event has been pushed.
    pub fn finish(self) -> Result<Value, Error> {
        self.root.context(MalformedSnafu {
            message: "value is incomplete",
        })
    }

    fn complete(&mut self, mut value: Value) {
        while let Some(frame) = self.stack.last_mut() {
            value = match frame {
                Frame::List(len, items) => {
                    items.push(value);
                    if items.len() < *len {
                        return;
                    }
                    Value::List(take(items))
                }
                Frame::Map(len, entries, key) => {
                    entries.push((key.take().expect("a key precedes each value"), value));
                    if entries.len() < *len {
                        return;
                    }
                    Value::Map(take(entries).into_iter().collect())
                }
            };
            self.stack.pop();
        }
        self.root = Some(value);
    }
}

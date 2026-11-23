use std::cmp::Ordering;

use covalence_lib_cid::{Bdasl, Blake3, Cid, Codec, Fact, Format};
use covalence_lib_dasl::Value;
use covalence_lib_data::Bytes;
use covalence_lib_error::snafu::ensure;

use crate::{ChildLinkSnafu, DuplicateNameSnafu, Error, MalformedSnafu, NameSnafu};

const TAG: &str = "covalence.fs.dir.v1";

/// The codec required for leaf links in one directory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LeafMode {
    /// Links name raw file bytes.
    Bytes = 0,
    /// Links name canonical DRISL values, including encoded byte strings.
    Dasl = 1,
}

impl LeafMode {
    fn codec(self) -> Codec {
        match self {
            Self::Bytes => Codec::Raw,
            Self::Dasl => Codec::Drisl,
        }
    }
}

/// The declared role of a child link.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EntryKind {
    /// A leaf whose codec is set by the enclosing directory's mode.
    Leaf = 0,
    /// A directory in this crate's versioned format.
    Directory = 1,
}

/// A directory child described by a byte name, kind and portable CID.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    /// The unmodified bytes of one filename component.
    pub name: Bytes,
    /// The declared role of the link.
    pub kind: EntryKind,
    /// The child's CID, whose format is checked by [`Directory::new`].
    pub cid: Cid<Bdasl>,
}

/// One immutable directory with sorted, unique child names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Directory {
    mode: LeafMode,
    children: Vec<Entry>,
}

impl Directory {
    /// Sort and validate the shallow entries of a directory.
    ///
    /// Names are compared lexicographically as unsigned bytes. Empty names,
    /// `.` and `..`, slashes, NUL and repeated names are rejected.
    /// Directory links require DRISL and BLAKE3. Leaf links require the exact
    /// mode codec with either supported BDASL hash. Target bytes are unseen.
    pub fn new(mode: LeafMode, mut children: Vec<Entry>) -> Result<Self, Error> {
        children.sort_unstable_by(|left, right| left.name.cmp(&right.name));
        Self::from_sorted(mode, children)
    }

    /// Return the mode of this directory's leaf links.
    pub fn mode(&self) -> LeafMode {
        self.mode
    }

    /// Return the immediate children in bytewise name order.
    pub fn children(&self) -> &[Entry] {
        &self.children
    }

    /// Look up one immediate child by its exact byte name.
    pub fn child(&self, name: &[u8]) -> Option<&Entry> {
        let index = self
            .children
            .binary_search_by(|entry| entry.name.as_ref().cmp(name))
            .ok()?;
        Some(&self.children[index])
    }

    /// Compute the directory CID without a store or access to any children.
    pub fn cid(&self) -> Result<Cid<Bdasl>, Error> {
        Ok(*self.block()?.claim())
    }

    fn from_sorted(mode: LeafMode, children: Vec<Entry>) -> Result<Self, Error> {
        let mut previous: Option<&Bytes> = None;
        for Entry { name, kind, cid } in &children {
            ensure!(
                !matches!(name.as_ref(), b"" | b"." | b"..")
                    && !name.contains(&b'/')
                    && !name.contains(&0),
                NameSnafu { name: name.clone() }
            );
            if let Some(previous) = previous {
                match previous.cmp(name) {
                    Ordering::Equal => return DuplicateNameSnafu { name: name.clone() }.fail(),
                    Ordering::Greater => return malformed("children are not sorted by name"),
                    Ordering::Less => {}
                }
            }
            previous = Some(name);
            let compatible = match kind {
                EntryKind::Leaf => cid.codec() == mode.codec().code(),
                EntryKind::Directory => cid.format() == Format::DRISL_BLAKE3,
            };
            ensure!(
                compatible,
                ChildLinkSnafu {
                    name: name.clone(),
                    kind: *kind,
                    cid: *cid,
                    mode
                }
            );
        }
        Ok(Self { mode, children })
    }

    pub(crate) fn block(&self) -> Result<Fact<Cid<Bdasl>>, Error> {
        let children = self.children.iter().map(|entry| {
            Value::List(vec![
                Value::Bytes(entry.name.clone()),
                Value::Integer(entry.kind as i64),
                Value::Link(entry.cid),
            ])
        });
        let bytes = Value::List(vec![
            Value::String(TAG.into()),
            Value::Integer(self.mode as i64),
            Value::List(children.collect()),
        ])
        .encode()?;
        Ok(Fact::<Blake3>::of(bytes.into())
            .widen()
            .with_codec(Codec::Drisl.code())?)
    }

    pub(crate) fn decode(bytes: &[u8]) -> Result<Self, Error> {
        let Value::List(fields) = Value::decode(bytes)? else {
            return malformed("expected a directory tuple");
        };
        let Ok(
            [
                Value::String(tag),
                Value::Integer(mode),
                Value::List(children),
            ],
        ) = <[Value; 3]>::try_from(fields)
        else {
            return malformed("expected a directory tuple");
        };
        if tag != TAG {
            return malformed("unknown directory tag");
        }
        let mode = match mode {
            0 => LeafMode::Bytes,
            1 => LeafMode::Dasl,
            _ => return malformed("unknown leaf mode"),
        };

        let mut entries = Vec::with_capacity(children.len());
        for child in children {
            let Value::List(fields) = child else {
                return malformed("expected a child tuple");
            };
            let Ok([Value::Bytes(name), Value::Integer(kind), Value::Link(cid)]) =
                <[Value; 3]>::try_from(fields)
            else {
                return malformed("expected a child tuple");
            };
            let kind = match kind {
                0 => EntryKind::Leaf,
                1 => EntryKind::Directory,
                _ => return malformed("unknown child kind"),
            };
            entries.push(Entry { name, kind, cid });
        }
        Self::from_sorted(mode, entries)
    }
}

fn malformed<T>(message: &'static str) -> Result<T, Error> {
    MalformedSnafu { message }.fail()
}

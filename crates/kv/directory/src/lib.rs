//! A key-value store in a filesystem directory.
//!
//! [`DirectoryKv`] maps UTF-8 keys to files below its root. Keys consist of
//! non-empty `/`-separated segments other than `.` and `..`, without `\`.
//! Capability-based filesystem access prevents symlinks from escaping the root.
//! A missing root is created only on the first write. Writes replace a value
//! atomically after its complete contents have been read into a temporary file
//! in the destination directory; a failed read leaves the previous value intact.
//! Create-only writes atomically link the completed file into place, preserving
//! an existing value and its metadata.
//!
//! `flush` syncs regular files and directories without following symlinks.
//! Streaming methods use bounded memory. Async methods perform the same blocking
//! filesystem operations and return ready futures.
#![deny(missing_docs)]

use std::{
    collections::BTreeMap,
    io::{self, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
};

use cap_std::{
    ambient_authority,
    fs::{Dir, File, OpenOptions},
};
use covalence_kv_trait::{Error, Kv, KvFuture, ready};
use covalence_lib_dasl::Value;
use covalence_lib_data::Bytes;
use once_cell::sync::OnceCell;

/// A key-value store whose keys name files below a directory.
///
/// An existing directory is opened when the store is constructed; a missing
/// root is opened when it appears or a write creates it. Initialization is
/// shared between callers; failed attempts can be retried. The handle is retained.
/// Reads from a missing root return no value. All paths
/// after opening it are resolved relative to its handle, including symlinks.
/// [`Kv::info`] reports `name` as `directory` and the absolute `root` path.
#[derive(Debug)]
pub struct DirectoryKv {
    root: PathBuf,
    directory: OnceCell<OpenedDirectory>,
}

impl DirectoryKv {
    /// A store rooted at `root`, resolved against the current directory.
    ///
    /// Fails if an existing root cannot be opened as a directory. A missing
    /// root is left absent until a write creates it.
    pub fn new(root: impl AsRef<Path>) -> Result<Self, Error> {
        let store = Self {
            root: std::path::absolute(root).map_err(backend)?,
            directory: OnceCell::new(),
        };
        store.directory(false)?;
        Ok(store)
    }

    fn directory(&self, create: bool) -> Result<Option<&OpenedDirectory>, Error> {
        match self
            .directory
            .get_or_try_init(|| OpenedDirectory::open(&self.root, create))
        {
            Ok(directory) => Ok(Some(directory)),
            Err(error) if !create && error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(backend(error)),
        }
    }

    fn write_from(
        &self,
        key: &[u8],
        reader: &mut dyn Read,
        len: u64,
        replace: bool,
    ) -> Result<bool, Error> {
        let key = checked_key(key)?;
        let (parent, name) = key.rsplit_once('/').unwrap_or(("", key));
        let root = &self.directory(true)?.expect("write opens root").dir;
        let parent = if parent.is_empty() {
            None
        } else {
            root.create_dir_all(parent).map_err(backend)?;
            Some(root.open_dir(parent).map_err(backend)?)
        };
        let parent = parent.as_ref().unwrap_or(root);
        let mut pending = Pending::new(parent).map_err(backend)?;
        let written = io::copy(&mut reader.take(len), &mut pending.file)
            .map_err(|source| Error::Io { source })?;
        if written != len {
            return Err(Error::Io {
                source: io::ErrorKind::UnexpectedEof.into(),
            });
        }
        if replace {
            pending.commit(name).map_err(backend)?;
            Ok(true)
        } else {
            pending.commit_new(name).map_err(backend)
        }
    }

    fn open(&self, key: &[u8]) -> Result<Option<File>, Error> {
        let key = checked_key(key)?;
        let Some(directory) = self.directory(false)? else {
            return Ok(None);
        };
        match directory.dir.open(key) {
            Ok(file) if file.metadata().map_err(backend)?.is_file() => Ok(Some(file)),
            Ok(_) => Err(backend(io::Error::new(
                io::ErrorKind::InvalidInput,
                "key is not a regular file",
            ))),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(backend(error)),
        }
    }
}

/// The retained root capability and parents whose new entries need syncing.
#[derive(Debug)]
struct OpenedDirectory {
    dir: Dir,
    created_parents: Vec<Dir>,
}

impl OpenedDirectory {
    fn open(root: &Path, create: bool) -> io::Result<Self> {
        let mut parents = Vec::new();
        if create {
            let mut path = root;
            while !path.try_exists()? {
                let Some(parent) = path.parent() else { break };
                parents.push(parent.to_owned());
                path = parent;
            }
            std::fs::create_dir_all(root)?;
        }
        Ok(Self {
            dir: Dir::open_ambient_dir(root, ambient_authority())?,
            created_parents: parents
                .into_iter()
                .map(|parent| Dir::open_ambient_dir(parent, ambient_authority()))
                .collect::<io::Result<_>>()?,
        })
    }
}

fn backend(source: io::Error) -> Error {
    Error::Backend {
        source: Box::new(source),
    }
}

fn checked_key(key: &[u8]) -> Result<&str, Error> {
    let key = std::str::from_utf8(key).map_err(|_| {
        backend(io::Error::new(
            io::ErrorKind::InvalidInput,
            "key is not UTF-8",
        ))
    })?;
    if key.contains(['\\', '\0']) || key.split('/').any(|part| matches!(part, "" | "." | "..")) {
        return Err(backend(io::Error::new(
            io::ErrorKind::InvalidInput,
            "key must contain non-empty relative path segments",
        )));
    }
    Ok(key)
}

/// A temporary file removed unless its rename committed the value.
struct Pending<'a> {
    dir: &'a Dir,
    name: String,
    file: File,
    committed: bool,
}

impl<'a> Pending<'a> {
    fn new(dir: &'a Dir) -> io::Result<Self> {
        loop {
            let name = format!(
                ".covalence-{:032x}.tmp",
                covalence_lib_rand::random::<u128>()
            );
            match dir.open_with(&name, OpenOptions::new().write(true).create_new(true)) {
                Ok(file) => {
                    return Ok(Self {
                        dir,
                        name,
                        file,
                        committed: false,
                    });
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            }
        }
    }

    fn commit_new(self, name: &str) -> io::Result<bool> {
        self.file.sync_all()?;
        match self.dir.hard_link(&self.name, self.dir, name) {
            Ok(()) => Ok(true),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Ok(false),
            Err(error) => Err(error),
        }
    }

    fn commit(mut self, name: &str) -> io::Result<()> {
        self.file.sync_all()?;
        self.dir.rename(&self.name, self.dir, name)?;
        self.committed = true;
        Ok(())
    }
}

impl Drop for Pending<'_> {
    fn drop(&mut self) {
        if !self.committed {
            let _ = self.dir.remove_file(&self.name);
        }
    }
}

/// Sync descendants without following symbolic links, then the directory.
fn sync_tree(dir: &Dir) -> io::Result<()> {
    for entry in dir.entries()? {
        let entry = entry?;
        let kind = entry.file_type()?;
        if kind.is_dir() {
            sync_tree(&entry.open_dir()?)?;
        } else if kind.is_file() {
            entry.open()?.sync_all()?;
        }
    }
    sync_dir(dir)
}

/// Reopen the directory for I/O before syncing its metadata.
fn sync_dir(dir: &Dir) -> io::Result<()> {
    // On Linux a capability directory handle uses O_PATH, which cannot be
    // fsynced. Opening `.` for reading creates an I/O handle for the same
    // directory, even if its original filesystem path has been replaced.
    dir.open(".")?.sync_all()
}

impl Kv for DirectoryKv {
    fn get(&self, key: &[u8]) -> Result<Option<Bytes>, Error> {
        let Some(mut file) = self.open(key)? else {
            return Ok(None);
        };
        let mut value = Vec::new();
        file.read_to_end(&mut value).map_err(backend)?;
        Ok(Some(value.into()))
    }

    fn size(&self, key: &[u8]) -> Result<Option<u64>, Error> {
        self.open(key)?
            .map(|file| {
                file.metadata()
                    .map(|metadata| metadata.len())
                    .map_err(backend)
            })
            .transpose()
    }

    fn get_range(&self, key: &[u8], start: u64, len: u64) -> Result<Option<Bytes>, Error> {
        let Some(mut file) = self.open(key)? else {
            return Ok(None);
        };
        let size = file.metadata().map_err(backend)?.len();
        file.seek(SeekFrom::Start(start.min(size)))
            .map_err(backend)?;
        let mut value = Vec::new();
        file.take(len).read_to_end(&mut value).map_err(backend)?;
        Ok(Some(value.into()))
    }

    fn put(&self, key: &[u8], value: Bytes) -> Result<(), Error> {
        self.put_from(key, &mut value.as_ref(), value.len() as u64)
    }

    fn put_from(&self, key: &[u8], reader: &mut dyn Read, len: u64) -> Result<(), Error> {
        self.write_from(key, reader, len, true).map(|_| ())
    }

    fn put_new(&self, key: &[u8], value: Bytes) -> Result<bool, Error> {
        self.put_new_from(key, &mut value.as_ref(), value.len() as u64)
    }

    fn put_new_from(&self, key: &[u8], reader: &mut dyn Read, len: u64) -> Result<bool, Error> {
        self.write_from(key, reader, len, false)
    }

    fn get_to(&self, key: &[u8], writer: &mut dyn Write) -> Result<Option<u64>, Error> {
        let Some(mut file) = self.open(key)? else {
            return Ok(None);
        };
        io::copy(&mut file, writer)
            .map(Some)
            .map_err(|source| Error::Io { source })
    }

    fn exists(&self, key: &[u8]) -> Result<bool, Error> {
        Ok(self.open(key)?.is_some())
    }
    fn aget<'a>(&'a self, key: &'a [u8]) -> KvFuture<'a, Option<Bytes>> {
        ready(self.get(key))
    }
    fn asize<'a>(&'a self, key: &'a [u8]) -> KvFuture<'a, Option<u64>> {
        ready(self.size(key))
    }
    fn aget_range<'a>(
        &'a self,
        key: &'a [u8],
        start: u64,
        len: u64,
    ) -> KvFuture<'a, Option<Bytes>> {
        ready(self.get_range(key, start, len))
    }
    fn aput<'a>(&'a self, key: &'a [u8], value: Bytes) -> KvFuture<'a, ()> {
        ready(self.put(key, value))
    }
    fn aput_new<'a>(&'a self, key: &'a [u8], value: Bytes) -> KvFuture<'a, bool> {
        ready(self.put_new(key, value))
    }
    fn aexists<'a>(&'a self, key: &'a [u8]) -> KvFuture<'a, bool> {
        ready(self.exists(key))
    }

    fn flush(&self) -> Result<(), Error> {
        if let Some(directory) = self.directory(false)? {
            sync_tree(&directory.dir).map_err(backend)?;
            for parent in &directory.created_parents {
                sync_dir(parent).map_err(backend)?;
            }
        }
        Ok(())
    }

    fn info(&self) -> Result<Value, Error> {
        Ok(Value::Map(BTreeMap::from([
            ("name".into(), Value::String("directory".into())),
            (
                "root".into(),
                Value::String(self.root.to_string_lossy().as_ref().into()),
            ),
        ])))
    }
}

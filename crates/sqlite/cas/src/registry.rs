//! The one SQLite VFS that every database of this crate is opened through.
//!
//! A VFS registration lasts for the life of the process, so there is one, made
//! when the first connection is opened. It finds the files of open databases
//! by names that are made here and never reused.

use std::{
    collections::BTreeMap,
    fmt::Display,
    io,
    sync::{Arc, Mutex, MutexGuard, PoisonError},
};

use covalence_lib_sqlite::{
    Connection,
    vfs::{self, File, Registered},
};

/// The name the VFS is registered under, unless something else has taken it.
const NAME: &str = "covalence-cas";

/// The files of the open databases by name, and the number of names made.
struct Files {
    made: u64,
    open: BTreeMap<String, Arc<dyn File>>,
}

static FILES: Mutex<Files> = Mutex::new(Files {
    made: 0,
    open: BTreeMap::new(),
});

static VFS: Mutex<Option<Registered>> = Mutex::new(None);

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A database file's place among the open files, until it is dropped.
///
/// Connections that are open keep their file; dropping the ticket only stops
/// new ones.
pub(crate) struct Ticket {
    name: String,
}

impl Ticket {
    /// A place for `file` under a new name, which ends with `label`.
    pub(crate) fn new(file: Arc<dyn File>, label: &dyn Display) -> Self {
        let mut files = lock(&FILES);
        files.made += 1;
        let name = format!("{}-{label}", files.made);
        files.open.insert(name.clone(), file);
        Self { name }
    }

    /// A new read-only connection to the file.
    pub(crate) fn connect(&self) -> Result<Connection, vfs::Error> {
        registered()?.open(&self.name)
    }
}

impl Drop for Ticket {
    fn drop(&mut self) {
        lock(&FILES).open.remove(&self.name);
    }
}

/// The VFS, registered on first use.
fn registered() -> Result<Registered, vfs::Error> {
    let mut slot = lock(&VFS);
    if let Some(registered) = *slot {
        return Ok(registered);
    }
    // Another copy of this crate may share this process's SQLite and hold the
    // name: take the first numbered name that is free.
    let mut name = NAME.to_owned();
    let mut taken = 0;
    let registered = loop {
        match vfs::register(&name, find) {
            Err(vfs::Error::Taken { .. }) if taken < 64 => {
                taken += 1;
                name = format!("{NAME}-{taken}");
            }
            registered => break registered?,
        }
    };
    *slot = Some(registered);
    Ok(registered)
}

/// The open file named `name`, for SQLite.
fn find(name: &str) -> io::Result<Option<Box<dyn File>>> {
    let file = lock(&FILES).open.get(name).cloned();
    Ok(file.map(|file| Box::new(file) as Box<dyn File>))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Empty;

    impl File for Empty {
        fn size(&self) -> io::Result<u64> {
            Ok(0)
        }
        fn read_at(&self, _offset: u64, buf: &mut [u8]) -> io::Result<usize> {
            Ok(buf.len())
        }
    }

    #[test]
    fn tickets_have_their_own_names_and_close_their_place_when_dropped() {
        let first = Ticket::new(Arc::new(Empty), &"label");
        let second = Ticket::new(Arc::new(Empty), &"label");
        assert_ne!(first.name, second.name);
        assert!(first.name.ends_with("-label"));
        assert!(find(&first.name).unwrap().is_some());
        assert!(find("label").unwrap().is_none());

        // An empty file is an empty database, and it cannot be written.
        let connection = first.connect().unwrap();
        let tables: i64 = connection
            .query_row("SELECT count(*) FROM sqlite_schema", [], |row| row.get(0))
            .unwrap();
        assert_eq!(tables, 0);
        assert!(connection.execute("CREATE TABLE t (x)", []).is_err());

        let name = first.name.clone();
        drop(first);
        assert!(find(&name).unwrap().is_none());
        assert!(find(&second.name).unwrap().is_some());
        // The open connection keeps its file; a new ticket gets a new name.
        connection
            .query_row("SELECT count(*) FROM sqlite_schema", [], |_| Ok(()))
            .unwrap();
        assert_ne!(Ticket::new(Arc::new(Empty), &"label").name, name);
    }
}

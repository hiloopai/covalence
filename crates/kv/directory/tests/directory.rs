use std::io::{self, Read};

use covalence_kv_directory::DirectoryKv;
use covalence_kv_trait::{Error, Kv};
use pollster::block_on;

#[test]
fn lazy_root_and_streams() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().join("store");
    let store = DirectoryKv::new(&root).unwrap().into_dyn();
    assert!(!root.exists());
    assert_eq!(store.get(b"blocks/hello").unwrap(), None);
    assert!(!store.exists(b"blocks/hello").unwrap());
    assert!(!root.exists());
    let mut reader = &b"hello trailing"[..];
    store.put_from(b"blocks/hello", &mut reader, 5).unwrap();
    assert_eq!(reader, b" trailing");
    assert_eq!(store.size(b"blocks/hello").unwrap(), Some(5));
    assert_eq!(
        store
            .get_range(b"blocks/hello", 1, u64::MAX)
            .unwrap()
            .unwrap(),
        "ello"
    );
    assert_eq!(
        store
            .get_range(b"blocks/hello", u64::MAX, 4)
            .unwrap()
            .unwrap(),
        ""
    );
    assert_eq!(block_on(store.asize(b"blocks/hello")).unwrap(), Some(5));
    assert!(block_on(store.aexists(b"blocks/hello")).unwrap());
    let mut output = Vec::new();
    assert_eq!(store.get_to(b"blocks/hello", &mut output).unwrap(), Some(5));
    assert_eq!(output, b"hello");
    block_on(store.aput(b"blocks/hello", "world".into())).unwrap();
    assert_eq!(std::fs::read(root.join("blocks/hello")).unwrap(), b"world");
}

struct Failing(bool);

impl Read for Failing {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        if self.0 {
            return Err(io::Error::other("source failed"));
        }
        self.0 = true;
        bytes[0] = 1;
        Ok(1)
    }
}

#[test]
fn failed_writes_keep_previous_value_and_remove_temporary_files() {
    let root = tempfile::tempdir().unwrap();
    let store = DirectoryKv::new(root.path()).unwrap();
    store.put(b"key", "old".into()).unwrap();
    for result in [
        store.put_from(b"key", &mut &b"short"[..], 9),
        store.put_from(b"key", &mut Failing(false), 9),
    ] {
        assert!(matches!(result, Err(Error::Io { .. })));
        assert_eq!(store.get(b"key").unwrap().unwrap(), "old");
    }
    assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 1);
}

#[test]
fn path_segments_cannot_change_authority() {
    let root = tempfile::tempdir().unwrap();
    let store = DirectoryKv::new(root.path().join("store")).unwrap();
    for key in [
        b"".as_slice(),
        b"/key",
        b"a//b",
        b"..",
        b"a/../key",
        b"a/./key",
        b"a\\key",
        b"a\0key",
        b"key/",
        b"\xff",
    ] {
        assert!(store.get(key).is_err(), "{key:?}");
        assert!(store.put(key, "value".into()).is_err(), "{key:?}");
        assert!(store.exists(key).is_err(), "{key:?}");
    }
    assert!(!root.path().join("store").exists());
}

#[cfg(unix)]
#[test]
fn symlinks_cannot_escape_and_replacing_root_keeps_open_authority() {
    use std::os::unix::fs::symlink;
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().join("store");
    let outside = temporary.path().join("outside");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("secret"), "secret").unwrap();
    symlink(&outside, root.join("escape")).unwrap();
    symlink(outside.join("secret"), root.join("file")).unwrap();
    let store = DirectoryKv::new(&root).unwrap();
    assert!(store.get(b"escape/secret").is_err());
    assert!(store.get(b"file").is_err());
    assert!(store.put(b"escape/new", "new".into()).is_err());
    assert!(!outside.join("new").exists());
    // Create-only writes preserve the symlink; replacement does not follow it.
    assert!(!store.put_new(b"file", "replacement".into()).unwrap());
    store.put(b"file", "replacement".into()).unwrap();
    assert_eq!(std::fs::read(outside.join("secret")).unwrap(), b"secret");
    let moved = temporary.path().join("moved");
    std::fs::rename(&root, &moved).unwrap();
    symlink(&outside, &root).unwrap();
    store.put(b"retained", "value".into()).unwrap();
    assert!(moved.join("retained").is_file());
    assert!(!outside.join("retained").exists());
    store.flush().unwrap();
}

#[test]
fn flush_persists_a_nested_lazy_root_without_following_symlinks() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().join("new/parents/store");
    let store = DirectoryKv::new(&root).unwrap().into_dyn();
    store.flush().unwrap();
    assert!(!root.exists());
    store.put(b"a/b/c", "durable".into()).unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink("/does/not/exist", root.join("ignored-link")).unwrap();
    store.flush().unwrap();
    let reopened = DirectoryKv::new(&root).unwrap();
    assert_eq!(reopened.get(b"a/b/c").unwrap().unwrap(), "durable");
}

#[test]
fn flush_an_existing_empty_root_without_creating_entries() {
    let root = tempfile::tempdir().unwrap();
    let store = DirectoryKv::new(root.path()).unwrap().into_dyn();
    store.flush().unwrap();
    assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
}

#[test]
fn failed_initialization_can_be_retried() {
    let temporary = tempfile::tempdir().unwrap();
    let parent = temporary.path().join("parent");
    let root = parent.join("store");
    let store = DirectoryKv::new(&root).unwrap();
    std::fs::write(&parent, "blocks creation").unwrap();
    assert!(store.put(b"key", "value".into()).is_err());
    std::fs::remove_file(parent).unwrap();
    store.put(b"key", "value".into()).unwrap();
    block_on(store.aflush()).unwrap();
    assert_eq!(store.get(b"key").unwrap().unwrap(), "value");
}

#[test]
fn concurrent_first_writes_share_one_retained_root() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().join("new/parents/store");
    let store = DirectoryKv::new(&root).unwrap();
    let ready = std::sync::Barrier::new(8);
    std::thread::scope(|threads| {
        for index in 0..8 {
            let (store, ready) = (&store, &ready);
            threads.spawn(move || {
                ready.wait();
                store
                    .put(index.to_string().as_bytes(), "value".into())
                    .unwrap();
            });
        }
    });
    store.flush().unwrap();
    assert_eq!(std::fs::read_dir(&root).unwrap().count(), 8);
    for index in 0..8 {
        assert_eq!(
            store.get(index.to_string().as_bytes()).unwrap().unwrap(),
            "value"
        );
    }
}

#[test]
fn create_only_writes_preserve_existing_bytes_and_metadata() {
    let root = tempfile::tempdir().unwrap();
    let store = DirectoryKv::new(root.path()).unwrap().into_dyn();
    assert!(store.put_new(b"key", "original".into()).unwrap());
    let path = root.path().join("key");
    let old_time = std::time::UNIX_EPOCH + std::time::Duration::from_secs(123);
    std::fs::File::options()
        .write(true)
        .open(&path)
        .unwrap()
        .set_modified(old_time)
        .unwrap();
    let modified = path.metadata().unwrap().modified().unwrap();
    assert!(!store.put_new(b"key", "replacement".into()).unwrap());
    assert!(!block_on(store.aput_new(b"key", "async replacement".into())).unwrap());
    assert!(matches!(
        store.put_new_from(b"key", &mut Failing(false), 9),
        Err(Error::Io { .. })
    ));
    assert_eq!(store.get(b"key").unwrap().unwrap(), "original");
    assert_eq!(path.metadata().unwrap().modified().unwrap(), modified);
    assert!(matches!(
        store.put_new_from(b"short", &mut &b"a"[..], 2),
        Err(Error::Io { .. })
    ));
    assert!(!store.exists(b"short").unwrap());
    assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 1);
}

#[test]
fn concurrent_create_only_writes_have_one_complete_winner() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().join("store");
    let store = DirectoryKv::new(&root).unwrap();
    let ready = std::sync::Barrier::new(8);
    let created = std::thread::scope(|threads| {
        let writers: Vec<_> = (0..8)
            .map(|index| {
                let (store, ready) = (&store, &ready);
                threads.spawn(move || {
                    ready.wait();
                    store
                        .put_new(b"winner", format!("value-{index}").into())
                        .unwrap()
                })
            })
            .collect();
        writers
            .into_iter()
            .map(|writer| writer.join().unwrap())
            .filter(|created| *created)
            .count()
    });
    assert_eq!(created, 1);
    let winner = store.get(b"winner").unwrap().unwrap();
    assert!((0..8).any(|index| winner == format!("value-{index}")));
    assert_eq!(std::fs::read_dir(root).unwrap().count(), 1);
    store.flush().unwrap();
}

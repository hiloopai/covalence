//! A process in which something else has registered a VFS under the name this
//! crate uses. It runs alone, so that the name is taken before the crate
//! registers its own VFS.

mod common;

use common::{Store, rows, sample};
use covalence_lib_sqlite::vfs;
use covalence_sqlite_cas::{Database, Options, verify::Verification};

#[test]
fn a_vfs_that_holds_the_usual_name_does_not_stop_databases() {
    let squatter = vfs::register("covalence-cas", |_: &str| Ok(None)).unwrap();
    let second = vfs::register("covalence-cas-1", |_: &str| Ok(None)).unwrap();

    let store = Store::new(sample(10));
    for verification in [Verification::Download, Verification::TrustedRanges] {
        let options = Options::new().verification(verification);
        let database = Database::open(store.clone(), &store.blake3(), options).unwrap();
        let connection = database.connect().unwrap();
        assert_eq!(rows(&connection, "SELECT count(*) FROM items"), ["10"]);
    }
    // The other VFSes keep their names, and none of the databases.
    assert_eq!(
        (squatter.name(), second.name()),
        ("covalence-cas", "covalence-cas-1")
    );
    assert!(squatter.open("1-anything").is_err());
}

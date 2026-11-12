//! The VFS as SQLite sees it: C callbacks over a safe [`Vfs`] and its [`File`]s.
//!
//! This is the only module of the crate that may use `unsafe`.
//!
//! - The io methods are version 1, so SQLite never asks to map pages or share
//!   memory, and no pointer into a file's bytes outlives the read that fills
//!   SQLite's buffer.
//! - Callbacks that run Rust code able to panic (a [`Vfs::open`], a
//!   [`File::read_at`] or a file's drop) catch the panic and return
//!   `SQLITE_IOERR`, so no panic unwinds into C.
//! - What SQLite passes in is trusted to follow its VFS contract. What the
//!   Rust side answers is checked before SQLite sees it.

use std::{
    ffi::{CStr, CString, c_char, c_int, c_void},
    mem, panic, ptr, slice,
    sync::{Mutex, PoisonError},
};

use rusqlite::ffi as sys;

use super::{Error, File, RegisterSnafu, TakenSnafu, Vfs};

/// The longest name of a database file, in bytes.
const MAX_NAME: c_int = 1024;

/// The part of `mxPathname` that SQLite keeps for the suffix of a journal's
/// name: it opens no database whose name comes closer to the limit.
const JOURNAL_SUFFIX: c_int = 8;

/// Serialises the check for a taken name with registration.
static REGISTER: Mutex<()> = Mutex::new(());

/// What a registered VFS keeps, reached through `pAppData`; never freed.
///
/// SQLite's threads share it. `default` is written once, at registration, and
/// only read afterwards, and `vfs` is `Sync`.
struct State {
    /// Opens files by name.
    vfs: Box<dyn Vfs>,
    /// The default VFS when this one was registered: it opens temporary files
    /// and answers for time, sleep and randomness.
    default: *mut sys::sqlite3_vfs,
}

/// An open file as SQLite allocates it: its header, then our handle.
#[repr(C)]
struct Handle {
    /// What SQLite reads: the io methods.
    base: sys::sqlite3_file,
    /// The open file, boxed; null once closed.
    open: *mut Open,
}

// SQLite allocates `szOsFile` bytes aligned to 8.
const _: () = assert!(align_of::<Handle>() <= 8);

/// An open file and the size it had when opened.
struct Open {
    /// The bytes.
    file: Box<dyn File>,
    /// The size read at open, at most `i64::MAX`.
    size: u64,
}

/// Register `vfs` under `name` for the life of the process, unless SQLite
/// already has a VFS of that name.
pub(super) fn register(name: CString, vfs: Box<dyn Vfs>) -> Result<&'static CStr, Error> {
    let _guard = REGISTER.lock().unwrap_or_else(PoisonError::into_inner);
    // SAFETY: `name` is NUL-terminated. Finding a VFS initialises SQLite if
    // needed and only reads its list.
    if !unsafe { sys::sqlite3_vfs_find(name.as_ptr()) }.is_null() {
        let name = name.to_string_lossy().into_owned();
        return TakenSnafu { name }.fail();
    }
    // SAFETY: a null name asks for the default VFS.
    let default = unsafe { sys::sqlite3_vfs_find(ptr::null()) };
    if default.is_null() {
        let code = sys::SQLITE_ERROR;
        return RegisterSnafu { code }.fail();
    }
    // SAFETY: `default` is registered, so it points to a live VFS. It is
    // assumed to stay live for the process, as SQLite's built-in VFSes are
    // statics.
    let default_size = unsafe { (*default).szOsFile };
    let name = name.into_raw();
    let state = Box::into_raw(Box::new(State { vfs, default }));
    let record = Box::into_raw(Box::new(sys::sqlite3_vfs {
        iVersion: 1,
        szOsFile: default_size.max(size_of::<Handle>() as c_int),
        mxPathname: MAX_NAME + JOURNAL_SUFFIX,
        pNext: ptr::null_mut(),
        zName: name,
        pAppData: state.cast(),
        xOpen: Some(x_open),
        xDelete: Some(x_delete),
        xAccess: Some(x_access),
        xFullPathname: Some(x_full_pathname),
        xDlOpen: Some(x_dl_open),
        xDlError: Some(x_dl_error),
        xDlSym: Some(x_dl_sym),
        xDlClose: Some(x_dl_close),
        xRandomness: Some(x_randomness),
        xSleep: Some(x_sleep),
        xCurrentTime: Some(x_current_time),
        xGetLastError: Some(x_get_last_error),
        xCurrentTimeInt64: None,
        xSetSystemCall: None,
        xGetSystemCall: None,
        xNextSystemCall: None,
    }));
    // SAFETY: `record` and everything it points to are live and, once
    // registered, never freed; it does not become the default VFS.
    let code = unsafe { sys::sqlite3_vfs_register(record, 0) };
    if code != sys::SQLITE_OK {
        // SAFETY: registration failed, so SQLite holds none of the three; each
        // came from the matching `into_raw` above and is freed once.
        unsafe {
            drop(Box::from_raw(record));
            drop(Box::from_raw(state));
            drop(CString::from_raw(name));
        }
        return RegisterSnafu { code }.fail();
    }
    // SAFETY: `name` came from `CString::into_raw` and is never freed.
    Ok(unsafe { CStr::from_ptr(name) })
}

/// Run `f`, turning a panic into `SQLITE_IOERR`.
fn guard(f: impl FnOnce() -> c_int) -> c_int {
    panic::catch_unwind(panic::AssertUnwindSafe(f)).unwrap_or(sys::SQLITE_IOERR)
}

/// The state of a VFS registered by [`register`].
///
/// # Safety
///
/// `vfs` must be a VFS that [`register`] made.
unsafe fn state<'a>(vfs: *mut sys::sqlite3_vfs) -> &'a State {
    // SAFETY: the caller's contract; `pAppData` is a leaked `State`. Sharing it
    // across threads is sound, though the raw `default` makes it `!Sync`: no
    // one writes it after registration, and it points to a VFS that is live
    // for the process.
    unsafe { &*(*vfs).pAppData.cast::<State>() }
}

/// The open file behind SQLite's handle.
///
/// # Safety
///
/// `file` must be one [`x_open`] filled, and not yet closed.
unsafe fn open<'a>(file: *mut sys::sqlite3_file) -> &'a Open {
    // SAFETY: the caller's contract; `x_open` wrote a `Handle` with a live `open`.
    unsafe { &*(*file.cast::<Handle>()).open }
}

unsafe extern "C" fn x_open(
    vfs: *mut sys::sqlite3_vfs,
    name: sys::sqlite3_filename,
    file: *mut sys::sqlite3_file,
    flags: c_int,
    out_flags: *mut c_int,
) -> c_int {
    // SAFETY: SQLite passes back the VFS it calls, which `register` made.
    let state = unsafe { state(vfs) };
    // Every temporary file SQLite opens, such as a temp database, a
    // sub-journal or a sort spill, is unnamed. A named open that is not a
    // main database is refused below, so this VFS never touches a local file
    // by name.
    if name.is_null() {
        let default = state.default;
        // SAFETY: `file` has our `szOsFile` bytes, no fewer than the default
        // VFS asks for, so the default VFS can open its own file in place.
        // The other arguments come from SQLite unchanged.
        return unsafe {
            match (*default).xOpen {
                Some(open) => open(default, name, file, flags, out_flags),
                None => sys::SQLITE_CANTOPEN,
            }
        };
    }
    // SAFETY: `file` points to at least a `sqlite3_file`. A null `pMethods`
    // tells SQLite there is nothing to close if the open fails.
    unsafe { (*file).pMethods = ptr::null() };
    guard(|| {
        if flags & sys::SQLITE_OPEN_MAIN_DB == 0 || flags & sys::SQLITE_OPEN_EXCLUSIVE != 0 {
            return sys::SQLITE_CANTOPEN;
        }
        // SAFETY: a non-null name from SQLite is NUL-terminated.
        let Ok(name) = unsafe { CStr::from_ptr(name) }.to_str() else {
            return sys::SQLITE_CANTOPEN;
        };
        let opened = match state.vfs.open(name) {
            Ok(Some(opened)) => opened,
            Ok(None) => return sys::SQLITE_CANTOPEN,
            Err(_) => return sys::SQLITE_IOERR,
        };
        // SQLite's offsets are signed, so a larger file could not be read.
        let size = match opened.size() {
            Ok(size) if i64::try_from(size).is_ok() => size,
            _ => return sys::SQLITE_IOERR,
        };
        let open = Box::into_raw(Box::new(Open { file: opened, size }));
        let methods = &raw const IO_METHODS;
        let base = sys::sqlite3_file { pMethods: methods };
        // SAFETY: `file` has `szOsFile` bytes, no fewer than a `Handle`,
        // aligned to 8, no less than a `Handle` needs (checked above).
        unsafe { file.cast::<Handle>().write(Handle { base, open }) };
        if !out_flags.is_null() {
            let rw = sys::SQLITE_OPEN_READWRITE | sys::SQLITE_OPEN_CREATE;
            // SAFETY: a non-null out pointer from SQLite is writable.
            unsafe { *out_flags = flags & !rw | sys::SQLITE_OPEN_READONLY };
        }
        sys::SQLITE_OK
    })
}

/// Delegates a VFS method to the default VFS, or gives `$none` if it lacks it.
macro_rules! delegate {
    ($name:ident, $method:ident, ($($arg:ident: $ty:ty),*) -> $ret:ty, $none:expr) => {
        unsafe extern "C" fn $name(vfs: *mut sys::sqlite3_vfs, $($arg: $ty),*) -> $ret {
            // SAFETY: `vfs` is ours, so its default VFS is live. SQLite's
            // arguments pass through unchanged, under the same contract.
            unsafe {
                let default = state(vfs).default;
                match (*default).$method {
                    Some(method) => method(default, $($arg),*),
                    None => $none,
                }
            }
        }
    };
}

/// A symbol from a dynamic library, as SQLite types it.
type Symbol = Option<unsafe extern "C" fn(*mut sys::sqlite3_vfs, *mut c_void, *const c_char)>;

delegate!(x_dl_open, xDlOpen, (name: *const c_char) -> *mut c_void, ptr::null_mut());
delegate!(x_dl_error, xDlError, (n: c_int, out: *mut c_char) -> (), ());
delegate!(x_dl_sym, xDlSym, (lib: *mut c_void, name: *const c_char) -> Symbol, None);
delegate!(x_dl_close, xDlClose, (lib: *mut c_void) -> (), ());
delegate!(x_randomness, xRandomness, (n: c_int, out: *mut c_char) -> c_int, 0);
delegate!(x_sleep, xSleep, (micros: c_int) -> c_int, 0);
delegate!(x_current_time, xCurrentTime, (out: *mut f64) -> c_int, sys::SQLITE_ERROR);
delegate!(x_get_last_error, xGetLastError, (n: c_int, out: *mut c_char) -> c_int, 0);

/// No file exists: there is never a journal or WAL to find.
unsafe extern "C" fn x_access(
    _vfs: *mut sys::sqlite3_vfs,
    _name: *const c_char,
    _flags: c_int,
    out: *mut c_int,
) -> c_int {
    // SAFETY: SQLite passes a writable result pointer.
    unsafe { *out = 0 };
    sys::SQLITE_OK
}

/// Nothing to delete, as [`x_access`] says; never the default VFS's files.
///
/// Temporary files are closed and deleted by the default VFS itself.
unsafe extern "C" fn x_delete(
    _vfs: *mut sys::sqlite3_vfs,
    _name: *const c_char,
    _sync_dir: c_int,
) -> c_int {
    sys::SQLITE_IOERR_DELETE_NOENT
}

/// Names are opaque, so the full name is the name, if it fits.
unsafe extern "C" fn x_full_pathname(
    _vfs: *mut sys::sqlite3_vfs,
    name: *const c_char,
    n_out: c_int,
    out: *mut c_char,
) -> c_int {
    // SAFETY: SQLite passes a NUL-terminated name.
    let bytes = unsafe { CStr::from_ptr(name) }.to_bytes_with_nul();
    match usize::try_from(n_out) {
        Ok(room) if bytes.len() <= room => {
            // SAFETY: `out` has `n_out` writable bytes, at least `bytes.len()`,
            // and cannot overlap the name SQLite passed in.
            unsafe { ptr::copy_nonoverlapping(bytes.as_ptr(), out.cast::<u8>(), bytes.len()) };
            sys::SQLITE_OK
        }
        _ => sys::SQLITE_CANTOPEN,
    }
}

/// The io methods of every file this module opens.
static IO_METHODS: sys::sqlite3_io_methods = sys::sqlite3_io_methods {
    iVersion: 1,
    xClose: Some(x_close),
    xRead: Some(x_read),
    xWrite: Some(x_write),
    xTruncate: Some(x_truncate),
    xSync: Some(x_sync),
    xFileSize: Some(x_file_size),
    xLock: Some(x_lock),
    xUnlock: Some(x_unlock),
    xCheckReservedLock: Some(x_check_reserved_lock),
    xFileControl: Some(x_file_control),
    xSectorSize: Some(x_sector_size),
    xDeviceCharacteristics: Some(x_device_characteristics),
    xShmMap: None,
    xShmLock: None,
    xShmBarrier: None,
    xShmUnmap: None,
    xFetch: None,
    xUnfetch: None,
};

unsafe extern "C" fn x_close(file: *mut sys::sqlite3_file) -> c_int {
    // SAFETY: SQLite closes only files `x_open` filled, each once; taking the
    // pointer leaves null behind, so a second close would free nothing.
    let open = unsafe { mem::replace(&mut (*file.cast::<Handle>()).open, ptr::null_mut()) };
    if open.is_null() {
        return sys::SQLITE_OK;
    }
    guard(|| {
        // SAFETY: `open` came from `Box::into_raw` in `x_open` and was just
        // taken out of the handle, so this is its only owner.
        drop(unsafe { Box::from_raw(open) });
        sys::SQLITE_OK
    })
}

unsafe extern "C" fn x_read(
    file: *mut sys::sqlite3_file,
    buffer: *mut c_void,
    amount: c_int,
    offset: sys::sqlite3_int64,
) -> c_int {
    // SAFETY: SQLite reads only open files that `x_open` filled.
    let open = unsafe { open(file) };
    let (Ok(want), Ok(offset)) = (usize::try_from(amount), u64::try_from(offset)) else {
        return sys::SQLITE_IOERR_READ;
    };
    // SAFETY: SQLite's buffer has `amount` writable bytes. Zeroing them first
    // makes them initialised, so they may be viewed as a `&mut [u8]`, and
    // leaves zeros wherever the file has no bytes, as SQLite requires of a
    // short read.
    let buffer = unsafe {
        ptr::write_bytes(buffer.cast::<u8>(), 0, want);
        slice::from_raw_parts_mut(buffer.cast::<u8>(), want)
    };
    // The bytes of the request that exist, by the size read at open.
    let exist = open.size.saturating_sub(offset).min(want as u64) as usize;
    guard(|| {
        if exist > 0 {
            match open.file.read_at(offset, &mut buffer[..exist]) {
                Ok(read) if read == exist => {}
                // An error, or a count other than the bytes that exist: the
                // file lied about its size or changed since it was opened.
                _ => return sys::SQLITE_IOERR_READ,
            }
        }
        if exist < want {
            sys::SQLITE_IOERR_SHORT_READ
        } else {
            sys::SQLITE_OK
        }
    })
}

unsafe extern "C" fn x_write(
    _file: *mut sys::sqlite3_file,
    _buffer: *const c_void,
    _amount: c_int,
    _offset: sys::sqlite3_int64,
) -> c_int {
    sys::SQLITE_READONLY
}

unsafe extern "C" fn x_truncate(_file: *mut sys::sqlite3_file, _size: sys::sqlite3_int64) -> c_int {
    sys::SQLITE_READONLY
}

unsafe extern "C" fn x_sync(_file: *mut sys::sqlite3_file, _flags: c_int) -> c_int {
    sys::SQLITE_OK
}

unsafe extern "C" fn x_file_size(
    file: *mut sys::sqlite3_file,
    out: *mut sys::sqlite3_int64,
) -> c_int {
    // SAFETY: the file is open and `x_open` filled it; `out` is writable. The
    // size fits, as `x_open` checked.
    unsafe { *out = open(file).size as sys::sqlite3_int64 };
    sys::SQLITE_OK
}

unsafe extern "C" fn x_lock(_file: *mut sys::sqlite3_file, _level: c_int) -> c_int {
    sys::SQLITE_OK
}

unsafe extern "C" fn x_unlock(_file: *mut sys::sqlite3_file, _level: c_int) -> c_int {
    sys::SQLITE_OK
}

unsafe extern "C" fn x_check_reserved_lock(
    _file: *mut sys::sqlite3_file,
    out: *mut c_int,
) -> c_int {
    // SAFETY: SQLite passes a writable result pointer.
    unsafe { *out = 0 };
    sys::SQLITE_OK
}

unsafe extern "C" fn x_file_control(
    _file: *mut sys::sqlite3_file,
    _op: c_int,
    _arg: *mut c_void,
) -> c_int {
    sys::SQLITE_NOTFOUND
}

unsafe extern "C" fn x_sector_size(_file: *mut sys::sqlite3_file) -> c_int {
    4096
}

/// Immutable: SQLite takes no locks and looks for no journal or WAL. Reads
/// need not be whole pages, so SQLite may read part of an overflow page.
unsafe extern "C" fn x_device_characteristics(_file: *mut sys::sqlite3_file) -> c_int {
    sys::SQLITE_IOCAP_IMMUTABLE | sys::SQLITE_IOCAP_SUBPAGE_READ
}

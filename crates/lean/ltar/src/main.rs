//! `leantar`, as Lake runs it: the two command lines of `Module.packLtar` and
//! `Module.unpackLtar`.
//!
//! - `leantar -s -C <lib> -C <ir> OUT.ltar MOD.trace (-i <index> FILE)...`
//! - `leantar -C <lib> -C <ir> -x -j -`, with `[{"file": ..., "hash": ...}]`
//!   on stdin.
//! - `leantar --version`.
//!
//! Any other flag fails. Unpacking always writes every file, each archive
//! all or nothing: Lake unpacks only after clearing the module's outputs.

use std::{
    fs, io,
    path::{Path, PathBuf},
    process::ExitCode,
    sync::atomic::{AtomicUsize, Ordering},
    thread,
};

use covalence_lean_ltar::{Archive, Dir, Entry, VERSION, pack, unpack};

fn main() -> ExitCode {
    match run(std::env::args().skip(1).collect()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("leantar: {message}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: Vec<String>) -> Result<(), String> {
    let mut args = args.into_iter().peekable();
    let (mut strip, mut extract, mut json) = (false, false, false);
    let mut bases = Vec::new();
    while let Some(arg) = args.next_if(|arg| arg.starts_with('-') && arg != "-") {
        match arg.as_str() {
            "--version" => {
                println!("leantar {VERSION}");
                return Ok(());
            }
            "-s" => strip = true,
            "-x" => extract = true,
            "-j" => json = true,
            "-C" => bases.push(PathBuf::from(args.next().ok_or("-C needs a directory")?)),
            other => return Err(format!("unsupported option {other}")),
        }
    }
    if bases.is_empty() || bases.len() > 2 {
        return Err("expected one or two -C directories".into());
    }
    let rest: Vec<String> = args.collect();
    if extract {
        if strip || !json || rest != ["-"] {
            return Err("unpacking takes exactly -x -j -".into());
        }
        unpack_all(&bases)
    } else {
        if !strip {
            return Err("packing requires -s: archives never store depHash".into());
        }
        pack_one(&bases, &rest)
    }
}

fn base(bases: &[PathBuf], dir: Dir) -> Result<&Path, String> {
    let index = match dir {
        Dir::Lib => 0,
        Dir::Ir => 1,
    };
    bases
        .get(index)
        .map(PathBuf::as_path)
        .ok_or_else(|| format!("no -C directory for index {index}"))
}

fn pack_one(bases: &[PathBuf], args: &[String]) -> Result<(), String> {
    let [out, trace, files @ ..] = args else {
        return Err("expected OUT.ltar MOD.trace FILE...".into());
    };
    let read = |dir: Dir, path: &str| -> Result<Entry, String> {
        let file = base(bases, dir)?.join(path);
        let bytes = fs::read(&file).map_err(|e| format!("read {}: {e}", file.display()))?;
        Ok(Entry {
            dir,
            path: path.to_owned(),
            bytes,
        })
    };
    let mut entries = Vec::new();
    let mut files = files.iter();
    while let Some(arg) = files.next() {
        let (dir, path) = if arg == "-i" {
            let dir = match files.next().map(String::as_str) {
                Some("0") => Dir::Lib,
                Some("1") => Dir::Ir,
                _ => return Err("-i takes 0 or 1".into()),
            };
            (dir, files.next().ok_or("-i needs a file")?)
        } else if arg.starts_with('-') {
            return Err(format!("unsupported argument {arg}"));
        } else {
            (Dir::Lib, arg)
        };
        entries.push(read(dir, path)?);
    }
    let archive = Archive {
        trace: read(Dir::Lib, trace)?,
        files: entries,
    };
    let bytes = pack(&archive).map_err(|e| format!("pack {out}: {e}"))?;
    let out = Path::new(out);
    let staged = stage(out, &bytes)?;
    staged
        .persist(out)
        .map_err(|e| format!("write {}: {}", out.display(), e.error))?;
    Ok(())
}

/// Writes `bytes` beside `path` under a temporary name.
fn stage(path: &Path, bytes: &[u8]) -> Result<tempfile::TempPath, String> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    fs::create_dir_all(parent).map_err(|e| format!("create {}: {e}", parent.display()))?;
    let mut file = tempfile::NamedTempFile::new_in(parent)
        .map_err(|e| format!("stage {}: {e}", path.display()))?;
    io::Write::write_all(&mut file, bytes).map_err(|e| format!("write {}: {e}", path.display()))?;
    Ok(file.into_temp_path())
}

fn unpack_all(bases: &[PathBuf]) -> Result<(), String> {
    let input = io::read_to_string(io::stdin()).map_err(|e| format!("read stdin: {e}"))?;
    let requests: Vec<serde_json::Value> =
        serde_json::from_str(&input).map_err(|e| format!("parse stdin: {e}"))?;
    let jobs = requests
        .iter()
        .map(|request| {
            let file = request["file"].as_str();
            let hash = request["hash"]
                .as_str()
                .and_then(|hash| u64::from_str_radix(hash, 16).ok());
            let other = request
                .as_object()
                .is_some_and(|o| o.keys().any(|k| k != "file" && k != "hash"));
            match (file, hash, other) {
                (Some(file), Some(hash), false) => Ok((file, hash)),
                _ => Err(format!(
                    "expected {{\"file\": ..., \"hash\": <hex>}}, got {request}"
                )),
            }
        })
        .collect::<Result<Vec<_>, _>>()?;
    let next = AtomicUsize::new(0);
    let work = || -> Vec<String> {
        let mut errors = Vec::new();
        while let Some(&(file, hash)) = jobs.get(next.fetch_add(1, Ordering::Relaxed)) {
            if let Err(error) = unpack_one(bases, file, hash) {
                errors.push(error);
            }
        }
        errors
    };
    // One archive per call is Lake's case: no threads for it.
    let workers = thread::available_parallelism()
        .map_or(1, usize::from)
        .min(jobs.len());
    let errors: Vec<String> = if workers <= 1 {
        work()
    } else {
        thread::scope(|scope| {
            let handles: Vec<_> = (0..workers).map(|_| scope.spawn(work)).collect();
            handles
                .into_iter()
                .flat_map(|handle| {
                    handle
                        .join()
                        .unwrap_or_else(|_| vec!["a worker panicked".into()])
                })
                .collect()
        })
    };
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("\n"))
    }
}

fn unpack_one(bases: &[PathBuf], file: &str, hash: u64) -> Result<(), String> {
    let bytes = fs::read(file).map_err(|e| format!("read {file}: {e}"))?;
    let archive = unpack(&bytes, hash).map_err(|e| format!("unpack {file}: {e}"))?;
    let mut staged = Vec::new();
    for entry in std::iter::once(&archive.trace).chain(&archive.files) {
        let path = base(bases, entry.dir)?.join(&entry.path);
        let temp = stage(&path, &entry.bytes)?;
        staged.push((temp, path));
    }
    let mut saved: Vec<PathBuf> = Vec::new();
    for (temp, path) in staged {
        if let Err(error) = temp.persist(&path) {
            let mut message = format!("write {}: {}", path.display(), error.error);
            for path in saved {
                if let Err(error) = fs::remove_file(&path) {
                    message += &format!("; roll back {}: {error}", path.display());
                }
            }
            return Err(message);
        }
        saved.push(path);
    }
    Ok(())
}

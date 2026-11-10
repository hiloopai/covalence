//! Replay in a fixed, already built checkout, sharing input digests per run.
use super::*;
use std::{os::unix::fs::MetadataExt, sync::Mutex};

#[derive(Args)]
pub struct Built {
    /// Recheck every theorem even when a prior successful receipt is available
    #[arg(long)]
    fresh: bool,
    /// One module name per line, selected by the caller's module cache
    #[arg(long)]
    modules: PathBuf,
    #[arg(long, default_value = ".")]
    project: PathBuf,
    /// Worker-owned successful receipts and native checker
    #[arg(long, default_value = ".lake/declaration-replay")]
    state: PathBuf,
    #[arg(long, default_value_t = 1)]
    jobs: usize,
}

#[derive(Clone, PartialEq, Eq)]
struct Stamp {
    dev: u64,
    ino: u64,
    len: u64,
    modified: (i64, i64),
    changed: (i64, i64),
}

fn stamp(path: &Path) -> Result<Stamp> {
    let meta = path
        .metadata()
        .into_diagnostic()
        .wrap_err_with(|| format!("stat {}", path.display()))?;
    Ok(Stamp {
        dev: meta.dev(),
        ino: meta.ino(),
        len: meta.len(),
        modified: (meta.mtime(), meta.mtime_nsec()),
        changed: (meta.ctime(), meta.ctime_nsec()),
    })
}

pub(super) struct Inputs {
    initial: BTreeMap<PathBuf, Stamp>,
    files: Mutex<BTreeMap<PathBuf, (Stamp, String)>>,
}

impl Inputs {
    fn new(roots: &[PathBuf]) -> Result<Self> {
        let mut initial = BTreeMap::new();
        let mut directories = roots.to_vec();
        let mut seen = std::collections::BTreeSet::new();
        while let Some(directory) = directories.pop() {
            let directory = match directory.canonicalize() {
                Ok(path) => path,
                // Lean's search path can include an unused, unbuilt package.
                // Files subsequently created there are absent from the inventory.
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error).into_diagnostic(),
            };
            if !seen.insert(directory.clone()) {
                continue;
            }
            for entry in fs::read_dir(&directory).into_diagnostic()? {
                let path = entry.into_diagnostic()?.path();
                let metadata = path.metadata().into_diagnostic()?;
                if metadata.is_dir() {
                    directories.push(path);
                } else if metadata.is_file() {
                    let path = path.canonicalize().into_diagnostic()?;
                    initial.insert(path.clone(), stamp(&path)?);
                }
            }
        }
        Ok(Self {
            initial,
            files: Mutex::new(BTreeMap::new()),
        })
    }

    pub(super) fn digest(&self, path: &Path) -> Result<String> {
        let path = path.canonicalize().into_diagnostic()?;
        let path = path.as_path();
        let before = stamp(path)?;
        // Filesystem clocks can lag the wall clock, even with nanosecond fields.
        // Membership and metadata come from before any native worker starts.
        if self.initial.get(path) != Some(&before) {
            return Err(miette!(
                "input changed during replay: {}; finish the build first",
                path.display()
            ));
        }
        let mut files = self
            .files
            .lock()
            .map_err(|e| miette!("input digest lock poisoned: {e}"))?;
        if let Some((saved, hash)) = files.get(path) {
            if saved != &before {
                return Err(miette!("input changed during replay: {}", path.display()));
            }
            return Ok(hash.clone());
        }
        let mut file = fs::File::open(path).into_diagnostic()?;
        let mut hasher = blake3::Hasher::new();
        hasher.update_reader(&mut file).into_diagnostic()?;
        if stamp(path)? != before {
            return Err(miette!("input changed while hashing: {}", path.display()));
        }
        let hash = hasher.finalize().to_hex().to_string();
        files.insert(path.to_owned(), (before, hash.clone()));
        Ok(hash)
    }

    pub(super) fn context(&self, inputs: &[InputFile]) -> Result<String> {
        let mut keys = Vec::with_capacity(inputs.len());
        for input in inputs {
            keys.push((&input.module, input.part, self.digest(&input.path)?));
        }
        keys.sort();
        fingerprint(&keys)
    }

    pub(super) fn verify(&self, inputs: &[InputFile]) -> Result<()> {
        for input in inputs {
            self.digest(&input.path)?;
        }
        Ok(())
    }

    fn verify_all(&self) -> Result<()> {
        let files = self
            .files
            .lock()
            .map_err(|e| miette!("input digest lock poisoned: {e}"))?;
        for (path, (saved, _)) in files.iter() {
            if &stamp(path)? != saved {
                return Err(miette!("input changed during replay: {}", path.display()));
            }
        }
        Ok(())
    }
}

fn relative_module(module: &str) -> Result<PathBuf> {
    if module.is_empty()
        || module.split('.').any(|part| {
            part.is_empty()
                || part
                    .chars()
                    .any(|c| !(c.is_alphanumeric() || c == '_' || c == '\''))
        })
    {
        return Err(miette!("invalid module name {module:?}"));
    }
    Ok(PathBuf::from(module.replace('.', "/")))
}

fn write_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| miette!("missing output parent"))?;
    fs::create_dir_all(parent).into_diagnostic()?;
    let mut file = tempfile::NamedTempFile::new_in(parent).into_diagnostic()?;
    serde_json::to_writer(&mut file, value).into_diagnostic()?;
    file.persist(path).into_diagnostic()?;
    Ok(())
}

pub(crate) fn run(args: Built) -> Result<()> {
    if args.jobs == 0 {
        return Err(miette!("jobs must be positive"));
    }
    let started = Instant::now();
    let root = args.project.canonicalize().into_diagnostic()?;
    let modules: Vec<_> = fs::read_to_string(&args.modules)
        .into_diagnostic()?
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(str::to_owned)
        .collect();
    let mut seen = std::collections::BTreeSet::new();
    for module in &modules {
        relative_module(module)?;
        if !seen.insert(module) {
            return Err(miette!("duplicate replay module {module}"));
        }
    }
    if modules.is_empty() {
        println!("{}", json!({"status":"kernel_replayed","modules":0}));
        return Ok(());
    }
    let state = root.join(args.state);
    fs::create_dir_all(&state).into_diagnostic()?;
    let state = state.canonicalize().into_diagnostic()?;
    let toolchain = PathBuf::from(
        String::from_utf8(checked_output(
            Command::new("lake")
                .current_dir(&root)
                .args(["env", "lean", "--print-prefix"]),
            "resolve replay toolchain",
        )?)
        .into_diagnostic()?
        .trim(),
    )
    .canonicalize()
    .into_diagnostic()?;
    let lean_path = String::from_utf8(checked_output(
        Command::new("lake")
            .current_dir(&root)
            .args(["env", "printenv", "LEAN_PATH"]),
        "resolve replay search path",
    )?)
    .into_diagnostic()?
    .trim()
    .to_owned();
    let mut roots: Vec<_> = std::env::split_paths(&lean_path)
        .map(|path| root.join(path))
        .collect();
    roots.extend([toolchain.join("bin"), toolchain.join("lib")]);
    let inputs = Inputs::new(&roots)?;
    // Pinned runtime bytes, plus actual imported files reported by each worker.
    // No toolchain archive, dependency copy, or persistent mtime trust is needed.
    let mut runtime = BTreeMap::new();
    for dir in [
        toolchain.join("bin"),
        toolchain.join("lib"),
        toolchain.join("lib/lean"),
    ] {
        for entry in fs::read_dir(dir).into_diagnostic()? {
            let path = entry.into_diagnostic()?.path();
            let name = path
                .file_name()
                .ok_or_else(|| miette!("invalid runtime path"))?
                .to_string_lossy();
            if path.is_file() && (name == "lean" || name == "leanc" || name.contains(".so")) {
                runtime.insert(
                    path.strip_prefix(&toolchain).into_diagnostic()?.to_owned(),
                    inputs.digest(&path)?,
                );
            }
        }
    }
    let context = fingerprint(&("mor/built-environment/v1", checker(), runtime))?;
    let bridge_dir = state.join("checkers").join(&context);
    let executable = bridge_dir.join("mor-kernel-replay");
    if !executable.is_file() {
        fs::create_dir_all(
            bridge_dir
                .parent()
                .ok_or_else(|| miette!("missing checker parent"))?,
        )
        .into_diagnostic()?;
        let pending = tempfile::tempdir_in(
            bridge_dir
                .parent()
                .ok_or_else(|| miette!("missing checker parent"))?,
        )
        .into_diagnostic()?;
        build_bridge(&toolchain, pending.path())?;
        fs::rename(pending.path(), &bridge_dir).into_diagnostic()?;
    }
    let next = std::sync::atomic::AtomicUsize::new(0);
    let results = std::thread::scope(|scope| -> Result<Vec<Value>> {
        let mut handles = Vec::new();
        for _ in 0..args.jobs.min(modules.len()) {
            let (root, state, modules, context, executable, toolchain, lean_path, inputs, next) = (
                &root,
                &state,
                &modules,
                &context,
                &executable,
                &toolchain,
                &lean_path,
                &inputs,
                &next,
            );
            handles.push(scope.spawn(move || -> Result<Vec<Value>> {
                let mut reports = Vec::new();
                loop {
                    let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    let Some(module) = modules.get(i) else { break };
                    eprintln!("replay: starting [{}/{}] {module}", i + 1, modules.len());
                    let relative = relative_module(module)?;
                    // Resolve the imports recorded in the compiled module through Lake's
                    // current search path, as upstream replay does. Compiler setup files
                    // can be absent or stale after an artifact restore.
                    let receipt = state.join("modules").join(&relative).with_extension("json");
                    let previous: Option<Value> = match fs::read(&receipt) {
                        Ok(bytes) => Some(serde_json::from_slice(&bytes).into_diagnostic()?),
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
                        Err(e) => return Err(e).into_diagnostic(),
                    };
                    let previous = previous
                        .map(|value| {
                            value["replay_cache"]
                                .as_str()
                                .ok_or_else(|| miette!("invalid replay receipt"))?
                                .parse()
                                .into_diagnostic()
                        })
                        .transpose()?;
                    let previous = if args.fresh { None } else { previous };
                    let module_dir = state.join("cas").join(&relative);
                    fs::create_dir_all(&module_dir).into_diagnostic()?;
                    let mut result = check_module(
                        &Invocation {
                            file: root
                                .join(".lake/build/lib/lean")
                                .join(&relative)
                                .with_extension("olean"),
                            imports: ImportSource::SearchPath(module.clone()),
                            toolchain: toolchain.clone(),
                            executable: executable.clone(),
                            cwd: root.clone(),
                            lean_path: Some(lean_path.into()),
                            context: context.clone(),
                            previous,
                            trace: Some(module_dir.join("last-trace.jsonl")),
                            cache: module_dir.join("cache.db").to_string_lossy().into_owned(),
                        },
                        Some(inputs),
                    )
                    .wrap_err_with(|| format!("replay {module}"))?;
                    result["module"] = json!(module);
                    write_json(&receipt, &result)?;
                    eprintln!(
                        "replay: [{}/{}] {module}: {} reused, {} fresh theorems",
                        i + 1,
                        modules.len(),
                        result["reused_theorems"],
                        result["fresh_theorems"]
                    );
                    reports.push(result);
                }
                Ok(reports)
            }));
        }
        let mut all = Vec::new();
        let mut failure = None;
        for handle in handles {
            match handle
                .join()
                .unwrap_or_else(|_| Err(miette!("replay worker panicked")))
            {
                Ok(reports) => all.extend(reports),
                Err(error) => {
                    if failure.is_none() {
                        failure = Some(error);
                    } else {
                        eprintln!("replay: another worker failed: {error:?}");
                    }
                }
            }
        }
        if let Some(error) = failure {
            return Err(error);
        }
        Ok(all)
    })?;
    inputs.verify_all()?;
    let report = json!({"status":"kernel_replayed", "modules":results.len(),
        "seconds":started.elapsed().as_secs_f64(), "results":results});
    write_json(&state.join("last-run.json"), &report)?;
    println!("{report}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn fixed_inputs_detect_edits_and_new_files() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("input");
        fs::write(&file, "before").unwrap();
        let inputs = Inputs::new(&[dir.path().to_owned()]).unwrap();
        assert_eq!(inputs.digest(&file).unwrap(), inputs.digest(&file).unwrap());
        fs::write(&file, "after").unwrap();
        assert!(inputs.digest(&file).is_err());
        let fresh = dir.path().join("new");
        fs::write(&fresh, "new").unwrap();
        assert!(inputs.digest(&fresh).is_err());
    }

    #[test]
    fn first_read_rejects_replacement_and_missing_roots_never_admit_new_files() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("input");
        fs::write(&file, "before").unwrap();
        let missing = dir.path().join("missing");
        let inputs = Inputs::new(&[dir.path().to_owned(), missing.clone()]).unwrap();
        let replacement = dir.path().join("replacement");
        fs::write(&replacement, "after!").unwrap();
        fs::rename(&replacement, &file).unwrap();
        assert!(inputs.digest(&file).is_err());
        fs::create_dir(&missing).unwrap();
        let new = missing.join("new");
        fs::write(&new, "new").unwrap();
        assert!(inputs.digest(&new).is_err());
    }

    #[test]
    fn final_verification_covers_earlier_workers_and_runtime_inputs() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("input");
        fs::write(&file, "before").unwrap();
        let inputs = Inputs::new(&[dir.path().to_owned()]).unwrap();
        inputs.digest(&file).unwrap();
        inputs.verify_all().unwrap();
        fs::write(&file, "after").unwrap();
        assert!(inputs.verify_all().is_err());
    }
    #[test]
    fn module_paths_cannot_escape_checkout() {
        assert_eq!(
            relative_module("Library.Example'").unwrap(),
            PathBuf::from("Library/Example'")
        );
        for name in [
            "",
            ".Library",
            "Library..X",
            "../bad",
            "Library/X",
            "Library\\X",
        ] {
            assert!(relative_module(name).is_err());
        }
    }
}

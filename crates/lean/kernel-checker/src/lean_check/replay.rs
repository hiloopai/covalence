//! Experimental compiled-declaration replay over a trusted CAS.
use super::*;
pub(super) mod built;
use covalence_cas_prolly::{Edit, Map};
use covalence_lib_dasl::Value as Dasl;
use std::{
    collections::VecDeque,
    io::{BufRead, BufReader},
    process::{Child, Stdio},
    sync::Arc,
    time::Instant,
};

const FORMAT: &str = "mor/kernel-replay/v1";

#[derive(Args)]
pub struct Replay {
    /// Primary .olean from a completed Lake build, with its trace and all parts
    file: PathBuf,
    #[arg(long)]
    environment: Blake3,
    /// Successful replay manifest from this trusted cache
    #[arg(long)]
    previous_replay: Option<Blake3>,
    #[arg(long)]
    trace: Option<PathBuf>,
    #[command(flatten)]
    storage: Storage,
}

#[derive(Deserialize)]
#[serde(crate = "covalence_lib_serde::serde")]
#[serde(deny_unknown_fields)]
struct Header {
    protocol: String,
    module: String,
    imports: Value,
    is_module: bool,
    declarations: usize,
    load_ms: u64,
    inputs: Vec<InputFile>,
}

#[derive(Deserialize)]
#[serde(crate = "covalence_lib_serde::serde")]
#[serde(deny_unknown_fields)]
struct InputFile {
    module: String,
    part: usize,
    path: PathBuf,
}

#[derive(Deserialize)]
#[serde(crate = "covalence_lib_serde::serde")]
#[serde(deny_unknown_fields)]
struct Payload {
    name: String,
    name_key: String,
    kind: String,
    fallback: Option<String>,
    interface: Value,
    body: Value,
    dependencies: Vec<String>,
    statement_only: bool,
    interface_dependencies: Vec<String>,
}

#[derive(Clone)]
struct Record {
    name: String,
    key: String,
    kind: String,
    fallback: Option<String>,
    local: Option<String>,
    dependencies: Vec<String>,
    query: Option<String>,
    /// For a theorem no consumer's check can see through (see the bridge's
    /// `statementOnly`): its kind and type, with the local constants the type uses.
    statement: Option<(String, Vec<String>)>,
    /// What consumers key on: `statement`'s fingerprint when set, else `query`.
    dependency_key: Option<String>,
}

#[derive(Serialize, Deserialize)]
#[serde(crate = "covalence_lib_serde::serde")]
#[serde(deny_unknown_fields)]
struct Manifest {
    kind: String,
    context: String,
    root: String,
}

fn fingerprint(value: &impl Serialize) -> Result<String> {
    Ok(Blake3::of(&serde_json::to_vec(value).into_diagnostic()?).to_string())
}

fn checker() -> String {
    let mut hash = blake3::Hasher::new();
    hash.update(include_bytes!("replay.rs"));
    hash.update(include_bytes!("KernelReplay.lean"));
    hash.update(include_bytes!("replay/built.rs"));
    hash.update(include_bytes!("../lean_check.rs"));
    hash.update(include_bytes!("../../../../../Cargo.lock"));
    hash.finalize().to_hex().to_string()
}

pub(super) fn build_bridge(prefix: &Path, dir: &Path) -> Result<PathBuf> {
    let source = dir.join("KernelReplay.lean");
    let c = dir.join("KernelReplay.c");
    let executable = dir.join("mor-kernel-replay");
    fs::write(&source, include_bytes!("KernelReplay.lean")).into_diagnostic()?;
    checked_output(
        Command::new(prefix.join("bin/lean"))
            .current_dir(dir)
            .arg("-c")
            .arg(&c)
            .arg(&source),
        "compile declaration replay bridge",
    )?;
    checked_output(
        Command::new(prefix.join("bin/leanc"))
            .args([
                "-O2",
                "-rdynamic",
                "-leanshared",
                "-lleanshared",
                "-Wl,-rpath,$ORIGIN/../lib/lean",
            ])
            .arg("-o")
            .arg(&executable)
            .arg(&c),
        "link declaration replay bridge",
    )?;
    Ok(executable)
}

fn record(payload: Payload) -> Result<Record> {
    let parts: Vec<(String, String)> = serde_json::from_str(&payload.name_key).into_diagnostic()?;
    if parts.is_empty() || parts.iter().any(|(tag, _)| tag != "str" && tag != "num") {
        return Err(miette!("invalid structural declaration identity"));
    }
    let statement = if payload.fallback.is_none() && payload.statement_only {
        let mut uses = payload.interface_dependencies;
        uses.sort();
        uses.dedup();
        Some((fingerprint(&(&payload.kind, &payload.interface))?, uses))
    } else {
        None
    };
    let local = if payload.fallback.is_none() {
        if payload.interface.is_null()
            || (matches!(payload.kind.as_str(), "theorem" | "definition" | "opaque")
                && payload.body.is_null())
        {
            return Err(miette!("incomplete declaration kernel inputs"));
        }
        Some(fingerprint(&(
            &payload.kind,
            &payload.interface,
            &payload.body,
        ))?)
    } else {
        None
    };
    let mut dependencies = payload.dependencies;
    dependencies.sort();
    dependencies.dedup();
    Ok(Record {
        name: payload.name,
        key: payload.name_key,
        kind: payload.kind,
        fallback: payload.fallback,
        local,
        dependencies,
        query: None,
        statement,
        dependency_key: None,
    })
}

// Exact nominal terms everywhere. A consumer keys on a dependency's complete
// inputs, except for theorems whose proofs the kernel cannot inspect, where it
// keys on their statements (`Record::statement`).
fn queries(records: &mut [Record], context: &str) -> Result<()> {
    // Inductives, constructors and recursors refer to each other. Hash their
    // complete stored metadata as one conservative block, then order its
    // external dependencies normally. The block is always freshly replayed.
    let structural: BTreeMap<_, _> = records
        .iter()
        .filter(|r| matches!(r.kind.as_str(), "inductive" | "constructor" | "recursor"))
        .map(|r| (r.key.clone(), r.local.clone()))
        .collect();
    if structural.is_empty() {
        return acyclic_queries(records, context);
    }
    let group = "structural-block";
    let dependencies: std::collections::BTreeSet<_> = records
        .iter()
        .filter(|r| structural.contains_key(&r.key))
        .flat_map(|r| &r.dependencies)
        .filter(|key| !structural.contains_key(*key))
        .cloned()
        .collect();
    let local = if structural.values().all(Option::is_some) {
        Some(fingerprint(&structural)?)
    } else {
        None
    };
    let mut graph = records.to_vec();
    for r in &mut graph {
        if structural.contains_key(&r.key) {
            r.dependencies = vec![group.into()];
        }
    }
    graph.push(Record {
        name: group.into(),
        key: group.into(),
        kind: "group".into(),
        fallback: None,
        local,
        dependencies: dependencies.into_iter().collect(),
        query: None,
        statement: None,
        dependency_key: None,
    });
    acyclic_queries(&mut graph, context)?;
    for (record, result) in records.iter_mut().zip(graph) {
        record.query = result.query;
        record.dependency_key = result.dependency_key;
    }
    Ok(())
}

fn acyclic_queries(records: &mut [Record], context: &str) -> Result<()> {
    for record in records.iter_mut() {
        record.query = None;
        record.dependency_key = None;
    }
    let positions: BTreeMap<_, _> = records
        .iter()
        .enumerate()
        .map(|(i, r)| (r.key.clone(), i))
        .collect();
    if positions.len() != records.len() {
        return Err(miette!("duplicate declaration identities"));
    }
    let mut waiting = vec![0; records.len()];
    let mut consumers = vec![Vec::new(); records.len()];
    for (i, record) in records.iter().enumerate() {
        for dependency in &record.dependencies {
            let index = *positions
                .get(dependency)
                .ok_or_else(|| miette!("missing local dependency {dependency}"))?;
            waiting[i] += 1;
            consumers[index].push(i);
        }
    }
    let mut ready: VecDeque<_> = waiting
        .iter()
        .enumerate()
        .filter_map(|(i, n)| (*n == 0).then_some(i))
        .collect();
    while let Some(i) = ready.pop_front() {
        let deps = dependency_keys(records, &positions, &records[i].dependencies);
        let query = match (&records[i].local, deps) {
            (Some(local), Some(deps)) => Some(fingerprint(&(
                FORMAT,
                context,
                &records[i].key,
                local,
                deps,
            ))?),
            _ => None,
        };
        let dependency_key = match (&records[i].statement, &query) {
            // The statement's own uses are among `dependencies`, so they are keyed.
            (Some((statement, uses)), Some(_)) => dependency_keys(records, &positions, uses)
                .map(|uses| fingerprint(&("statement", context, &records[i].key, statement, uses)))
                .transpose()?,
            _ => query.clone(),
        };
        records[i].query = query;
        records[i].dependency_key = dependency_key;
        for &next in &consumers[i] {
            waiting[next] -= 1;
            if waiting[next] == 0 {
                ready.push_back(next);
            }
        }
    }
    Ok(())
}

fn dependency_keys(
    records: &[Record],
    positions: &BTreeMap<String, usize>,
    names: &[String],
) -> Option<Vec<(String, String)>> {
    names
        .iter()
        .map(|key| {
            records[positions[key]]
                .dependency_key
                .clone()
                .map(|query| (key.clone(), query))
        })
        .collect()
}

struct Bridge {
    child: Child,
    input: std::process::ChildStdin,
    output: BufReader<std::process::ChildStdout>,
    diagnostics: tempfile::NamedTempFile,
}

impl Bridge {
    fn read<T: covalence_lib_serde::serde::de::DeserializeOwned>(&mut self) -> Result<T> {
        let mut line = String::new();
        if self.output.read_line(&mut line).into_diagnostic()? == 0 {
            let status = self.child.wait().into_diagnostic()?;
            return Err(miette!(
                "kernel replay ended early ({status}):\n{}",
                fs::read_to_string(self.diagnostics.path()).into_diagnostic()?
            ));
        }
        serde_json::from_str(&line)
            .into_diagnostic()
            .wrap_err("invalid replay bridge report")
    }
}

impl Drop for Bridge {
    fn drop(&mut self) {
        if let Err(error) = self.child.kill()
            && error.kind() != std::io::ErrorKind::InvalidInput
        {
            eprintln!("stop replay bridge: {error}");
        }
        if let Err(error) = self.child.wait() {
            eprintln!("reap replay bridge: {error}");
        }
    }
}

pub(super) fn run(args: Replay) -> Result<()> {
    let workspace = args.storage.workspace.canonicalize().into_diagnostic()?;
    let meta = workspace
        .join("environments")
        .join(args.environment.to_string());
    let bytes = read(&meta.with_extension("json"))?;
    if Blake3::of(&bytes) != args.environment {
        return Err(miette!("environment CID mismatch"));
    }
    let environment = decode_environment(&bytes)?;
    if !environment.kernel_replay {
        return Err(miette!("prepare with --kernel-replay first"));
    }
    let toolchain = object_path(&workspace, &environment.toolchain)?;
    let result = check_module(
        &Invocation {
            file: args.file,
            imports: ImportSource::Prepared(meta.with_extension("setup.json")),
            toolchain: toolchain.clone(),
            executable: toolchain.join("bin/mor-kernel-replay"),
            cwd: toolchain,
            lean_path: None,
            context: args.environment.to_string(),
            previous: args.previous_replay,
            trace: args.trace,
            cache: args.storage.cache,
        },
        None,
    )?;
    println!("{result}");
    Ok(())
}

enum ImportSource {
    Prepared(PathBuf),
    SearchPath(String),
}

struct Invocation {
    file: PathBuf,
    imports: ImportSource,
    toolchain: PathBuf,
    executable: PathBuf,
    cwd: PathBuf,
    lean_path: Option<std::ffi::OsString>,
    context: String,
    previous: Option<Blake3>,
    trace: Option<PathBuf>,
    cache: String,
}

fn check_module(args: &Invocation, inputs: Option<&built::Inputs>) -> Result<Value> {
    let started = Instant::now();
    let mut trace = trace::Trace::open(args.trace.as_deref())?;
    let source = args.file.canonicalize().into_diagnostic()?;
    if source.extension().is_none_or(|s| s != "olean") {
        return Err(miette!("expected primary .olean"));
    }
    let scratch = tempfile::tempdir().into_diagnostic()?;
    let artifact = scratch.path().join("target.olean");
    let mut source_parts = Vec::new();
    for suffix in ["trace", "olean", "olean.server", "olean.private"] {
        let file = source.with_extension(suffix);
        if file.exists() {
            if let Some(inputs) = inputs {
                inputs.digest(&file)?;
                source_parts.push(file.clone());
            }
            fs::copy(&file, artifact.with_extension(suffix))
                .into_diagnostic()
                .wrap_err_with(|| format!("snapshot artifact {}", file.display()))?;
        }
    }
    let diagnostics = tempfile::NamedTempFile::new().into_diagnostic()?;
    let mut command = Command::new(&args.executable);
    match &args.imports {
        ImportSource::Prepared(setup) => {
            command.arg(setup);
        }
        ImportSource::SearchPath(module) => {
            command.args(["--search-path", module]);
        }
    }
    let mut child = command
        .current_dir(&args.cwd)
        .env_clear()
        .env("LEAN_PATH", args.lean_path.as_deref().unwrap_or_default())
        .env("LD_LIBRARY_PATH", args.toolchain.join("lib/lean"))
        .env("LEAN_SYSROOT", &args.toolchain)
        .env("PATH", args.toolchain.join("bin"))
        .env("LEAN_NUM_THREADS", "1")
        .arg(&artifact)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(diagnostics.as_file().try_clone().into_diagnostic()?)
        .spawn()
        .into_diagnostic()?;
    let input = child
        .stdin
        .take()
        .ok_or_else(|| miette!("missing replay stdin"))?;
    let output = BufReader::new(
        child
            .stdout
            .take()
            .ok_or_else(|| miette!("missing replay stdout"))?,
    );
    let mut bridge = Bridge {
        child,
        input,
        output,
        diagnostics,
    };
    let header: Header = bridge.read()?;
    if header.protocol != FORMAT {
        return Err(miette!("unsupported replay bridge protocol"));
    }
    let context = fingerprint(&(
        FORMAT,
        checker(),
        &args.context,
        inputs
            .map(|inputs| inputs.context(&header.inputs))
            .transpose()?,
        &header.module,
        &header.imports,
        header.is_module,
        "exact-nominal-kernel-terms;statement-keyed-irrelevant-proofs;trust=0;no-sorry;theorem-reuse-only",
    ))?;
    let mut records = Vec::with_capacity(header.declarations);
    for _ in 0..header.declarations {
        records.push(record(bridge.read()?)?);
    }
    let encoded: Value = bridge.read()?;
    if encoded["encoded"] != true {
        return Err(miette!("incomplete kernel input encoding"));
    }
    queries(&mut records, &context)?;
    let cas: Arc<dyn Cas> = Arc::from(store(&args.cache)?);
    let mut prior = BTreeMap::new();
    let map = if let Some(id) = args.previous {
        let fact = cas
            .get_fact(&id.into())
            .into_diagnostic()?
            .ok_or_else(|| miette!("missing replay manifest {id}"))?;
        let previous: Manifest = serde_json::from_slice(fact.bytes()).into_diagnostic()?;
        if previous.kind != FORMAT {
            return Err(miette!("not a successful kernel replay manifest"));
        }
        let map = Map::open(cas.clone(), previous.root.parse().into_diagnostic()?);
        for entry in map.range(..).into_diagnostic()? {
            let (key, value) = entry.into_diagnostic()?;
            let Dasl::String(query) = value else {
                return Err(miette!("invalid replay index"));
            };
            prior.insert(String::from_utf8(key.to_vec()).into_diagnostic()?, query);
        }
        map
    } else {
        Map::empty(cas.clone()).into_diagnostic()?
    };
    let reuse: Vec<_> = records
        .iter()
        .filter(|r| {
            r.kind == "theorem"
                && r.query
                    .as_ref()
                    .is_some_and(|q| prior.get(&r.key) == Some(q))
        })
        .map(|r| r.key.clone())
        .collect();
    writeln!(bridge.input, "{}", json!({"reuse": reuse})).into_diagnostic()?;
    bridge.input.flush().into_diagnostic()?;
    let done: Value = bridge.read()?;
    let status = bridge.child.wait().into_diagnostic()?;
    if !status.success() || done["done"] != true || done["reused"] != reuse.len() {
        return Err(miette!(
            "kernel replay failed ({status}):\n{}",
            fs::read_to_string(bridge.diagnostics.path()).into_diagnostic()?
        ));
    }
    let mut extra = String::new();
    if bridge.output.read_line(&mut extra).into_diagnostic()? != 0 {
        return Err(miette!("unexpected trailing replay output"));
    }
    // Publish no acceptance record until the complete current module succeeds.
    if let Some(inputs) = inputs {
        inputs.verify(&header.inputs)?;
        for path in &source_parts {
            inputs.digest(path)?;
        }
    }
    let mut edits = Vec::new();
    let mut fresh_theorems = 0;
    let mut fresh_other = 0;
    let mut excluded = 0;
    for record in &records {
        let old = prior.remove(&record.key);
        let hit = record.kind == "theorem"
            && record
                .query
                .as_ref()
                .is_some_and(|q| old.as_ref() == Some(q));
        let skipped = record.fallback.as_deref() == Some("unsafe_or_partial");
        if skipped {
            excluded += 1;
        } else if !hit && record.kind == "theorem" {
            fresh_theorems += 1;
        } else if !hit {
            fresh_other += 1;
        }
        trace.event(|| {
            json!({"operation":"kernel_replay", "name":record.name,
            "identity":record.key, "input_fingerprint":record.query,
            "outcome": if hit {"reused"} else if skipped {"excluded"} else {"checked"},
            "reason":if hit {"exact_proof_and_dependencies"} else if skipped {"unsafe_or_partial"}
                else if record.query.is_none() {"unsupported_or_cyclic_dependencies"}
                else {"cache_miss"}})
        })?;
        if record.kind == "theorem"
            && let Some(query) = &record.query
        {
            if old.as_ref() != Some(query) {
                edits.push(Edit::Insert(
                    record.key.as_bytes().to_vec().into(),
                    Dasl::String(query.clone()),
                ));
            }
        } else if old.is_some() {
            edits.push(Edit::Remove(record.key.as_bytes().to_vec().into()));
        }
    }
    for key in prior.into_keys() {
        edits.push(Edit::Remove(key.into_bytes().into()));
    }
    edits.sort_by(|a, b| a.key().cmp(b.key()));
    let map = map.apply(edits).into_diagnostic()?;
    let manifest = Manifest {
        kind: FORMAT.into(),
        context,
        root: map.root().to_string(),
    };
    let id = cas
        .put(serde_json::to_vec(&manifest).into_diagnostic()?.into())
        .into_diagnostic()?;
    Ok(
        json!({"status":"kernel_replayed", "replay_cache":id.to_string(),
        "declarations":records.len(), "reused_theorems":reuse.len(),
        "fresh_theorems":fresh_theorems, "fresh_other_declarations":fresh_other,
        "excluded_unsafe_or_partial":excluded,
        "timings_ms":{"load":header.load_ms, "encoding":encoded["encoding_ms"],
            "replay":done["replay_ms"], "total":started.elapsed().as_secs_f64()*1000.0}}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn replay_receipts_merge_insertions_and_removed_declarations_in_key_order() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let file = root.join("Proof.olean");
        let executable = root.join("bridge");
        fs::write(&file, "compiled module").unwrap();
        let invocation = |previous| Invocation {
            file: file.clone(),
            imports: ImportSource::SearchPath("Proof".into()),
            toolchain: root.to_owned(),
            executable: executable.clone(),
            cwd: root.to_owned(),
            lean_path: None,
            context: "fixed-context".into(),
            previous,
            trace: None,
            cache: root.join("receipts.db").to_str().unwrap().into(),
        };
        let bridge = |names: &[&str], reused: usize| {
            let mut reports = vec![json!({"protocol": FORMAT, "module": "Proof",
                "imports": [], "is_module": true, "declarations": names.len(),
                "load_ms": 0, "inputs": []})];
            for name in names {
                reports.push(json!({"name": name,
                    "name_key": serde_json::to_string(&[("str", name)]).unwrap(),
                    "kind": "theorem", "fallback": null, "interface": "True",
                    "body": "True.intro", "dependencies": [], "statement_only": true,
                    "interface_dependencies": []}));
            }
            reports.push(json!({"encoded": true, "encoding_ms": 0}));
            let output = reports
                .iter()
                .map(Value::to_string)
                .collect::<Vec<_>>()
                .join("\n");
            fs::write(&executable, format!(
                "#!/bin/sh\n/bin/cat <<'REPORTS'\n{output}\nREPORTS\nIFS= read -r reuse\nprintf '%s\\n' '{{\"done\":true,\"reused\":{reused},\"replay_ms\":0}}'\n"
            )).unwrap();
            fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).unwrap();
        };
        bridge(&["a", "m", "z"], 0);
        let initial = check_module(&invocation(None), None).unwrap();
        bridge(&["b", "n", "z"], 1);
        let changed = check_module(
            &invocation(Some(
                initial["replay_cache"].as_str().unwrap().parse().unwrap(),
            )),
            None,
        )
        .unwrap();
        assert_eq!(changed["reused_theorems"], 1);
        assert_eq!(changed["fresh_theorems"], 2);
        bridge(&["b", "n", "z"], 3);
        let unchanged = check_module(
            &invocation(Some(
                changed["replay_cache"].as_str().unwrap().parse().unwrap(),
            )),
            None,
        )
        .unwrap();
        assert_eq!(unchanged["reused_theorems"], 3);
        assert_eq!(unchanged["fresh_theorems"], 0);
    }

    fn item(name: &str, local: Option<&str>, deps: &[&str]) -> Record {
        Record {
            name: name.into(),
            key: name.into(),
            kind: "theorem".into(),
            fallback: None,
            local: local.map(str::to_owned),
            dependencies: deps.iter().map(|s| (*s).into()).collect(),
            query: None,
            statement: None,
            dependency_key: None,
        }
    }

    #[test]
    fn dependency_changes_propagate_but_unrelated_insertions_do_not() {
        let mut old = vec![
            item("a", Some("body-a"), &[]),
            item("b", Some("body-b"), &["a"]),
            item("c", Some("body-c"), &[]),
        ];
        queries(&mut old, "environment").unwrap();
        let mut new = vec![
            item("inserted", Some("body-new"), &[]),
            item("c", Some("body-c"), &[]),
            item("b", Some("body-b"), &["a"]),
            item("a", Some("body-a2"), &[]),
        ];
        queries(&mut new, "environment").unwrap();
        assert_ne!(old[0].query, new[3].query);
        assert_ne!(old[1].query, new[2].query);
        assert_eq!(old[2].query, new[1].query);
        let prior = new[1].query.clone();
        queries(&mut new, "different-environment").unwrap();
        assert_ne!(prior, new[1].query);
    }

    #[test]
    fn irrelevant_proof_edits_keep_consumer_keys_but_statements_and_their_uses_do_not() {
        let build = |proof: &str, statement: &str, uses: &str, irrelevant: bool| {
            let mut items = vec![
                item("used", Some(uses), &[]),
                item("lemma", Some(proof), &["used"]),
                item("consumer", Some("body"), &["lemma"]),
            ];
            if irrelevant {
                items[1].statement = Some((statement.into(), vec!["used".into()]));
            }
            queries(&mut items, "context").unwrap();
            items
        };
        let base = build("proof", "type", "def", true);
        let proof_edit = build("proof2", "type", "def", true);
        // The edited theorem itself is checked again; its consumer is not.
        assert_ne!(base[1].query, proof_edit[1].query);
        assert_eq!(base[2].query, proof_edit[2].query);
        assert_ne!(base[2].query, build("proof", "type2", "def", true)[2].query);
        // A definition the statement uses can change what the statement means.
        assert_ne!(base[2].query, build("proof", "type", "def2", true)[2].query);
        // Proofs the kernel can reduce keep invalidating their consumers.
        let relevant = build("proof", "type", "def", false);
        assert_ne!(
            relevant[2].query,
            build("proof2", "type", "def", false)[2].query
        );
    }

    #[test]
    fn structural_cycles_share_complete_inputs_and_propagate_changes() {
        let mut items = vec![
            item("type", Some("shape"), &["ctor"]),
            item("ctor", Some("fields"), &["type", "ctor"]),
            item("proof", Some("body"), &["type"]),
            item("other", Some("body"), &[]),
        ];
        items[0].kind = "inductive".into();
        items[1].kind = "constructor".into();
        queries(&mut items, "context").unwrap();
        assert!(items.iter().all(|r| r.query.is_some()));
        let original = items.clone();
        items[1].local = Some("changed-fields".into());
        queries(&mut items, "context").unwrap();
        assert_ne!(items[2].query, original[2].query);
        assert_eq!(items[3].query, original[3].query);
        items[1].local = None;
        // A second evaluation must not retain earlier keys for blocked nodes.
        for item in &mut items {
            item.query = None;
        }
        queries(&mut items, "context").unwrap();
        assert!(items[..3].iter().all(|r| r.query.is_none()));
    }

    #[test]
    fn unsupported_and_cyclic_closures_never_receive_reuse_keys() {
        let mut items = vec![
            item("unsupported", None, &[]),
            item("uses", Some("body"), &["unsupported"]),
            item("cycle1", Some("a"), &["cycle2"]),
            item("cycle2", Some("b"), &["cycle1"]),
        ];
        queries(&mut items, "context").unwrap();
        assert!(items.iter().all(|r| r.query.is_none()));
        assert!(queries(&mut [item("missing", Some("a"), &["absent"])], "context").is_err());
        assert!(
            queries(
                &mut [item("same", Some("a"), &[]), item("same", Some("b"), &[])],
                "context"
            )
            .is_err()
        );
    }
}

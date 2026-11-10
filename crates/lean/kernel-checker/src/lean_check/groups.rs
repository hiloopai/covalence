//! Proof replacements against a pinned baseline, not full-source equivalence.
use super::*;
use covalence_cas_prolly::{Edit, Map};
use covalence_lib_dasl::Value as Dasl;
use std::{
    io::{BufRead, BufReader, Read, Seek, SeekFrom},
    process::{Child, Stdio},
    sync::Arc,
};

#[derive(Args)]
pub struct ProofGroups {
    file: PathBuf,
    #[arg(long)]
    environment: Blake3,
    /// Manifest returned by an earlier proof-groups run (also usable on a fresh worker)
    #[arg(long)]
    previous_proofs: Option<Blake3>,
    /// JSONL execution evidence for regression tests; does not affect cache identity
    #[arg(long, hide = true)]
    trace: Option<PathBuf>,
    #[command(flatten)]
    storage: Storage,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(crate = "covalence_lib_serde::serde")]
#[serde(deny_unknown_fields)]
struct Span {
    source: String,
    len: usize,
}

impl Span {
    fn new(bytes: &[u8]) -> Self {
        Self {
            source: digest(bytes),
            len: bytes.len(),
        }
    }
    fn matches(&self, source: &[u8], start: usize) -> bool {
        start
            .checked_add(self.len)
            .and_then(|end| source.get(start..end))
            .is_some_and(|bytes| digest(bytes) == self.source)
    }
    fn value(&self) -> Result<Dasl> {
        Ok(Dasl::Bytes(
            serde_json::to_vec(self).into_diagnostic()?.into(),
        ))
    }
    fn decode(value: Dasl) -> Result<Self> {
        let Dasl::Bytes(bytes) = value else {
            return Err(miette!("invalid source span"));
        };
        serde_json::from_slice(&bytes).into_diagnostic()
    }
}

#[derive(Serialize, Deserialize)]
#[serde(crate = "covalence_lib_serde::serde")]
#[serde(deny_unknown_fields)]
struct Group {
    signature: String,
    source: Span,
    snapshot: Snapshot,
    count: usize,
    proofs: bool,
    leading: bool,
}

#[derive(Serialize, Deserialize)]
#[serde(crate = "covalence_lib_serde::serde")]
#[serde(deny_unknown_fields)]
struct Snapshot {
    pack: String,
    offset: u64,
    len: u64,
    source: String,
}

#[derive(Serialize, Deserialize)]
#[serde(crate = "covalence_lib_serde::serde")]
#[serde(deny_unknown_fields)]
struct Baseline {
    kind: String,
    environment: String,
    checker: String,
    header: Span,
    tail: Span,
    groups: Vec<Group>,
}

#[derive(Serialize, Deserialize)]
#[serde(crate = "covalence_lib_serde::serde")]
#[serde(deny_unknown_fields)]
struct Revision {
    kind: String,
    baseline: String,
    root: String,
    source: String,
    history: String,
}

#[derive(Deserialize)]
#[serde(crate = "covalence_lib_serde::serde")]
struct ParsedGroup {
    index: usize,
    start: usize,
    stop: usize,
    signature: String,
    count: usize,
    proofs: bool,
    leading: bool,
}

fn checker() -> String {
    let mut hash = blake3::Hasher::new();
    hash.update(include_bytes!("groups.rs"));
    hash.update(include_bytes!("Groups.lean"));
    hash.update(include_bytes!("../lean_check.rs"));
    hash.update(include_bytes!("../../../../../Cargo.lock"));
    hash.finalize().to_hex().to_string()
}

pub(super) fn build_bridge(prefix: &Path, dir: &Path) -> Result<PathBuf> {
    let source = dir.join("Groups.lean");
    let c = dir.join("Groups.c");
    let executable = dir.join("mor-proof-groups");
    fs::write(&source, include_bytes!("Groups.lean")).into_diagnostic()?;
    checked_output(
        Command::new(prefix.join("bin/lean"))
            .current_dir(dir)
            .arg("-c")
            .arg(&c)
            .arg(&source),
        "compile native group bridge",
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
        "link native group bridge",
    )?;
    Ok(executable)
}

fn put(cas: &dyn Cas, value: &impl Serialize) -> Result<String> {
    Ok(cas
        .put(serde_json::to_vec(value).into_diagnostic()?.into())
        .into_diagnostic()?
        .to_string())
}

fn get<T: covalence_lib_serde::serde::de::DeserializeOwned>(cas: &dyn Cas, id: &str) -> Result<T> {
    let cid: Cid = id.parse().into_diagnostic()?;
    let fact = cas
        .get_fact(&cid)
        .into_diagnostic()?
        .ok_or_else(|| miette!("missing proof-group object {id}"))?;
    serde_json::from_slice(fact.bytes()).into_diagnostic()
}

fn digest(bytes: &[u8]) -> String {
    Blake3::of(bytes).to_string()
}

fn snapshot(cas: &dyn Cas, workspace: &Path, object: &Snapshot) -> Result<PathBuf> {
    let cid: Cid = object.source.parse().into_diagnostic()?;
    let dir = workspace.join("group-snapshots");
    fs::create_dir_all(&dir).into_diagnostic()?;
    let path = dir.join(cid.to_string());
    if !path.is_file() {
        let pack_id: Cid = object.pack.parse().into_diagnostic()?;
        let packs = workspace.join("group-packs");
        fs::create_dir_all(&packs).into_diagnostic()?;
        let pack_path = packs.join(pack_id.to_string());
        if !pack_path.is_file() {
            let fact = cas
                .get_fact(&pack_id)
                .into_diagnostic()?
                .ok_or_else(|| miette!("missing group snapshot pack {pack_id}"))?;
            let mut file = tempfile::NamedTempFile::new_in(&packs).into_diagnostic()?;
            file.write_all(fact.bytes()).into_diagnostic()?;
            file.as_file()
                .set_permissions(fs::Permissions::from_mode(0o444))
                .into_diagnostic()?;
            file.persist(&pack_path).into_diagnostic()?;
        }
        let mut pack = fs::File::open(&pack_path).into_diagnostic()?;
        let size = pack.metadata().into_diagnostic()?.len();
        if object
            .offset
            .checked_add(object.len)
            .is_none_or(|end| end > size)
        {
            return Err(miette!("snapshot range exceeds pack"));
        }
        pack.seek(SeekFrom::Start(object.offset))
            .into_diagnostic()?;
        let mut bytes = Vec::new();
        pack.take(object.len)
            .read_to_end(&mut bytes)
            .into_diagnostic()?;
        if bytes.len() as u64 != object.len || digest(&bytes) != object.source {
            return Err(miette!("snapshot member CID mismatch"));
        }
        let mut file = tempfile::NamedTempFile::new_in(&dir).into_diagnostic()?;
        std::io::copy(&mut GzDecoder::new(bytes.as_slice()), file.as_file_mut())
            .into_diagnostic()
            .wrap_err("decode group snapshot")?;
        file.as_file()
            .set_permissions(fs::Permissions::from_mode(0o444))
            .into_diagnostic()?;
        file.persist(&path).into_diagnostic()?;
    }
    Ok(path)
}

fn append_snapshot(pack: &mut Vec<u8>, path: &Path) -> Result<Snapshot> {
    let mut packed = GzEncoder::new(Vec::new(), Compression::fast());
    std::io::copy(&mut fs::File::open(path).into_diagnostic()?, &mut packed).into_diagnostic()?;
    let bytes = packed.finish().into_diagnostic()?;
    let len = bytes.len() as u64;
    pack.extend_from_slice(&len.to_le_bytes());
    let offset = pack.len() as u64;
    pack.extend_from_slice(&bytes);
    Ok(Snapshot {
        pack: String::new(),
        offset,
        len,
        source: digest(&bytes),
    })
}

fn flush_pack(cas: &dyn Cas, pack: &mut Vec<u8>, groups: &mut [Group]) -> Result<usize> {
    if pack.is_empty() {
        return Ok(0);
    }
    let bytes = std::mem::take(pack);
    let size = bytes.len();
    let id = cas.put(bytes.into()).into_diagnostic()?.to_string();
    for group in groups
        .iter_mut()
        .rev()
        .take_while(|g| g.snapshot.pack.is_empty())
    {
        group.snapshot.pack = id.clone();
    }
    Ok(size)
}

struct Bridge {
    child: Child,
    output: BufReader<std::process::ChildStdout>,
    diagnostics: tempfile::NamedTempFile,
    input: tempfile::TempDir,
    workspace: PathBuf,
}

impl Bridge {
    fn read(&mut self) -> Result<Value> {
        let mut line = String::new();
        self.output.read_line(&mut line).into_diagnostic()?;
        if line.is_empty() {
            let status = self.child.wait().into_diagnostic()?;
            return Err(miette!(
                "proof-group bridge ended before its result ({status}):\n{}",
                fs::read_to_string(self.diagnostics.path()).into_diagnostic()?
            ));
        }
        serde_json::from_str(&line)
            .into_diagnostic()
            .wrap_err("invalid group bridge response")
    }
    fn send(&mut self, message: Value) -> Result<()> {
        let input = self
            .child
            .stdin
            .as_mut()
            .ok_or_else(|| miette!("closed group bridge input"))?;
        writeln!(input, "{message}").into_diagnostic()?;
        input.flush().into_diagnostic()
    }
    fn finish(&mut self) -> Result<()> {
        let status = self.child.wait().into_diagnostic()?;
        let diagnostics = fs::read_to_string(self.diagnostics.path()).into_diagnostic()?;
        if !status.success() {
            return Err(miette!(
                "proof-group bridge failed ({status}):\n{diagnostics}"
            ));
        }
        std::io::stderr()
            .write_all(diagnostics.as_bytes())
            .into_diagnostic()
    }
}

impl Drop for Bridge {
    fn drop(&mut self) {
        // Protocol failures must not leave a compiler waiting for an answer.
        if let Err(error) = self.child.kill()
            && error.kind() != std::io::ErrorKind::InvalidInput
        {
            eprintln!("stop group bridge: {error}");
        }
        if let Err(error) = self.child.wait() {
            eprintln!("reap group bridge: {error}");
        }
    }
}

fn start_bridge(
    args: &ProofGroups,
    source: &[u8],
    baseline: bool,
    trace: &mut trace::Trace,
) -> Result<(Bridge, usize)> {
    let workspace = args.storage.workspace.canonicalize().into_diagnostic()?;
    let meta = workspace
        .join("environments")
        .join(args.environment.to_string());
    let bytes = read(&meta.with_extension("json"))?;
    if Blake3::of(&bytes) != args.environment {
        return Err(miette!("local environment CID mismatch"));
    }
    let env = decode_environment(&bytes)?;
    if !env.proof_groups {
        return Err(miette!("prepare with --proof-groups first"));
    }
    let toolchain = object_path(&workspace, &env.toolchain)?;
    let input = tempfile::tempdir().into_diagnostic()?;
    let source_file = input.path().join("source.lean");
    fs::write(&source_file, source).into_diagnostic()?;
    // Replacement receipts never enter the full-source success namespace.
    if baseline {
        run_lean(&toolchain, &meta, input.path(), &env)?;
        trace.event(|| {
            json!({"operation":"source_check", "identity":{"module":env.module_path},
            "input_fingerprint":digest(source), "result_fingerprint":null,
            "outcome":"checked", "reason":"baseline_validation"})
        })?;
    }
    let diagnostics = tempfile::NamedTempFile::new().into_diagnostic()?;
    let mut child = Command::new(toolchain.join("bin/mor-proof-groups"))
        // Closure relocation requires the same loader name on every worker.
        .current_dir(&toolchain)
        .env_clear()
        .env("LD_LIBRARY_PATH", "lib/lean")
        .env("LEAN_SYSROOT", &toolchain)
        .env("PATH", toolchain.join("bin"))
        .arg(meta.with_extension("setup.json"))
        .arg(&source_file)
        .arg(&env.module_path)
        .arg(input.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(diagnostics.as_file().try_clone().into_diagnostic()?)
        .spawn()
        .into_diagnostic()?;
    let output = BufReader::new(
        child
            .stdout
            .take()
            .ok_or_else(|| miette!("missing bridge stdout"))?,
    );
    let mut bridge = Bridge {
        child,
        output,
        diagnostics,
        input,
        workspace,
    };
    let response = bridge.read()?;
    if response["protocol"] != "mor/proof-groups/v3" {
        return Err(miette!(
            "outdated group bridge; prepare a new proof-group environment"
        ));
    }
    let header = response["headerEnd"]
        .as_u64()
        .ok_or_else(|| miette!("missing group header"))? as usize;
    bridge.send(json!({"mode": if baseline {"baseline"} else {"replace"}}))?;
    trace.event(|| {
        json!({"operation":"bridge_start", "identity":{"module":env.module_path},
        "input_fingerprint":digest(source), "result_fingerprint":null,
        "outcome":"started", "reason":if baseline {"baseline"} else {"changed_group"}})
    })?;
    Ok((bridge, header))
}

fn group_key(index: usize) -> Vec<u8> {
    (index as u64).to_be_bytes().to_vec()
}

fn parsed(event: Value, index: usize, start: usize, source: &[u8]) -> Result<(ParsedGroup, Span)> {
    let group: ParsedGroup = serde_json::from_value(event).into_diagnostic()?;
    if group.index != index || group.start != start || group.stop <= start {
        return Err(miette!("invalid group ordering or range"));
    }
    let bytes = source
        .get(start..group.stop)
        .ok_or_else(|| miette!("invalid group range"))?;
    Ok((group, Span::new(bytes)))
}

#[derive(Default)]
struct CheckStats {
    baseline: bool,
    exact: bool,
    hits: usize,
    checked: usize,
    snapshot_bytes: usize,
    parsed: usize,
    loaded: usize,
}

fn report_revision(id: String, revision: &Revision, groups: &[Group], stats: CheckStats) {
    let total: usize = groups.iter().filter(|g| g.proofs).map(|g| g.count).sum();
    println!(
        "{}",
        json!({"status": if stats.baseline {"baseline_checked"} else {"proof_replacements_verified"},
        "exact_hit":stats.exact, "baseline":revision.baseline,"proof_cache":id,"index_root":revision.root,
        "group_hits":stats.hits,"group_misses":groups.len()-stats.hits,"reused_declarations":total-stats.checked,
        "checked_declarations":stats.checked,"new_snapshot_bytes":stats.snapshot_bytes,
        "parsed_groups":stats.parsed,"loaded_snapshots":stats.loaded})
    );
}

pub(super) fn run(args: ProofGroups) -> Result<()> {
    let mut trace = trace::Trace::open(args.trace.as_deref())?;
    let source = read(&args.file)?;
    std::str::from_utf8(&source)
        .into_diagnostic()
        .wrap_err("Lean source is not UTF-8")?;
    let source_hash = digest(&source);
    let cas: Arc<dyn Cas> = Arc::from(store(&args.storage.cache)?);
    let previous: Option<Revision> = args
        .previous_proofs
        .map(|id| get(&*cas, &id.to_string()))
        .transpose()?;
    if previous
        .as_ref()
        .is_some_and(|r| r.kind != "mor/proof-groups/revision/v2")
    {
        return Err(miette!(
            "unsupported proof-group revision; create a new baseline"
        ));
    }
    let old: Option<Baseline> = previous
        .as_ref()
        .map(|r| get(&*cas, &r.baseline))
        .transpose()?;
    if old.as_ref().is_some_and(|b| {
        b.kind != "mor/proof-groups/baseline/v2"
            || b.environment != args.environment.to_string()
            || b.checker != checker()
    }) {
        return Err(miette!(
            "proof groups belong to a different environment or checker"
        ));
    }
    if let (Some(revision), Some(baseline)) = (&previous, &old) {
        if revision.source == source_hash {
            trace.event(|| json!({"operation":"proof_revision", "identity":{"baseline":revision.baseline},
                "input_fingerprint":source_hash, "result_fingerprint":args.previous_proofs.unwrap().to_string(),
                "outcome":"reused", "reason":"exact_revision"}))?;
            report_revision(
                args.previous_proofs.unwrap().to_string(),
                revision,
                &baseline.groups,
                CheckStats {
                    exact: true,
                    hits: baseline.groups.len(),
                    ..CheckStats::default()
                },
            );
            return Ok(());
        }
        let history = Map::open(cas.clone(), revision.history.parse().into_diagnostic()?);
        if let Some(value) = history.get(source_hash.as_bytes()).into_diagnostic()? {
            let Dasl::String(root) = value else {
                return Err(miette!("invalid revision index"));
            };
            let known = Revision {
                kind: revision.kind.clone(),
                baseline: revision.baseline.clone(),
                root,
                source: source_hash,
                history: revision.history.clone(),
            };
            let id = put(&*cas, &known)?;
            trace.event(|| {
                json!({"operation":"proof_revision", "identity":{"baseline":known.baseline},
                "input_fingerprint":known.source, "result_fingerprint":id,
                "outcome":"reused", "reason":"revision_history"})
            })?;
            report_revision(
                id,
                &known,
                &baseline.groups,
                CheckStats {
                    exact: true,
                    hits: baseline.groups.len(),
                    ..CheckStats::default()
                },
            );
            return Ok(());
        }
    }
    let (baseline, map, history, stats) = if let (Some(revision), Some(baseline)) =
        (&previous, &old)
    {
        if !baseline.header.matches(&source, 0) {
            trace.event(|| {
                json!({"operation":"proof_revision", "identity":{"baseline":revision.baseline},
                    "input_fingerprint":source_hash, "result_fingerprint":null,
                    "outcome":"rejected", "reason":"imports_changed"})
            })?;
            return Err(miette!("imports changed; create a new baseline"));
        }
        let map = Map::open(cas.clone(), revision.root.parse().into_diagnostic()?);
        let mut entries = map.range(..).into_diagnostic()?;
        let mut bridge: Option<Bridge> = None;
        let mut loaded = 0;
        let mut end = baseline.header.len;
        let mut edits = Vec::new();
        let (mut hits, mut checked, mut parsed_count) = (0, 0, 0);
        for (index, group) in baseline.groups.iter().enumerate() {
            let (key, value) = entries
                .next()
                .transpose()
                .into_diagnostic()?
                .ok_or_else(|| miette!("incomplete group index"))?;
            if key.as_ref() != group_key(index) {
                return Err(miette!("invalid group index"));
            }
            let prior = Span::decode(value)?;
            // A hash hit reuses a compiler-established boundary in the original
            // context. A miss must ask Lean where the next boundary lies.
            let (span, hit, reason) = if prior.matches(&source, end) {
                (prior.clone(), true, "current_span")
            } else if group.source.matches(&source, end) {
                (
                    Span {
                        source: group.source.source.clone(),
                        len: group.source.len,
                    },
                    true,
                    "baseline_span",
                )
            } else {
                if !group.proofs {
                    trace.event(|| json!({"operation":"proof_group", "identity":{"baseline":revision.baseline,"index":index},
                            "input_fingerprint":source_hash, "result_fingerprint":null,
                            "outcome":"rejected", "reason":"nonproof_context_changed"}))?;
                    return Err(miette!(
                        "interface or context changed; create a new baseline"
                    ));
                }
                if bridge.is_none() {
                    let (child, header) = start_bridge(&args, &source, false, &mut trace)?;
                    if header != baseline.header.len {
                        return Err(miette!("imports changed"));
                    }
                    bridge = Some(child);
                }
                let child = bridge
                    .as_mut()
                    .ok_or_else(|| miette!("missing group bridge"))?;
                let paths: Vec<PathBuf> = baseline.groups[loaded..index]
                    .iter()
                    .map(|g| snapshot(&*cas, &child.workspace, &g.snapshot))
                    .collect::<Result<_>>()?;
                child.send(
                    json!({"index":index,"start":end,"leading":group.leading,"loadBefore":paths}),
                )?;
                let (event, span) = parsed(child.read()?, index, end, &source)?;
                parsed_count += 1;
                trace.event(|| json!({"operation":"group_parse", "identity":{"baseline":revision.baseline,"index":index},
                        "input_fingerprint":span.source, "result_fingerprint":digest(event.signature.as_bytes()),
                        "outcome":"executed", "reason":"source_changed"}))?;
                for (before, saved) in baseline.groups[loaded..index].iter().enumerate() {
                    trace.event(|| json!({"operation":"snapshot_load", "identity":{"baseline":revision.baseline,"index":loaded+before},
                            "input_fingerprint":saved.snapshot.source, "result_fingerprint":saved.snapshot.source,
                            "outcome":"loaded", "reason":"original_context"}))?;
                }
                if digest(event.signature.as_bytes()) != group.signature
                    || event.count != group.count
                    || event.proofs != group.proofs
                    || event.leading != group.leading
                {
                    trace.event(|| json!({"operation":"proof_group", "identity":{"baseline":revision.baseline,"index":index},
                            "input_fingerprint":span.source, "result_fingerprint":digest(event.signature.as_bytes()),
                            "outcome":"rejected", "reason":"interface_changed"}))?;
                    return Err(miette!(
                        "group {index} interface or context changed; create a new baseline"
                    ));
                }
                let receipt = serde_json::to_vec(&json!({"kind":"mor/proof-group-success/v2",
                    "baseline":revision.baseline,"index":index,"source":span.source}))
                .into_diagnostic()?;
                let cid: Cid = Blake3::of(&receipt).into();
                let hit = cas.get_fact(&cid).into_diagnostic()?.is_some();
                let path = snapshot(&*cas, &child.workspace, &group.snapshot)?;
                child.send(json!({"action":if hit {"skip"} else {"check"},"snapshot":path}))?;
                if child.read()?["saved"].as_u64() != Some(index as u64) {
                    return Err(miette!("group was not checked"));
                }
                trace.event(|| json!({"operation":"snapshot_load", "identity":{"baseline":revision.baseline,"index":index},
                        "input_fingerprint":group.snapshot.source, "result_fingerprint":group.snapshot.source,
                        "outcome":"loaded", "reason":"restore_original_context"}))?;
                loaded = index + 1;
                if !hit {
                    cas.put(receipt.into()).into_diagnostic()?;
                }
                (
                    span,
                    hit,
                    if hit {
                        "proof_receipt"
                    } else {
                        "source_changed"
                    },
                )
            };
            // This result fingerprint is the parsed interface/context signature.
            // Group identities remain ordinal within this baseline, not stable declaration IDs.
            trace.event(|| json!({"operation":"proof_group", "identity":{"baseline":revision.baseline,"index":index},
                    "input_fingerprint":span.source, "result_fingerprint":group.signature,
                    "outcome":if hit {"reused"} else {"checked"}, "reason":reason}))?;
            end = end
                .checked_add(span.len)
                .ok_or_else(|| miette!("invalid source length"))?;
            if hit {
                hits += 1;
            } else {
                checked += group.count;
            }
            if span != prior {
                edits.push(Edit::Insert(key, span.value()?));
            }
        }
        if entries.next().transpose().into_diagnostic()?.is_some() {
            return Err(miette!("extra group index entries"));
        }
        if end.checked_add(baseline.tail.len) != Some(source.len())
            || !baseline.tail.matches(&source, end)
        {
            trace.event(|| {
                json!({"operation":"proof_revision", "identity":{"baseline":revision.baseline},
                    "input_fingerprint":source_hash, "result_fingerprint":null,
                    "outcome":"rejected", "reason":"tail_changed"})
            })?;
            return Err(miette!(
                "declarations removed or file tail changed; create a new baseline"
            ));
        }
        if let Some(mut child) = bridge {
            child.send(json!({"done":true}))?;
            child.finish()?;
        }
        let map = map.apply(edits).into_diagnostic()?;
        let history = Map::open(cas.clone(), revision.history.parse().into_diagnostic()?);
        (
            revision.baseline.clone(),
            map,
            history,
            CheckStats {
                hits,
                checked,
                parsed: parsed_count,
                loaded,
                ..CheckStats::default()
            },
        )
    } else {
        let (mut bridge, header_end) = start_bridge(&args, &source, true, &mut trace)?;
        let header = Span::new(
            source
                .get(..header_end)
                .ok_or_else(|| miette!("invalid header range"))?,
        );
        let mut end = header_end;
        let mut groups = Vec::new();
        let mut pack = Vec::new();
        let mut edits = Vec::new();
        let (mut bytes, mut checked) = (0, 0);
        loop {
            let event = bridge.read()?;
            if event["done"] == true {
                if event["groups"].as_u64() != Some(groups.len() as u64) {
                    return Err(miette!("incomplete group result"));
                }
                break;
            }
            let index = groups.len();
            let (parsed, span) = parsed(event, index, end, &source)?;
            end = parsed.stop;
            trace.event(|| json!({"operation":"group_parse", "identity":{"baseline_source":source_hash,"index":index},
                    "input_fingerprint":span.source, "result_fingerprint":digest(parsed.signature.as_bytes()),
                    "outcome":"executed", "reason":"baseline"}))?;
            bridge.send(json!({"action":"check"}))?;
            if bridge.read()?["saved"].as_u64() != Some(index as u64) {
                return Err(miette!("group was not checked"));
            }
            trace.event(|| json!({"operation":"proof_group", "identity":{"baseline_source":source_hash,"index":index},
                    "input_fingerprint":span.source, "result_fingerprint":digest(parsed.signature.as_bytes()),
                    "outcome":"checked", "reason":"baseline"}))?;
            let snapshot = append_snapshot(
                &mut pack,
                &bridge.input.path().join(format!("{index}.snap")),
            )?;
            checked += if parsed.proofs { parsed.count } else { 0 };
            edits.push(Edit::Insert(group_key(index).into(), span.value()?));
            groups.push(Group {
                signature: digest(parsed.signature.as_bytes()),
                source: span,
                snapshot,
                count: parsed.count,
                proofs: parsed.proofs,
                leading: parsed.leading,
            });
            if groups.len() % 32 == 0 {
                bytes += flush_pack(&*cas, &mut pack, &mut groups)?;
            }
        }
        bridge.finish()?;
        bytes += flush_pack(&*cas, &mut pack, &mut groups)?;
        let baseline = Baseline {
            kind: "mor/proof-groups/baseline/v2".to_owned(),
            environment: args.environment.to_string(),
            checker: checker(),
            header,
            tail: Span::new(&source[end..]),
            groups,
        };
        let count = baseline.groups.len();
        let id = put(&*cas, &baseline)?;
        let map = Map::empty(cas.clone())
            .into_diagnostic()?
            .apply(edits)
            .into_diagnostic()?;
        let history = Map::empty(cas.clone()).into_diagnostic()?;
        (
            id,
            map,
            history,
            CheckStats {
                baseline: true,
                checked,
                snapshot_bytes: bytes,
                parsed: count,
                ..CheckStats::default()
            },
        )
    };
    let history = history
        .insert(
            source_hash.as_bytes().to_vec().into(),
            Dasl::String(map.root().to_string()),
        )
        .into_diagnostic()?;
    let revision = Revision {
        kind: "mor/proof-groups/revision/v2".to_owned(),
        baseline,
        root: map.root().to_string(),
        source: source_hash,
        history: history.root().to_string(),
    };
    let id = put(&*cas, &revision)?;
    let baseline: Baseline = get(&*cas, &revision.baseline)?;
    report_revision(id, &revision, &baseline.groups, stats);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use covalence_cas_trait::Put;

    #[test]
    fn corrupt_snapshot_is_rejected_before_publication() {
        let dir = tempfile::tempdir().unwrap();
        let cas = SqliteCas::open(dir.path().join("cas.db")).unwrap();
        let cid: Cid = cas.put(b"not a gzip snapshot".to_vec().into()).unwrap();
        let object = Snapshot {
            pack: cid.to_string(),
            offset: 0,
            len: 19,
            source: cid.to_string(),
        };
        let error = snapshot(&cas, dir.path(), &object).unwrap_err();
        assert!(error.to_string().contains("decode group snapshot"));
        assert!(
            !dir.path()
                .join("group-snapshots")
                .join(cid.to_string())
                .exists()
        );
        let missing: Cid = Blake3::of(b"absent").into();
        assert!(
            snapshot(
                &cas,
                dir.path(),
                &Snapshot {
                    pack: missing.to_string(),
                    offset: 0,
                    len: 1,
                    source: missing.to_string()
                }
            )
            .unwrap_err()
            .to_string()
            .contains("missing group snapshot")
        );
    }

    #[test]
    fn snapshot_pack_rejects_wrong_member_and_out_of_bounds_range() {
        let dir = tempfile::tempdir().unwrap();
        let cas = SqliteCas::open(dir.path().join("cas.db")).unwrap();
        let id: Cid = cas.put(b"member".to_vec().into()).unwrap();
        let source = Blake3::of(b"another member").to_string();
        for (offset, len, error) in [
            (0, 6, "member CID mismatch"),
            (4, 8, "range exceeds pack"),
            (u64::MAX, 2, "range exceeds pack"),
        ] {
            let object = Snapshot {
                pack: id.to_string(),
                offset,
                len,
                source: source.clone(),
            };
            assert!(
                snapshot(&cas, dir.path(), &object)
                    .unwrap_err()
                    .to_string()
                    .contains(error)
            );
            assert!(!dir.path().join("group-snapshots").join(&source).exists());
        }
    }

    #[test]
    fn revision_from_another_context_is_rejected_even_for_exact_source() {
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join("cas.db");
        let cas = SqliteCas::open(&cache).unwrap();
        let file = dir.path().join("Proof.lean");
        fs::write(&file, "theorem p : True := True.intro\n").unwrap();
        let environment = Blake3::of(b"environment");
        let root = Map::empty(Arc::new(SqliteCas::open(&cache).unwrap()))
            .unwrap()
            .root()
            .to_string();
        for (bound_environment, bound_checker) in [
            (Blake3::of(b"other environment").to_string(), checker()),
            (environment.to_string(), "other checker".to_owned()),
        ] {
            let baseline = put(
                &cas,
                &Baseline {
                    kind: "mor/proof-groups/baseline/v2".into(),
                    environment: bound_environment,
                    checker: bound_checker,
                    header: Span::new(b""),
                    tail: Span::new(b""),
                    groups: vec![],
                },
            )
            .unwrap();
            let revision = put(
                &cas,
                &Revision {
                    kind: "mor/proof-groups/revision/v2".into(),
                    baseline,
                    root: root.clone(),
                    source: digest(&read(&file).unwrap()),
                    history: Map::empty(Arc::new(SqliteCas::open(&cache).unwrap()))
                        .unwrap()
                        .root()
                        .to_string(),
                },
            )
            .unwrap();
            let error = run(ProofGroups {
                file: file.clone(),
                environment,
                previous_proofs: Some(revision.parse().unwrap()),
                trace: None,
                storage: Storage {
                    cache: cache.to_string_lossy().into_owned(),
                    workspace: dir.path().join("absent"),
                },
            })
            .unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains("different environment or checker")
            );
        }
    }
}

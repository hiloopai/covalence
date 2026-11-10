//! Packed transport for successful replay receipts. S3 write access is trust.
//!
//! Git revisions locate candidates; only the caller's ledger and the checker's
//! complete input fingerprints authorize reuse. No native working state travels.
use covalence_cas_prolly::{Edit, Map};
use covalence_cas_sqlite::SqliteCas;
use covalence_cas_trait::{Cas, GetExt};
use covalence_kv_s3::{AwsOptions, S3Kv};
use covalence_kv_trait::Kv;
use covalence_lib_cid::Cid;
use covalence_lib_cli::clap::{self, Args as ClapArgs, Subcommand};
use covalence_lib_dasl::Value as Dasl;
use covalence_lib_error::miette::{IntoDiagnostic, Result, WrapErr, miette};
use covalence_lib_serde::{Deserialize, Serialize};
use flate2::{Compression, read::GzDecoder, write::GzEncoder};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{BufRead, BufReader, Read, Seek, Write},
    path::{Path, PathBuf},
    sync::Arc,
    time::Instant,
};

const FORMAT: &str = "mor/receipt-pack/v1";
const REPLAY: &str = "mor/kernel-replay/v1";
const MAX_PACK: u64 = 512 * 1024 * 1024;
const MAX_EXPANDED: u64 = 2 * 1024 * 1024 * 1024;
const MAX_RECORD: u64 = 64 * 1024 * 1024;

#[derive(ClapArgs)]
pub struct Args {
    /// Dedicated trusted S3 prefix, protected by IAM (never a PR-writable cache)
    #[arg(long)]
    remote: String,
    /// Caller/checker/policy fingerprint; revisions are discovery hints only
    #[arg(long)]
    scope: String,
    #[arg(long, default_value = ".lake/declaration-replay")]
    state: PathBuf,
    /// Successful module ledger to publish, or a separate downloaded ledger
    #[arg(long)]
    ledger: PathBuf,
    /// Current module input rows; hydrate proof maps only for shared-ledger misses
    #[arg(long)]
    current_inputs: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Import the first available candidate; preserve existing local proof pointers
    Pull {
        #[arg(long, required = true)]
        revision: Vec<String>,
    },
    /// Publish after all checks succeed; index publication is atomic and last
    Push {
        #[arg(long)]
        revision: String,
    },
}

#[derive(Serialize, Deserialize)]
#[serde(crate = "covalence_lib_serde::serde")]
#[serde(deny_unknown_fields)]
struct Index {
    format: String,
    scope: String,
    pack: String,
    bytes: u64,
    ledger: String,
    ledger_hash: String,
}

#[derive(Serialize, Deserialize)]
#[serde(crate = "covalence_lib_serde::serde")]
#[serde(deny_unknown_fields)]
struct Header {
    format: String,
    scope: String,
    ledger: String,
}

// Field order matches the checker's manifest encoding, hence its CAS identity.
#[derive(Serialize, Deserialize)]
#[serde(crate = "covalence_lib_serde::serde")]
#[serde(deny_unknown_fields)]
struct Manifest {
    kind: String,
    context: String,
    root: String,
}

#[derive(Serialize, Deserialize)]
#[serde(crate = "covalence_lib_serde::serde")]
#[serde(deny_unknown_fields)]
struct Module {
    name: String,
    receipt: String,
    manifest: Manifest,
    entries: BTreeMap<String, String>,
}

fn hex(value: &str, lengths: &[usize]) -> Result<()> {
    if !lengths.contains(&value.len())
        || !value
            .bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
    {
        return Err(miette!("invalid cache digest/revision {value:?}"));
    }
    Ok(())
}

fn relative(name: &str) -> Result<PathBuf> {
    if name.is_empty()
        || name.split('.').any(|part| {
            part.is_empty()
                || !part
                    .chars()
                    .all(|c| c.is_alphanumeric() || c == '_' || c == '\'')
        })
    {
        return Err(miette!("invalid receipt module name {name:?}"));
    }
    Ok(PathBuf::from(name.replace('.', "/")))
}

fn line(out: &mut impl Write, value: &impl Serialize) -> Result<()> {
    serde_json::to_writer(&mut *out, value).into_diagnostic()?;
    out.write_all(b"\n").into_diagnostic()
}

fn atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| miette!("missing output parent"))?;
    fs::create_dir_all(parent).into_diagnostic()?;
    let mut pending = tempfile::NamedTempFile::new_in(parent).into_diagnostic()?;
    pending.write_all(bytes).into_diagnostic()?;
    pending.persist(path).into_diagnostic()?;
    Ok(())
}

fn export_module(state: &Path, name: &str) -> Result<Option<Module>> {
    let relative = relative(name)?;
    let pointer = state.join("modules").join(&relative).with_extension("json");
    let bytes = match fs::read(pointer) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).into_diagnostic(),
    };
    let pointer: Value = serde_json::from_slice(&bytes).into_diagnostic()?;
    let receipt = pointer["replay_cache"]
        .as_str()
        .ok_or_else(|| miette!("invalid local receipt"))?;
    let id: Cid = receipt.parse().into_diagnostic()?;
    let path = state.join("cas").join(&relative).join("cache.db");
    if !path.is_file() {
        return Err(miette!("missing local receipt CAS {}", path.display()));
    }
    let cas: Arc<dyn Cas> = Arc::new(SqliteCas::open(path).into_diagnostic()?);
    let fact = cas
        .get_fact(&id)
        .into_diagnostic()?
        .ok_or_else(|| miette!("missing manifest {id}"))?;
    let manifest: Manifest = serde_json::from_slice(fact.bytes()).into_diagnostic()?;
    if manifest.kind != REPLAY {
        return Err(miette!("not a successful replay manifest"));
    }
    let map = Map::open(cas, manifest.root.parse().into_diagnostic()?);
    let mut entries = BTreeMap::new();
    for entry in map.range(..).into_diagnostic()? {
        let (key, value) = entry.into_diagnostic()?;
        let Dasl::String(query) = value else {
            return Err(miette!("invalid replay query"));
        };
        entries.insert(String::from_utf8(key.to_vec()).into_diagnostic()?, query);
    }
    Ok(Some(Module {
        name: name.into(),
        receipt: receipt.into(),
        manifest,
        entries,
    }))
}

fn import_module(state: &Path, module: &Module) -> Result<()> {
    let relative = relative(&module.name)?;
    if module.manifest.kind != REPLAY {
        return Err(miette!("not a successful replay manifest"));
    }
    let directory = state.join("cas").join(&relative);
    fs::create_dir_all(&directory).into_diagnostic()?;
    let cas: Arc<dyn Cas> =
        Arc::new(SqliteCas::open(directory.join("cache.db")).into_diagnostic()?);
    let map = Map::empty(cas.clone())
        .into_diagnostic()?
        .apply(
            module
                .entries
                .iter()
                .map(|(k, v)| Edit::Insert(k.as_bytes().to_vec().into(), Dasl::String(v.clone()))),
        )
        .into_diagnostic()?;
    if map.root().to_string() != module.manifest.root {
        return Err(miette!("receipt map root mismatch for {}", module.name));
    }
    let id = cas
        .put(
            serde_json::to_vec(&module.manifest)
                .into_diagnostic()?
                .into(),
        )
        .into_diagnostic()?;
    if id.to_string() != module.receipt {
        return Err(miette!("receipt manifest mismatch for {}", module.name));
    }
    let pointer = state.join("modules").join(relative).with_extension("json");
    if !pointer.exists() {
        atomic(
            &pointer,
            &serde_json::to_vec(&json!({"replay_cache":module.receipt})).into_diagnostic()?,
        )?;
    }
    Ok(())
}

fn digest(file: &mut fs::File) -> Result<String> {
    file.rewind().into_diagnostic()?;
    let mut hash = blake3::Hasher::new();
    hash.update_reader(&mut *file).into_diagnostic()?;
    file.rewind().into_diagnostic()?;
    Ok(hash.finalize().to_hex().to_string())
}

fn index_key(scope: &str, revision: &str) -> Result<String> {
    hex(scope, &[64])?;
    hex(revision, &[40, 64])?;
    Ok(format!("indexes/{scope}/{revision}.json"))
}

fn publish(kv: &dyn Kv, args: &Args, revision: &str) -> Result<Value> {
    let key = index_key(&args.scope, revision)?;
    let ledger = fs::read_to_string(&args.ledger).into_diagnostic()?;
    // A reader imports only changed modules. Carry unchanged proof maps forward
    // before publishing the next snapshot, preserving all new local pointers.
    let parent = args
        .state
        .join("shared")
        .join(format!("{}.json", args.scope));
    let mut missing = false;
    for row in ledger.lines().skip(1) {
        let name = row
            .split('\t')
            .next()
            .ok_or_else(|| miette!("invalid module ledger"))?;
        if !args
            .state
            .join("modules")
            .join(relative(name)?)
            .with_extension("json")
            .is_file()
        {
            missing = true;
        }
    }
    if missing && parent.is_file() {
        let index: Index =
            serde_json::from_slice(&fs::read(parent).into_diagnostic()?).into_diagnostic()?;
        validate_index(&index, &args.scope)?;
        let scratch = tempfile::tempdir().into_diagnostic()?;
        let previous = Args {
            remote: args.remote.clone(),
            scope: args.scope.clone(),
            state: args.state.clone(),
            ledger: scratch.path().join("ledger"),
            current_inputs: None,
            command: Command::Pull { revision: vec![] },
        };
        restore_pack(kv, &previous, &index)?;
    }

    let header = Header {
        format: FORMAT.into(),
        scope: args.scope.clone(),
        ledger,
    };
    let mut pack = tempfile::tempfile().into_diagnostic()?;
    let mut gzip = GzEncoder::new(&mut pack, Compression::fast());
    line(&mut gzip, &header)?;
    let mut count = 0;
    for row in header.ledger.lines().skip(1) {
        let name = row
            .split('\t')
            .next()
            .ok_or_else(|| miette!("invalid module ledger"))?;
        if let Some(module) = export_module(&args.state, name)? {
            line(&mut gzip, &module)?;
            count += 1;
        }
    }
    gzip.finish().into_diagnostic()?;
    let bytes = pack.metadata().into_diagnostic()?.len();
    if bytes > MAX_PACK {
        return Err(miette!("receipt pack exceeds {MAX_PACK} bytes"));
    }
    let hash = digest(&mut pack)?;
    let blob = format!("packs/{hash}");
    // No index can point at an incomplete pack, even after an interrupted upload.
    kv.put_new_from(blob.as_bytes(), &mut pack, bytes)
        .into_diagnostic()?;
    let index = Index {
        format: FORMAT.into(),
        scope: args.scope.clone(),
        pack: hash,
        bytes,
        ledger_hash: blake3::hash(header.ledger.as_bytes()).to_hex().to_string(),
        ledger: header.ledger,
    };
    let created = kv
        .put_new(
            key.as_bytes(),
            serde_json::to_vec(&index).into_diagnostic()?.into(),
        )
        .into_diagnostic()?;
    Ok(
        json!({"status":"published", "new_index":created, "modules":count, "bytes":bytes, "pack":index.pack}),
    )
}

fn read_line(input: &mut impl BufRead) -> Result<Option<Vec<u8>>> {
    let mut line = Vec::new();
    input
        .take(MAX_RECORD + 1)
        .read_until(b'\n', &mut line)
        .into_diagnostic()?;
    if line.len() as u64 > MAX_RECORD {
        return Err(miette!("receipt record too large"));
    }
    if line.is_empty() {
        return Ok(None);
    }
    if line.last() != Some(&b'\n') {
        return Err(miette!("truncated receipt pack"));
    }
    Ok(Some(line))
}

fn needed(args: &Args, ledger: &str) -> Result<Option<BTreeSet<String>>> {
    let Some(path) = &args.current_inputs else {
        return Ok(None);
    };
    let current = fs::read_to_string(path).into_diagnostic()?;
    let saved: BTreeSet<_> = ledger.lines().skip(1).collect();
    let same_policy = current.lines().next() == ledger.lines().next();
    current
        .lines()
        .skip(1)
        .filter(|row| !same_policy || !saved.contains(row))
        .map(|row| {
            let name = row
                .split('\t')
                .next()
                .ok_or_else(|| miette!("invalid input row"))?;
            relative(name)?;
            Ok(name.to_owned())
        })
        .collect::<Result<BTreeSet<_>>>()
        .map(Some)
}

fn unpack(file: &mut fs::File, args: &Args) -> Result<usize> {
    let mut input = BufReader::new(GzDecoder::new(file).take(MAX_EXPANDED + 1));
    let bytes = read_line(&mut input)?.ok_or_else(|| miette!("empty receipt pack"))?;
    let header: Header = serde_json::from_slice(&bytes).into_diagnostic()?;
    if header.format != FORMAT || header.scope != args.scope {
        return Err(miette!("receipt pack scope/format mismatch"));
    }
    let needed = needed(args, &header.ledger)?;
    // The complete pack was hash-checked before parsing. Only misses need local
    // proof maps: unchanged modules already have a trusted module-level receipt.
    let parent = args
        .state
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    fs::create_dir_all(parent).into_diagnostic()?;
    // Sticky disks may be on a different filesystem from the runner's /tmp.
    let staged = tempfile::tempdir_in(parent).into_diagnostic()?;
    let mut size = bytes.len() as u64;
    let mut names = BTreeSet::new();
    let mut imported = BTreeSet::new();
    while let Some(bytes) = read_line(&mut input)? {
        size += bytes.len() as u64;
        if size > MAX_EXPANDED {
            return Err(miette!("expanded receipt pack too large"));
        }
        let module: Module = serde_json::from_slice(&bytes).into_diagnostic()?;
        if !names.insert(module.name.clone()) {
            return Err(miette!("duplicate receipt module"));
        }
        relative(&module.name)?;
        if needed
            .as_ref()
            .is_none_or(|selected| selected.contains(&module.name))
            && !args
                .state
                .join("modules")
                .join(relative(&module.name)?)
                .with_extension("json")
                .exists()
        {
            import_module(staged.path(), &module)?;
            imported.insert(module.name);
        }
    }
    for name in &imported {
        let relative = relative(name)?;
        let pointer = args
            .state
            .join("modules")
            .join(&relative)
            .with_extension("json");
        if pointer.exists() {
            continue;
        }
        // Module paths nest (`A` and `A/B`), so a module owns only
        // the `cache.db` in its CAS directory; the subdirectories belong to others.
        let target = args.state.join("cas").join(&relative);
        if target.join("cache.db").exists() {
            let module = export_module(staged.path(), name)?
                .ok_or_else(|| miette!("missing staged receipt"))?;
            import_module(&args.state, &module)?;
        } else {
            fs::create_dir_all(&target).into_diagnostic()?;
            fs::rename(
                staged.path().join("cas").join(&relative).join("cache.db"),
                target.join("cache.db"),
            )
            .into_diagnostic()?;
            atomic(
                &pointer,
                &fs::read(
                    staged
                        .path()
                        .join("modules")
                        .join(&relative)
                        .with_extension("json"),
                )
                .into_diagnostic()?,
            )?;
        }
    }
    atomic(&args.ledger, header.ledger.as_bytes())?;
    Ok(imported.len())
}

fn validate_index(index: &Index, scope: &str) -> Result<()> {
    if index.format != FORMAT
        || index.scope != scope
        || index.bytes > MAX_PACK
        || blake3::hash(index.ledger.as_bytes()).to_hex().as_str() != index.ledger_hash
    {
        return Err(miette!("invalid receipt index"));
    }
    hex(&index.pack, &[64])
}

fn restore_pack(kv: &dyn Kv, args: &Args, index: &Index) -> Result<usize> {
    let mut pack = tempfile::tempfile().into_diagnostic()?;
    let size = kv
        .get_to(format!("packs/{}", index.pack).as_bytes(), &mut pack)
        .into_diagnostic()?;
    if size != Some(index.bytes) || digest(&mut pack)? != index.pack {
        return Err(miette!("missing or corrupt receipt pack"));
    }
    unpack(&mut pack, args)
}

fn pull(kv: &dyn Kv, args: &Args, revisions: &[String]) -> Result<Value> {
    for revision in revisions {
        let key = index_key(&args.scope, revision)?;
        let Some(bytes) = kv.get(key.as_bytes()).into_diagnostic()? else {
            continue;
        };
        let index: Index = serde_json::from_slice(&bytes).into_diagnostic()?;
        validate_index(&index, &args.scope)?;
        let parent = args
            .state
            .join("shared")
            .join(format!("{}.json", args.scope));
        let ledger_only = needed(args, &index.ledger)?.is_some_and(|modules| modules.is_empty());
        let modules = if ledger_only {
            atomic(&args.ledger, index.ledger.as_bytes())?;
            0
        } else {
            restore_pack(kv, args, &index)?
        };
        atomic(&parent, &bytes)?;
        return Ok(
            json!({"status":"restored", "revision":revision, "bytes":bytes.len() as u64 + if ledger_only {0} else {index.bytes}, "modules":modules, "ledger_only":ledger_only}),
        );
    }
    Ok(json!({"status":"miss"}))
}

pub fn run(args: Args) -> Result<()> {
    let start = Instant::now();
    let kv = S3Kv::from_aws(
        args.remote.parse().into_diagnostic()?,
        &AwsOptions::default(),
    )
    .into_diagnostic()?;
    let mut report = match &args.command {
        Command::Pull { revision } => pull(&kv, &args, revision),
        Command::Push { revision } => publish(&kv, &args, revision),
    }
    .wrap_err("shared receipt cache")?;
    report["seconds"] = json!(start.elapsed().as_secs_f64());
    println!("{report}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> Module {
        let cas: Arc<dyn Cas> = Arc::new(SqliteCas::open_in_memory().unwrap());
        let entries: BTreeMap<String, String> = (0..30_000)
            .map(|n| (format!("proof{n}"), format!("query{n}")))
            .collect();
        let map =
            Map::empty(cas)
                .unwrap()
                .apply(entries.iter().map(|(k, v)| {
                    Edit::Insert(k.as_bytes().to_vec().into(), Dasl::String(v.clone()))
                }))
                .unwrap();
        let manifest = Manifest {
            kind: REPLAY.into(),
            context: "context".into(),
            root: map.root().to_string(),
        };
        let receipt =
            covalence_lib_cid::Blake3::of(&serde_json::to_vec(&manifest).unwrap()).to_string();
        Module {
            name: "Fixture.Proofs".into(),
            receipt,
            manifest,
            entries,
        }
    }

    fn args(root: &Path) -> Args {
        Args {
            remote: String::new(),
            scope: "a".repeat(64),
            state: root.join("state"),
            ledger: root.join("ledger"),
            current_inputs: None,
            command: Command::Pull { revision: vec![] },
        }
    }

    fn pack(args: &Args, modules: &[Module]) -> fs::File {
        let mut file = tempfile::tempfile().unwrap();
        let mut gzip = GzEncoder::new(&mut file, Compression::fast());
        line(
            &mut gzip,
            &Header {
                format: FORMAT.into(),
                scope: args.scope.clone(),
                ledger: "successful ledger\n".into(),
            },
        )
        .unwrap();
        for module in modules {
            line(&mut gzip, module).unwrap();
        }
        gzip.finish().unwrap();
        file.rewind().unwrap();
        file
    }

    #[test]
    fn thirty_thousand_entries_rebuild_identical_cas_and_keep_local_pointer() {
        let root = tempfile::tempdir().unwrap();
        let args = args(root.path());
        let module = fixture();
        let expected = module.receipt.clone();
        let mut file = pack(&args, &[module]);
        assert_eq!(unpack(&mut file, &args).unwrap(), 1);
        let exported = export_module(&args.state, "Fixture.Proofs")
            .unwrap()
            .unwrap();
        assert_eq!(exported.receipt, expected);
        assert_eq!(exported.entries.len(), 30_000);
        let pointer = args.state.join("modules/Fixture/Proofs.json");
        fs::write(&pointer, "existing local pointer").unwrap();
        file.rewind().unwrap();
        unpack(&mut file, &args).unwrap();
        assert_eq!(
            fs::read_to_string(pointer).unwrap(),
            "existing local pointer"
        );
    }

    #[test]
    fn nested_module_paths_restore_independently() {
        let root = tempfile::tempdir().unwrap();
        let args = args(root.path());
        let mut parent = fixture();
        parent.name = "Fixture".into();
        let child = fixture();
        let mut file = pack(&args, &[parent, child]);
        assert_eq!(unpack(&mut file, &args).unwrap(), 2);
        for name in ["Fixture", "Fixture.Proofs"] {
            let module = export_module(&args.state, name).unwrap().unwrap();
            assert_eq!(module.entries.len(), 30_000);
        }
    }

    #[test]
    fn corrupt_map_publishes_neither_pointers_nor_ledger() {
        let root = tempfile::tempdir().unwrap();
        let args = args(root.path());
        let good = fixture();
        let mut bad = fixture();
        bad.name = "Fixture.Corrupt".into();
        bad.entries.insert("proof0".into(), "forged".into());
        assert!(unpack(&mut pack(&args, &[good, bad]), &args).is_err());
        assert!(!args.ledger.exists());
        assert!(!args.state.exists());
    }

    #[test]
    fn paths_revisions_and_policy_are_checked_at_boundary() {
        for name in ["../outside", "Fixture..X", "/root", "", "a/b", "a\\b"] {
            assert!(relative(name).is_err());
        }
        assert!(index_key(&"a".repeat(64), &"b".repeat(40)).is_ok());
        assert!(index_key("../bad", &"b".repeat(40)).is_err());
        assert!(index_key(&"a".repeat(64), "main").is_err());
        let root = tempfile::tempdir().unwrap();
        let mut args = args(root.path());
        let mut file = pack(&args, &[]);
        args.scope = "b".repeat(64);
        assert!(unpack(&mut file, &args).is_err());
        assert!(!args.ledger.exists());
        assert!(read_line(&mut BufReader::new(b"truncated".as_slice())).is_err());
    }

    #[test]
    fn current_inputs_select_only_changed_modules_and_policy_changes_select_all() {
        let root = tempfile::tempdir().unwrap();
        let mut args = args(root.path());
        let path = root.path().join("inputs");
        args.current_inputs = Some(path.clone());
        let ledger = "policy\nA\ta\tout\nB\tb\tout\n";
        fs::write(&path, ledger).unwrap();
        assert!(needed(&args, ledger).unwrap().unwrap().is_empty());
        fs::write(&path, "policy\nA\tchanged\tout\nB\tb\tout\nC\tnew\tout\n").unwrap();
        assert_eq!(
            needed(&args, ledger).unwrap().unwrap(),
            BTreeSet::from(["A".into(), "C".into()])
        );
        assert_eq!(
            needed(&args, &ledger.replace("policy", "other-policy"))
                .unwrap()
                .unwrap()
                .len(),
            3
        );
        let mut index = Index {
            format: FORMAT.into(),
            scope: args.scope.clone(),
            pack: "a".repeat(64),
            bytes: 1,
            ledger: ledger.into(),
            ledger_hash: blake3::hash(ledger.as_bytes()).to_hex().to_string(),
        };
        validate_index(&index, &args.scope).unwrap();
        index.ledger.push_str("Forged\tinput\toutput\n");
        assert!(validate_index(&index, &args.scope).is_err());
    }
}

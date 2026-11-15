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

const FORMAT: &str = "mor/receipt-pack/v2";
/// What earlier checkers wrote: an index naming one pack of every module.
const FORMAT_V1: &str = "mor/receipt-pack/v1";
/// A push adds a pack of only the modules whose rows changed; an index naming
/// more than this many packs is written as one merged pack instead.
const MAX_PACKS: usize = 16;
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

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(crate = "covalence_lib_serde::serde")]
#[serde(deny_unknown_fields)]
struct Pack {
    pack: String,
    bytes: u64,
}

/// A revision's receipts: its ledger and the packs that hold them, oldest
/// first. A module in a later pack replaces the same module in an earlier one.
#[derive(Serialize, Deserialize)]
#[serde(crate = "covalence_lib_serde::serde")]
#[serde(deny_unknown_fields)]
struct Index {
    format: String,
    scope: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    packs: Vec<Pack>,
    /// `FORMAT_V1`'s single pack.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pack: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    bytes: Option<u64>,
    ledger: String,
    ledger_hash: String,
}

impl Index {
    fn packs(&self) -> Vec<Pack> {
        match (&self.pack, self.bytes) {
            (Some(pack), Some(bytes)) => vec![Pack {
                pack: pack.clone(),
                bytes,
            }],
            _ => self.packs.clone(),
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(crate = "covalence_lib_serde::serde")]
#[serde(deny_unknown_fields)]
struct Header {
    format: String,
    scope: String,
    /// `FORMAT_V1` packs carried their ledger; an index carries it now.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    ledger: Option<String>,
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

fn module_name(row: &str) -> Result<&str> {
    row.split('\t')
        .next()
        .ok_or_else(|| miette!("invalid module ledger"))
}

/// Write `lines`, each a module's record, as one pack, and upload it.
fn write_pack<'a>(
    kv: &dyn Kv,
    scope: &str,
    lines: impl IntoIterator<Item = &'a [u8]>,
) -> Result<Pack> {
    let mut pack = tempfile::tempfile().into_diagnostic()?;
    let mut gzip = GzEncoder::new(&mut pack, Compression::fast());
    line(
        &mut gzip,
        &Header {
            format: FORMAT.into(),
            scope: scope.into(),
            ledger: None,
        },
    )?;
    for record in lines {
        gzip.write_all(record).into_diagnostic()?;
    }
    gzip.finish().into_diagnostic()?;
    let bytes = pack.metadata().into_diagnostic()?.len();
    if bytes > MAX_PACK {
        return Err(miette!("receipt pack exceeds {MAX_PACK} bytes"));
    }
    let hash = digest(&mut pack)?;
    // No index can point at an incomplete pack, even after an interrupted upload.
    kv.put_new_from(format!("packs/{hash}").as_bytes(), &mut pack, bytes)
        .into_diagnostic()?;
    Ok(Pack { pack: hash, bytes })
}

/// Publish this ledger's receipts under `revision`. Against the index this
/// checkout pulled, only the modules whose ledger rows changed go into a new
/// pack; the index names it after the parent's packs, which still hold every
/// other module. Past [`MAX_PACKS`] the packs are merged into one.
fn publish(kv: &dyn Kv, args: &Args, revision: &str) -> Result<Value> {
    let key = index_key(&args.scope, revision)?;
    let mut phases = serde_json::Map::new();
    let mut phase = Instant::now();
    let mut mark = |name: &str, phase: &mut Instant| {
        phases.insert(name.into(), json!(phase.elapsed().as_secs_f64()));
        *phase = Instant::now();
    };
    let ledger = fs::read_to_string(&args.ledger).into_diagnostic()?;
    let shared = args
        .state
        .join("shared")
        .join(format!("{}.json", args.scope));
    let parent = if shared.is_file() {
        let index: Index =
            serde_json::from_slice(&fs::read(&shared).into_diagnostic()?).into_diagnostic()?;
        validate_index(&index, &args.scope)?;
        // A changed policy line makes every row new.
        (index.ledger.lines().next() == ledger.lines().next()).then_some(index)
    } else {
        None
    };
    let before = parent
        .as_ref()
        .map(|index| {
            index
                .ledger
                .lines()
                .skip(1)
                .map(|row| Ok((module_name(row)?, row)))
                .collect::<Result<BTreeMap<_, _>>>()
        })
        .transpose()?
        .unwrap_or_default();
    let mut current = BTreeSet::new();
    let mut fresh = BTreeMap::new();
    for row in ledger.lines().skip(1) {
        let name = module_name(row)?;
        current.insert(name.to_owned());
        if before.get(name) == Some(&row) {
            continue;
        }
        if let Some(module) = export_module(&args.state, name)? {
            let mut record = serde_json::to_vec(&module).into_diagnostic()?;
            record.push(b'\n');
            fresh.insert(name.to_owned(), record);
        }
    }
    let changed = fresh.len();
    mark("export", &mut phase);
    let mut packs = parent.as_ref().map(Index::packs).unwrap_or_default();
    let mut uploaded = Vec::new();
    if packs.len() >= MAX_PACKS {
        // Newest first: a module's latest receipt wins.
        let mut merged = fresh;
        for pack in packs.iter().rev() {
            let mut file = fetch_pack(kv, pack)?;
            each_module(&mut file, &args.scope, |record, module| {
                if current.contains(&module.name) && !merged.contains_key(&module.name) {
                    merged.insert(module.name, record.to_vec());
                }
                Ok(())
            })?;
        }
        mark("merge", &mut phase);
        let pack = write_pack(kv, &args.scope, merged.values().map(Vec::as_slice))?;
        uploaded.push(pack.clone());
        packs = vec![pack];
    } else if !fresh.is_empty() || packs.is_empty() {
        let pack = write_pack(kv, &args.scope, fresh.values().map(Vec::as_slice))?;
        uploaded.push(pack.clone());
        packs.push(pack);
    }
    mark("upload", &mut phase);
    let index = Index {
        format: FORMAT.into(),
        scope: args.scope.clone(),
        packs,
        pack: None,
        bytes: None,
        ledger_hash: blake3::hash(ledger.as_bytes()).to_hex().to_string(),
        ledger,
    };
    let created = kv
        .put_new(
            key.as_bytes(),
            serde_json::to_vec(&index).into_diagnostic()?.into(),
        )
        .into_diagnostic()?;
    mark("index", &mut phase);
    Ok(json!({
        "status": "published",
        "new_index": created,
        "modules": changed,
        "packs": index.packs.len(),
        "bytes": uploaded.iter().map(|pack| pack.bytes).sum::<u64>(),
        "uploaded": uploaded.iter().map(|pack| pack.pack.clone()).collect::<Vec<_>>(),
        "phases": phases,
    }))
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
            let name = module_name(row)?;
            relative(name)?;
            Ok(name.to_owned())
        })
        .collect::<Result<BTreeSet<_>>>()
        .map(Some)
}

/// Each module of a hash-checked pack, with its record line, after checking
/// the pack's header against `scope`.
fn each_module(
    file: &mut fs::File,
    scope: &str,
    mut visit: impl FnMut(&[u8], Module) -> Result<()>,
) -> Result<()> {
    file.rewind().into_diagnostic()?;
    let mut input = BufReader::new(GzDecoder::new(&mut *file).take(MAX_EXPANDED + 1));
    let bytes = read_line(&mut input)?.ok_or_else(|| miette!("empty receipt pack"))?;
    let header: Header = serde_json::from_slice(&bytes).into_diagnostic()?;
    if !matches!(header.format.as_str(), FORMAT | FORMAT_V1) || header.scope != scope {
        return Err(miette!("receipt pack scope/format mismatch"));
    }
    let mut size = bytes.len() as u64;
    let mut names = BTreeSet::new();
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
        visit(&bytes, module)?;
    }
    Ok(())
}

/// Import from `packs`, newest first, the receipts this checkout lacks, then
/// write `ledger`. The packs were hash-checked before parsing. Only misses
/// need local proof maps: unchanged modules already have a trusted
/// module-level receipt.
fn unpack(packs: &mut [fs::File], args: &Args, ledger: &str) -> Result<usize> {
    let needed = needed(args, ledger)?;
    let parent = args
        .state
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    fs::create_dir_all(parent).into_diagnostic()?;
    // Sticky disks may be on a different filesystem from the runner's /tmp.
    let staged = tempfile::tempdir_in(parent).into_diagnostic()?;
    let mut seen = BTreeSet::new();
    let mut imported = BTreeSet::new();
    for file in packs.iter_mut() {
        each_module(file, &args.scope, |_, module| {
            if !seen.insert(module.name.clone()) {
                return Ok(());
            }
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
            Ok(())
        })?;
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
    atomic(&args.ledger, ledger.as_bytes())?;
    Ok(imported.len())
}

fn validate_index(index: &Index, scope: &str) -> Result<()> {
    let shape = match index.format.as_str() {
        FORMAT => index.pack.is_none() && index.bytes.is_none() && !index.packs.is_empty(),
        FORMAT_V1 => index.pack.is_some() && index.bytes.is_some() && index.packs.is_empty(),
        _ => false,
    };
    if !shape
        || index.scope != scope
        || index.packs().len() > MAX_PACKS
        || blake3::hash(index.ledger.as_bytes()).to_hex().as_str() != index.ledger_hash
    {
        return Err(miette!("invalid receipt index"));
    }
    for pack in index.packs() {
        if pack.bytes > MAX_PACK {
            return Err(miette!("invalid receipt index"));
        }
        hex(&pack.pack, &[64])?;
    }
    Ok(())
}

fn fetch_pack(kv: &dyn Kv, pack: &Pack) -> Result<fs::File> {
    let mut file = tempfile::tempfile().into_diagnostic()?;
    let size = kv
        .get_to(format!("packs/{}", pack.pack).as_bytes(), &mut file)
        .into_diagnostic()?;
    if size != Some(pack.bytes) || digest(&mut file)? != pack.pack {
        return Err(miette!("missing or corrupt receipt pack"));
    }
    Ok(file)
}

fn restore(kv: &dyn Kv, args: &Args, index: &Index) -> Result<usize> {
    let mut packs = index
        .packs()
        .iter()
        .rev()
        .map(|pack| fetch_pack(kv, pack))
        .collect::<Result<Vec<_>>>()?;
    unpack(&mut packs, args, &index.ledger)
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
            restore(kv, args, &index)?
        };
        atomic(&parent, &bytes)?;
        let packs = index.packs();
        return Ok(json!({
            "status": "restored",
            "revision": revision,
            "packs": packs.len(),
            "bytes": bytes.len() as u64 + if ledger_only { 0 } else { packs.iter().map(|pack| pack.bytes).sum() },
            "modules": modules,
            "ledger_only": ledger_only,
        }));
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

    const LEDGER: &str = "successful ledger\n";

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
                ledger: None,
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

    /// A small receipt for module `name`, its entries tagged by `version`.
    fn small(name: &str, version: &str) -> Module {
        let cas: Arc<dyn Cas> = Arc::new(SqliteCas::open_in_memory().unwrap());
        let entries: BTreeMap<String, String> = (0..3)
            .map(|n| (format!("proof{n}"), format!("{version}{n}")))
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
            name: name.into(),
            receipt,
            manifest,
            entries,
        }
    }

    /// Replace this checkout's receipt of `module`, as a replay does.
    fn replay(args: &Args, module: &Module) {
        let relative = relative(&module.name).unwrap();
        let _ = fs::remove_file(
            args.state
                .join("modules")
                .join(&relative)
                .with_extension("json"),
        );
        let _ = fs::remove_dir_all(args.state.join("cas").join(&relative));
        import_module(&args.state, module).unwrap();
    }

    fn ledger(rows: &[(&str, &str)]) -> String {
        let mut ledger = String::from("policy\n");
        for (name, input) in rows {
            ledger.push_str(&format!("{name}\t{input}\tout\n"));
        }
        ledger
    }

    #[test]
    fn a_push_sends_only_changed_receipts_and_a_pull_takes_the_newest() {
        let kv = covalence_kv_memory::MemoryKv::default();
        let root = tempfile::tempdir().unwrap();
        let first = Args {
            state: root.path().join("first"),
            ..args(root.path())
        };
        replay(&first, &small("A", "a"));
        replay(&first, &small("B", "b"));
        fs::write(&first.ledger, ledger(&[("A", "1"), ("B", "1")])).unwrap();
        let one = "1".repeat(40);
        let published = publish(&kv, &first, &one).unwrap();
        assert_eq!(
            (published["modules"].as_u64(), published["packs"].as_u64()),
            (Some(2), Some(1))
        );
        // The next checkout pulls only what changed, replays B and pushes.
        let second = Args {
            state: root.path().join("second"),
            ledger: root.path().join("pulled"),
            current_inputs: Some(root.path().join("current")),
            ..args(root.path())
        };
        let changed = ledger(&[("A", "1"), ("B", "2")]);
        fs::write(root.path().join("current"), &changed).unwrap();
        pull(&kv, &second, std::slice::from_ref(&one)).unwrap();
        replay(&second, &small("B", "b2"));
        fs::write(root.path().join("ledger2"), &changed).unwrap();
        let pushing = Args {
            ledger: root.path().join("ledger2"),
            current_inputs: None,
            ..second
        };
        let two = "2".repeat(40);
        let published = publish(&kv, &pushing, &two).unwrap();
        assert_eq!(
            (published["modules"].as_u64(), published["packs"].as_u64()),
            (Some(1), Some(2))
        );
        assert!(published["bytes"].as_u64().unwrap() < 2_000);
        // A fresh checkout gets A from the first pack and B from the second.
        let third = Args {
            state: root.path().join("third"),
            ..args(root.path())
        };
        let restored = pull(&kv, &third, std::slice::from_ref(&two)).unwrap();
        assert_eq!(restored["modules"].as_u64(), Some(2));
        let b = export_module(&third.state, "B").unwrap().unwrap();
        assert_eq!(b.entries["proof0"], "b20");
        assert!(export_module(&third.state, "A").unwrap().is_some());
    }

    #[test]
    fn past_the_pack_limit_a_push_merges_every_pack() {
        let kv = covalence_kv_memory::MemoryKv::default();
        let root = tempfile::tempdir().unwrap();
        let args = args(root.path());
        replay(&args, &small("A", "a"));
        let mut parent = None;
        for n in 0..=MAX_PACKS {
            replay(&args, &small("B", &format!("b{n}")));
            fs::write(&args.ledger, ledger(&[("A", "1"), ("B", &n.to_string())])).unwrap();
            let revision = format!("{n:040}");
            let published = publish(&kv, &args, &revision).unwrap();
            let expected = if n + 1 > MAX_PACKS { 1 } else { n + 1 };
            assert_eq!(
                published["packs"].as_u64(),
                Some(expected as u64),
                "push {n}"
            );
            // Each push follows the previous one's index, as a pull leaves it.
            let key = index_key(&args.scope, &revision).unwrap();
            let index = kv.get(key.as_bytes()).unwrap().unwrap();
            atomic(
                &args
                    .state
                    .join("shared")
                    .join(format!("{}.json", args.scope)),
                &index,
            )
            .unwrap();
            parent = Some(revision);
        }
        let fresh = Args {
            state: root.path().join("fresh"),
            ..args
        };
        let restored = pull(&kv, &fresh, &[parent.unwrap()]).unwrap();
        assert_eq!(
            (restored["modules"].as_u64(), restored["packs"].as_u64()),
            (Some(2), Some(1))
        );
        let b = export_module(&fresh.state, "B").unwrap().unwrap();
        assert_eq!(b.entries["proof0"], format!("b{MAX_PACKS}0"));
    }

    #[test]
    fn an_index_of_one_pack_from_an_earlier_checker_still_restores() {
        let kv = covalence_kv_memory::MemoryKv::default();
        let root = tempfile::tempdir().unwrap();
        let args = args(root.path());
        let mut file = tempfile::tempfile().unwrap();
        let mut gzip = GzEncoder::new(&mut file, Compression::fast());
        line(
            &mut gzip,
            &Header {
                format: FORMAT_V1.into(),
                scope: args.scope.clone(),
                ledger: Some(LEDGER.into()),
            },
        )
        .unwrap();
        line(&mut gzip, &small("A", "a")).unwrap();
        gzip.finish().unwrap();
        let bytes = file.metadata().unwrap().len();
        let hash = digest(&mut file).unwrap();
        kv.put_new_from(format!("packs/{hash}").as_bytes(), &mut file, bytes)
            .unwrap();
        let index = Index {
            format: FORMAT_V1.into(),
            scope: args.scope.clone(),
            packs: Vec::new(),
            pack: Some(hash),
            bytes: Some(bytes),
            ledger: LEDGER.into(),
            ledger_hash: blake3::hash(LEDGER.as_bytes()).to_hex().to_string(),
        };
        let revision = "3".repeat(40);
        kv.put_new(
            index_key(&args.scope, &revision).unwrap().as_bytes(),
            serde_json::to_vec(&index).unwrap().into(),
        )
        .unwrap();
        let restored = pull(&kv, &args, &[revision]).unwrap();
        assert_eq!(restored["modules"].as_u64(), Some(1));
    }

    #[test]
    fn thirty_thousand_entries_rebuild_identical_cas_and_keep_local_pointer() {
        let root = tempfile::tempdir().unwrap();
        let args = args(root.path());
        let module = fixture();
        let expected = module.receipt.clone();
        let file = pack(&args, &[module]);
        assert_eq!(unpack(&mut [file], &args, LEDGER).unwrap(), 1);
        let exported = export_module(&args.state, "Fixture.Proofs")
            .unwrap()
            .unwrap();
        assert_eq!(exported.receipt, expected);
        assert_eq!(exported.entries.len(), 30_000);
        let pointer = args.state.join("modules/Fixture/Proofs.json");
        fs::write(&pointer, "existing local pointer").unwrap();
        unpack(&mut [pack(&args, &[fixture()])], &args, LEDGER).unwrap();
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
        let file = pack(&args, &[parent, child]);
        assert_eq!(unpack(&mut [file], &args, LEDGER).unwrap(), 2);
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
        assert!(unpack(&mut [pack(&args, &[good, bad])], &args, LEDGER).is_err());
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
        let file = pack(&args, &[]);
        args.scope = "b".repeat(64);
        assert!(unpack(&mut [file], &args, LEDGER).is_err());
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
            packs: vec![Pack {
                pack: "a".repeat(64),
                bytes: 1,
            }],
            pack: None,
            bytes: None,
            ledger: ledger.into(),
            ledger_hash: blake3::hash(ledger.as_bytes()).to_hex().to_string(),
        };
        validate_index(&index, &args.scope).unwrap();
        index.ledger.push_str("Forged\tinput\toutput\n");
        assert!(validate_index(&index, &args.scope).is_err());
    }
}

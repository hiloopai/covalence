//! Prepared, content-addressed environments for trusted Lean source checks.
use std::{
    collections::BTreeMap,
    fs,
    io::Write,
    os::unix::fs::PermissionsExt,
    path::{Component, Path, PathBuf},
    process::Command,
};

mod groups;
mod replay;
mod trace;

use covalence_cas_kv::KvCas;
use covalence_cas_sqlite::SqliteCas;
use covalence_cas_trait::{Cas, GetExt};
use covalence_kv_s3::{AwsOptions, S3Kv};
use covalence_kv_trait::Kv;
use covalence_lib_cid::{Blake3, Cid, Format};
use covalence_lib_cli::clap::{self, Args, Subcommand};
use covalence_lib_error::miette::{Context, IntoDiagnostic, Result, miette};
use covalence_lib_serde::{Deserialize, Serialize};
use flate2::{Compression, read::GzDecoder, write::GzEncoder};
use serde_json::{Value, json};

#[derive(Subcommand)]
pub enum Options {
    /// Build imports and snapshot the environment into CAS and the local workspace
    Prepare(Prepare),
    /// Materialize and verify a CAS environment on this worker, once
    Restore(Restore),
    /// Check source against an explicit prepared environment; no Lake or input scans
    Check(Check),
    /// Build a baseline or check replacement proofs against its saved group contexts
    ProofGroups(groups::ProofGroups),
    /// Replay compiled artifacts, reusing exact checked theorem/dependency inputs
    Replay(replay::Replay),
    /// Reuse theorem checks directly from a completed, fixed Lake checkout
    ReplayBuilt(replay::built::Built),
}

#[derive(Args)]
pub struct Storage {
    /// Trusted CAS: SQLite file or s3://bucket/dedicated-prefix
    #[arg(long)]
    cache: String,
    /// Worker-owned materialized CAS objects; never edit these files
    #[arg(long, default_value = ".lake/lean-check")]
    workspace: PathBuf,
}

#[derive(Args)]
pub struct Prepare {
    file: PathBuf,
    /// Build and snapshot the experimental native proof-group bridge
    #[arg(long)]
    proof_groups: bool,
    /// Include the experimental native declaration replay bridge
    #[arg(long)]
    kernel_replay: bool,
    #[arg(long, default_value = ".")]
    project: PathBuf,
    #[command(flatten)]
    storage: Storage,
}

#[derive(Args)]
pub struct Restore {
    /// Environment CID returned by prepare
    environment: Blake3,
    #[command(flatten)]
    storage: Storage,
}

#[derive(Args)]
pub struct Check {
    /// Source bytes to check as the module recorded in the environment
    file: PathBuf,
    #[arg(long)]
    environment: Blake3,
    #[command(flatten)]
    storage: Storage,
}

// A changed Lake wire format must not silently omit a compiler input.
#[derive(Deserialize, Serialize)]
#[serde(crate = "covalence_lib_serde::serde")]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Setup {
    name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    package: Option<String>,
    is_module: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    imports: Option<Value>,
    import_arts: BTreeMap<String, Vec<Vec<PathBuf>>>,
    dynlibs: Vec<PathBuf>,
    plugins: Vec<Value>,
    options: BTreeMap<String, Value>,
}

#[derive(Deserialize, Serialize)]
#[serde(crate = "covalence_lib_serde::serde")]
#[serde(deny_unknown_fields)]
struct Environment {
    kind: String,
    platform: [String; 2],
    toolchain: String,
    imports: String,
    module_path: PathBuf,
    proof_groups: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    kernel_replay: bool,
}

fn validate_setup(setup: &Setup) -> Result<()> {
    if !setup.dynlibs.is_empty() || !setup.plugins.is_empty() || setup.imports.is_some() {
        return Err(miette!(
            "native plugins, dynamic libraries and import overrides are not supported"
        ));
    }
    if setup.options.get("warningAsError") != Some(&Value::Bool(true)) {
        return Err(miette!("prepared environment must enable warningAsError"));
    }
    Ok(())
}

fn read(path: &Path) -> Result<Vec<u8>> {
    fs::read(path)
        .into_diagnostic()
        .wrap_err_with(|| format!("read {}", path.display()))
}

fn checked_output(command: &mut Command, label: &str) -> Result<Vec<u8>> {
    let output = command
        .output()
        .into_diagnostic()
        .wrap_err_with(|| format!("start {label}"))?;
    if !output.status.success() {
        return Err(miette!(
            "{label} failed ({}):\n{}\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    std::io::stderr()
        .write_all(&output.stderr)
        .into_diagnostic()?;
    Ok(output.stdout)
}

fn store(location: &str) -> Result<Box<dyn Cas>> {
    if location.starts_with("s3://") {
        let kv = S3Kv::from_aws(location.parse().into_diagnostic()?, &AwsOptions::default())
            .into_diagnostic()?
            .into_dyn();
        Ok(Box::new(KvCas::new(kv)))
    } else {
        Ok(Box::new(SqliteCas::open(location).into_diagnostic()?))
    }
}

fn safe_relative(path: &Path) -> Result<()> {
    if path.as_os_str().is_empty() || !path.components().all(|c| matches!(c, Component::Normal(_)))
    {
        return Err(miette!("unsafe environment path: {}", path.display()));
    }
    Ok(())
}

fn toolchain_files(root: &Path, dir: &Path, files: &mut BTreeMap<PathBuf, PathBuf>) -> Result<()> {
    for entry in fs::read_dir(dir).into_diagnostic()? {
        let path = entry.into_diagnostic()?.path();
        if path.is_dir() {
            if path
                .symlink_metadata()
                .into_diagnostic()?
                .file_type()
                .is_symlink()
            {
                return Err(miette!(
                    "toolchain directory symlink is unsupported: {}",
                    path.display()
                ));
            }
            toolchain_files(root, &path, files)?;
        } else {
            let name = path.file_name().unwrap_or_default().to_string_lossy();
            if name.ends_with(".olean")
                || name.contains(".olean.")
                || name.ends_with(".ir")
                || name.ends_with(".ir.sig")
                || name.contains(".so")
                || name.ends_with(".dylib")
            {
                files.insert(path.strip_prefix(root).into_diagnostic()?.to_owned(), path);
            }
        }
    }
    Ok(())
}

// Archive metadata is fixed so equivalent inputs have the same CID across checkouts.
// gzip keeps a whole toolchain below SQLite's single-blob limit and avoids thousands
// of S3 requests. Packing and verification happen only at prepare/restore time.
fn archive(cas: &dyn Cas, files: BTreeMap<PathBuf, PathBuf>) -> Result<String> {
    let mut packed = tempfile::NamedTempFile::new().into_diagnostic()?;
    {
        let gzip = GzEncoder::new(packed.as_file_mut(), Compression::fast());
        let mut tar = tar::Builder::new(gzip);
        for (name, path) in files {
            safe_relative(&name)?;
            let mut input = fs::File::open(&path).into_diagnostic()?;
            let metadata = input.metadata().into_diagnostic()?;
            if !metadata.is_file() {
                return Err(miette!("not a regular file: {}", path.display()));
            }
            let mut header = tar::Header::new_gnu();
            header.set_size(metadata.len());
            header.set_mode(if metadata.permissions().mode() & 0o111 == 0 {
                0o444
            } else {
                0o555
            });
            header.set_mtime(0);
            header.set_uid(0);
            header.set_gid(0);
            header.set_cksum();
            tar.append_data(&mut header, name, &mut input)
                .into_diagnostic()?;
        }
        tar.into_inner()
            .into_diagnostic()?
            .finish()
            .into_diagnostic()?;
    }
    let mut hash = blake3::Hasher::new();
    hash.update_reader(fs::File::open(packed.path()).into_diagnostic()?)
        .into_diagnostic()?;
    let cid = Blake3::new(Format::BLAKE3, hash.finalize().as_bytes()).into_diagnostic()?;
    let stored = cas
        .put_from(
            cid,
            &mut fs::File::open(packed.path()).into_diagnostic()?,
            packed.as_file().metadata().into_diagnostic()?.len(),
        )
        .into_diagnostic()?;
    Ok(stored.to_string())
}

fn object_path(workspace: &Path, cid: &str) -> Result<PathBuf> {
    let cid: Blake3 = cid.parse().into_diagnostic()?;
    Ok(workspace.join("objects").join(cid.to_string()))
}

fn install(cas: &dyn Cas, workspace: &Path, cid: &str) -> Result<PathBuf> {
    let target = object_path(workspace, cid)?;
    if target.exists() {
        if !target
            .symlink_metadata()
            .into_diagnostic()?
            .file_type()
            .is_dir()
        {
            return Err(miette!(
                "materialized object is not a directory: {}",
                target.display()
            ));
        }
        return Ok(target);
    }
    let objects = workspace.join("objects");
    fs::create_dir_all(&objects).into_diagnostic()?;
    let mut packed = tempfile::NamedTempFile::new_in(workspace).into_diagnostic()?;
    let key: Cid = cid.parse().into_diagnostic()?;
    if cas
        .get_to(&key, packed.as_file_mut())
        .into_diagnostic()?
        .is_none()
    {
        return Err(miette!("missing environment object {cid}"));
    }
    let tree = tempfile::tempdir_in(&objects).into_diagnostic()?;
    let mut tar = tar::Archive::new(GzDecoder::new(
        fs::File::open(packed.path()).into_diagnostic()?,
    ));
    for entry in tar.entries().into_diagnostic()? {
        let mut entry = entry.into_diagnostic()?;
        let path = entry.path().into_diagnostic()?.into_owned();
        safe_relative(&path)?;
        if !entry.header().entry_type().is_file() {
            return Err(miette!(
                "environment archive must contain only regular files"
            ));
        }
        let dest = tree.path().join(&path);
        fs::create_dir_all(
            dest.parent()
                .ok_or_else(|| miette!("missing artifact parent"))?,
        )
        .into_diagnostic()?;
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&dest)
            .into_diagnostic()?;
        std::io::copy(&mut entry, &mut file).into_diagnostic()?;
        let mode = if entry.header().mode().into_diagnostic()? & 0o111 == 0 {
            0o444
        } else {
            0o555
        };
        file.set_permissions(fs::Permissions::from_mode(mode))
            .into_diagnostic()?;
    }
    // Only publish complete, verified trees. Directory entries stay writable by
    // the trusted workspace owner for removal; artifact files are read-only.
    match fs::rename(tree.path(), &target) {
        Ok(()) => {}
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::AlreadyExists | std::io::ErrorKind::DirectoryNotEmpty
            ) =>
        {
            if !target.is_dir() {
                return Err(miette!("concurrent object publication failed: {error}"));
            }
        }
        Err(error) => {
            return Err(error)
                .into_diagnostic()
                .wrap_err("publish environment object");
        }
    }
    Ok(target)
}

fn decode_environment(bytes: &[u8]) -> Result<Environment> {
    let env: Environment = serde_json::from_slice(bytes).into_diagnostic()?;
    if env.kind != "mor/lean-environment/v3"
        || env.platform != [std::env::consts::OS, std::env::consts::ARCH]
    {
        return Err(miette!("unsupported environment format or platform"));
    }
    safe_relative(&env.module_path)?;
    Ok(env)
}

fn materialize(cas: &dyn Cas, workspace: &Path, cid: Blake3, bytes: &[u8]) -> Result<()> {
    let env = decode_environment(bytes)?;
    let toolchain = install(cas, workspace, &env.toolchain)?;
    let lean = toolchain.join("bin/lean");
    if !lean.is_file() || lean.metadata().into_diagnostic()?.permissions().mode() & 0o111 == 0 {
        return Err(miette!("environment has no executable Lean"));
    }
    let imports = install(cas, workspace, &env.imports)?;
    let mut setup: Setup =
        serde_json::from_slice(&read(&imports.join("setup.json"))?).into_diagnostic()?;
    validate_setup(&setup)?;
    for groups in setup.import_arts.values_mut() {
        for path in groups.iter_mut().flatten() {
            safe_relative(path)?;
            *path = imports.join(&*path);
            if !path.is_file() {
                return Err(miette!("missing prepared import: {}", path.display()));
            }
        }
    }
    let environments = workspace.join("environments");
    fs::create_dir_all(&environments).into_diagnostic()?;
    for (suffix, data) in [
        ("setup.json", serde_json::to_vec(&setup).into_diagnostic()?),
        ("json", bytes.to_vec()),
    ] {
        let mut pending = tempfile::NamedTempFile::new_in(&environments).into_diagnostic()?;
        pending.write_all(&data).into_diagnostic()?;
        pending
            .as_file()
            .set_permissions(fs::Permissions::from_mode(0o444))
            .into_diagnostic()?;
        pending
            .persist(environments.join(format!("{cid}.{suffix}")))
            .into_diagnostic()?;
    }
    Ok(())
}

fn prepare(args: Prepare) -> Result<()> {
    let project = args.project.canonicalize().into_diagnostic()?;
    let file = project.join(args.file).canonicalize().into_diagnostic()?;
    let relative = file.strip_prefix(&project).into_diagnostic()?.to_owned();
    safe_relative(&relative)?;
    let source = read(&file)?;
    let setup = checked_output(
        Command::new("lake")
            .current_dir(&project)
            .args(["--rehash", "setup-file"])
            .arg(&file),
        "Lake dependency preparation",
    )?;
    if read(&file)? != source {
        return Err(miette!(
            "source changed during preparation; use an idle checkout"
        ));
    }
    let mut setup: Setup = serde_json::from_slice(&setup).into_diagnostic()?;
    setup
        .options
        .insert("warningAsError".into(), Value::Bool(true));
    validate_setup(&setup)?;
    let prefix = checked_output(
        Command::new("lake")
            .current_dir(&project)
            .args(["env", "lean", "--print-prefix"]),
        "resolve Lean toolchain",
    )?;
    let prefix = PathBuf::from(String::from_utf8(prefix).into_diagnostic()?.trim());
    let mut files = BTreeMap::from([(PathBuf::from("bin/lean"), prefix.join("bin/lean"))]);
    let bridge = tempfile::tempdir().into_diagnostic()?;
    if args.proof_groups {
        let executable = groups::build_bridge(&prefix, bridge.path())?;
        files.insert(PathBuf::from("bin/mor-proof-groups"), executable);
    }
    if args.kernel_replay {
        let executable = replay::build_bridge(&prefix, bridge.path())?;
        files.insert(PathBuf::from("bin/mor-kernel-replay"), executable);
    }
    toolchain_files(&prefix, &prefix.join("lib"), &mut files)?;
    let cas = store(&args.storage.cache)?;
    let toolchain = archive(&*cas, files)?;
    let mut files = BTreeMap::new();
    for groups in setup.import_arts.values_mut() {
        for path in groups.iter_mut().flatten() {
            let name = PathBuf::from(format!("imports/{}", files.len()));
            files.insert(name.clone(), project.join(&*path));
            *path = name;
        }
    }
    let mut setup_file = tempfile::NamedTempFile::new().into_diagnostic()?;
    setup_file
        .write_all(&serde_json::to_vec(&setup).into_diagnostic()?)
        .into_diagnostic()?;
    files.insert(PathBuf::from("setup.json"), setup_file.path().to_owned());
    let imports = archive(&*cas, files)?;
    let bytes = serde_json::to_vec(&Environment {
        kind: "mor/lean-environment/v3".into(),
        platform: [std::env::consts::OS.into(), std::env::consts::ARCH.into()],
        toolchain,
        imports,
        module_path: relative,
        proof_groups: args.proof_groups,
        kernel_replay: args.kernel_replay,
    })
    .into_diagnostic()?;
    let cid: Cid = cas.put(bytes.clone().into()).into_diagnostic()?;
    fs::create_dir_all(&args.storage.workspace).into_diagnostic()?;
    let workspace = args.storage.workspace.canonicalize().into_diagnostic()?;
    materialize(
        &*cas,
        &workspace,
        cid.to_string().parse().into_diagnostic()?,
        &bytes,
    )?;
    println!("{}", json!({"environment": cid.to_string()}));
    Ok(())
}

fn restore(args: Restore) -> Result<()> {
    let cas = store(&args.storage.cache)?;
    let cid: Cid = args.environment.into();
    let fact = cas
        .get_fact(&cid)
        .into_diagnostic()?
        .ok_or_else(|| miette!("missing environment {cid}"))?;
    fs::create_dir_all(&args.storage.workspace).into_diagnostic()?;
    let workspace = args.storage.workspace.canonicalize().into_diagnostic()?;
    materialize(&*cas, &workspace, args.environment, fact.bytes())?;
    println!("{}", json!({"environment": cid.to_string()}));
    Ok(())
}

fn run_lean(toolchain: &Path, meta: &Path, input: &Path, env: &Environment) -> Result<()> {
    let output = checked_output(
        Command::new(toolchain.join("bin/lean"))
            .current_dir(input)
            .env_clear()
            .env("LEAN_SYSROOT", toolchain)
            .env("PATH", toolchain.join("bin"))
            .arg(format!(
                "--setup={}",
                meta.with_extension("setup.json").display()
            ))
            .arg("--stdin")
            .arg(&env.module_path)
            .stdin(fs::File::open(input.join("source.lean")).into_diagnostic()?),
        "Lean source check",
    )?;
    std::io::stderr().write_all(&output).into_diagnostic()
}

fn check(args: Check) -> Result<()> {
    let source = read(&args.file)?;
    let receipt = serde_json::to_vec(&json!({
        "kind": "mor/lean-source-success/v2",
        "checker": blake3::hash(include_bytes!("lean_check.rs")).to_hex().to_string(),
        "dependencies": blake3::hash(include_bytes!("../../../../Cargo.lock")).to_hex().to_string(),
        "platform": [std::env::consts::OS, std::env::consts::ARCH],
        "environment": args.environment.to_string(),
        "source": Blake3::of(&source).to_string(),
    }))
    .into_diagnostic()?;
    let cid: Cid = Blake3::of(&receipt).into();
    let cas = store(&args.storage.cache)?;
    if cas
        .get_fact(&cid)
        .into_diagnostic()
        .wrap_err("read cached success")?
        .is_some()
    {
        println!("{}", json!({"status": "hit", "receipt": cid.to_string()}));
        return Ok(());
    }
    let workspace = args
        .storage
        .workspace
        .canonicalize()
        .into_diagnostic()
        .wrap_err("cache miss needs a prepared workspace; run lean-check restore")?;
    let meta = workspace
        .join("environments")
        .join(args.environment.to_string());
    let bytes = read(&meta.with_extension("json"))
        .wrap_err("cache miss needs a prepared environment; run lean-check restore")?;
    if Blake3::of(&bytes) != args.environment {
        return Err(miette!("local environment manifest does not match its CID"));
    }
    let env = decode_environment(&bytes)?;
    let toolchain = object_path(&workspace, &env.toolchain)?;
    let input = tempfile::tempdir().into_diagnostic()?;
    let source_file = input.path().join("source.lean");
    fs::write(&source_file, &source).into_diagnostic()?;
    run_lean(&toolchain, &meta, input.path(), &env)?;
    let mut result = json!({});
    let stored = cas
        .put(receipt.into())
        .into_diagnostic()
        .wrap_err("store successful check")?;
    if stored != cid {
        return Err(miette!("CAS returned the wrong receipt CID"));
    }
    result["status"] = json!("checked");
    result["receipt"] = json!(cid.to_string());
    println!("{result}");
    Ok(())
}

pub fn run(args: Options) -> Result<()> {
    match args {
        Options::Prepare(args) => prepare(args),
        Options::Restore(args) => restore(args),
        Options::Check(args) => check(args),
        Options::ProofGroups(args) => groups::run(args),
        Options::Replay(args) => replay::run(args),
        Options::ReplayBuilt(args) => replay::built::run(args),
    }
}

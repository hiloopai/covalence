//! `scope`: the part of a list of Lake targets that a diff can change.
//!
//! A module's Lake trace covers its source, its imports' outputs, the
//! toolchain, the workspace's configuration and the `needs` of its library.
//! A change from `base` therefore reaches each changed module and every
//! module importing it, and the modules of a library whose `needs` input
//! changed. Lake does not trace files a module reads while elaborating, so
//! `include_str` and `include_bytes` readers count as changed too.
//!
//! Anything else returns the targets unchanged: a change to a package's
//! Lake configuration, its manifest or the toolchain, a workspace this
//! cannot read (a `lakefile.lean`), and any changed file it cannot attribute
//! to a module. Callers name the files Lake never reads with `--exclude`.

use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    fs, io,
    path::{Component, Path, PathBuf},
    process::Command,
};

use crate::{
    error::{Error, IoResult, Result, invalid},
    modules::{Sources, imports, setting},
    project::{Project, Source, output},
};

/// Files whose change can change every module's trace.
const CONFIG: [&str; 4] = [
    "lakefile.toml",
    "lakefile.lean",
    "lake-manifest.json",
    "lean-toolchain",
];

/// The `targets` a change from `base` can affect, narrowed to modules
/// (`+Module`) inside a library. `excludes` are Git glob pathspecs, from the
/// repository's root, of files Lake never reads.
pub(crate) fn scope(
    project: &Project,
    base: &str,
    excludes: &[String],
    targets: &[String],
) -> Result<Vec<String>> {
    if targets.is_empty() {
        return Err(invalid("scope needs the targets to narrow"));
    }
    if let Some(option) = targets.iter().find(|target| target.starts_with('-')) {
        return Err(invalid(format!(
            "scope takes Lake targets, not options such as {option}"
        )));
    }
    let changed = changed(&project.root, base, excludes)?;
    let (targets, reason) = narrow(project, &changed, targets)?;
    match reason {
        Some(reason) => eprintln!("Lake cache: scope is every target: {reason}"),
        None => eprintln!(
            "Lake cache: scope is {} target(s) for {} changed file(s)",
            targets.len(),
            changed.len()
        ),
    }
    Ok(targets)
}

/// Every file that differs between `base` and the working tree, untracked
/// files included, as absolute paths.
fn changed(root: &Path, base: &str, excludes: &[String]) -> Result<BTreeSet<PathBuf>> {
    let git = |args: &[&str]| {
        let mut command = Command::new("git");
        command.current_dir(root).args(args).arg("--").arg(":(top)");
        command.args(
            excludes
                .iter()
                .map(|pattern| format!(":(top,glob,exclude){pattern}")),
        );
        output("git", &mut command)
    };
    let top = PathBuf::from(output(
        "git",
        Command::new("git")
            .current_dir(root)
            .args(["rev-parse", "--show-toplevel"]),
    )?);
    let tracked = git(&["diff", "--name-only", "--no-renames", "-z", base])?;
    let untracked = git(&[
        "ls-files",
        "--full-name",
        "--others",
        "--exclude-standard",
        "-z",
    ])?;
    Ok(tracked
        .split('\0')
        .chain(untracked.split('\0'))
        .filter(|path| !path.is_empty())
        .map(|path| top.join(path))
        .collect())
}

/// `path` without `.` and `..` components, which Git paths and source
/// folders such as `../..` can hold.
fn normal(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
}

/// One package of this repository: the root or a path dependency.
struct Package {
    dir: PathBuf,
    config: String,
}

/// The modules reachable from the targets, with their files and imports.
#[derive(Default)]
struct Graph {
    files: BTreeMap<String, PathBuf>,
    imports: BTreeMap<String, Vec<String>>,
}

impl Graph {
    fn add(&mut self, sources: &Sources, start: impl IntoIterator<Item = String>) -> Result<()> {
        let mut queue: VecDeque<String> = start.into_iter().collect();
        while let Some(module) = queue.pop_front() {
            if self.imports.contains_key(&module) {
                continue;
            }
            // Modules of Git dependencies and of Lean itself are not ours.
            let Some((_, path)) = sources.find(&module) else {
                continue;
            };
            let source = fs::read_to_string(&path).operation(format!("read {}", path.display()))?;
            let found = imports(&source);
            queue.extend(found.iter().cloned());
            self.imports.insert(module.clone(), found);
            self.files.insert(module, normal(&path));
        }
        Ok(())
    }

    /// `module` and everything it imports.
    fn closure<'a>(&'a self, module: &'a str) -> BTreeSet<&'a str> {
        let mut seen = BTreeSet::new();
        let mut queue = vec![module];
        while let Some(module) = queue.pop() {
            if let Some(found) = self.imports.get(module)
                && seen.insert(module)
            {
                queue.extend(found.iter().map(String::as_str));
            }
        }
        seen
    }
}

/// What a target stands for: modules, or a target this cannot read.
enum Target {
    Library(Vec<String>),
    Modules(Vec<String>),
    Unknown,
}

/// The narrowed targets, or all of them with the reason.
fn narrow(
    project: &Project,
    changed: &BTreeSet<PathBuf>,
    targets: &[String],
) -> Result<(Vec<String>, Option<String>)> {
    let everything = |reason: String| Ok((targets.to_vec(), Some(reason)));
    // Any Lake configuration, also one of a workspace elsewhere in the repository.
    if let Some(path) = changed.iter().find(|path| {
        path.file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| CONFIG.contains(&name))
    }) {
        return everything(format!("{} changed", path.display()));
    }
    let all = project.packages()?;
    let ours: Vec<_> = all
        .into_iter()
        .filter(|package| package.source == Source::Repository)
        .collect();
    let mut packages = Vec::new();
    for package in &ours {
        let dir = normal(&project.root.join(&package.dir));
        let config = match fs::read_to_string(dir.join("lakefile.toml")) {
            Ok(config) => config,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return everything(format!("{} has no lakefile.toml", package.name));
            }
            Err(source) => {
                return Err(Error::Io {
                    operation: format!("read {}", dir.join("lakefile.toml").display()),
                    source,
                });
            }
        };
        packages.push(Package { dir, config });
    }
    let sources = Sources::new(project, &ours)?;
    let root = &packages[0];
    let kinds: Vec<Target> = targets
        .iter()
        .map(|target| {
            // Lake reads a bare name as a library or executable before a module.
            if let Some(module) = target.strip_prefix('+') {
                return Target::Modules(vec![module.to_owned()]);
            }
            let modules = || sources.target_modules(&root.config, &root.dir, target);
            match section(&root.config, target) {
                Some("lean_lib") => Target::Library(modules()),
                Some(_) => Target::Modules(modules()),
                None if sources.find(target).is_some() => Target::Modules(vec![target.clone()]),
                None => Target::Unknown,
            }
        })
        .collect();
    let mut graph = Graph::default();
    for kind in &kinds {
        if let Target::Library(modules) | Target::Modules(modules) = kind {
            graph.add(&sources, modules.iter().cloned())?;
        }
    }

    let mut seeds = BTreeSet::new();
    let mut attributed = BTreeSet::new();
    for path in changed {
        if path
            .extension()
            .is_some_and(|extension| extension == "lean")
        {
            attributed.insert(path);
            for (_, dir) in sources.roots() {
                if let Ok(relative) = path.strip_prefix(normal(dir)) {
                    let module = relative.with_extension("");
                    if let Some(module) = module.to_str() {
                        seeds.insert(module.replace('/', "."));
                    }
                }
            }
        }
    }
    for (module, file) in &graph.files {
        let source = fs::read_to_string(file).operation(format!("read {}", file.display()))?;
        let dir = file.parent().unwrap_or(file);
        for included in includes(&source) {
            if let Some(path) = changed.get(&normal(&dir.join(included))) {
                seeds.insert(module.clone());
                attributed.insert(path);
            }
        }
    }
    for package in &packages {
        for (library, needs) in needs(&package.config, &package.dir) {
            let reached: Vec<_> = match needs {
                Some(paths) => changed
                    .iter()
                    .filter(|changed| paths.iter().any(|path| changed.starts_with(path)))
                    .collect(),
                // A need on another target: any change can reach it.
                None => changed.iter().collect(),
            };
            if !reached.is_empty() {
                attributed.extend(reached);
                let modules = sources.target_modules(&package.config, &package.dir, &library);
                graph.add(&sources, modules.iter().cloned())?;
                seeds.extend(modules);
            }
        }
    }
    if let Some(path) = changed.iter().find(|path| !attributed.contains(path)) {
        return everything(format!(
            "{} is no module, included file or `needs` input; --exclude it if Lake never reads it",
            path.display()
        ));
    }

    let mut importers: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for (module, found) in &graph.imports {
        for import in found {
            importers.entry(import).or_default().push(module);
        }
    }
    let mut affected = BTreeSet::new();
    let mut queue: Vec<&str> = seeds.iter().map(String::as_str).collect();
    while let Some(module) = queue.pop() {
        if affected.insert(module) {
            queue.extend(importers.get(module).into_iter().flatten());
        }
    }

    let mut narrowed = Vec::new();
    for (target, kind) in targets.iter().zip(&kinds) {
        match kind {
            Target::Unknown => narrowed.push(target.clone()),
            Target::Modules(modules) => {
                if modules
                    .iter()
                    .any(|module| graph.closure(module).iter().any(|m| affected.contains(m)))
                {
                    narrowed.push(target.clone());
                }
            }
            // A library's affected modules, less those another one imports:
            // Lake builds a module's imports first.
            Target::Library(modules) => {
                let hit: BTreeSet<&str> = modules
                    .iter()
                    .map(String::as_str)
                    .filter(|module| affected.contains(module) && graph.files.contains_key(*module))
                    .collect();
                let implied: BTreeSet<&str> = hit
                    .iter()
                    .flat_map(|module| graph.imports[*module].iter().map(String::as_str))
                    .collect();
                narrowed.extend(hit.difference(&implied).map(|module| format!("+{module}")));
            }
        }
    }
    let mut seen = BTreeSet::new();
    narrowed.retain(|target| seen.insert(target.clone()));
    Ok((narrowed, None))
}

/// `lean_lib` or `lean_exe`, for the table of target `name`.
fn section(config: &str, name: &str) -> Option<&'static str> {
    config.split("\n[[").skip(1).find_map(|section| {
        let kind = ["lean_lib", "lean_exe"]
            .into_iter()
            .find(|kind| section.starts_with(&format!("{kind}]]")))?;
        (setting(section, "name").first().map(String::as_str) == Some(name)).then_some(kind)
    })
}

/// Each library with `needs`, and the paths of those inputs; `None` when a
/// need is not an `input_dir` or `input_file` of this package.
fn needs(config: &str, dir: &Path) -> Vec<(String, Option<Vec<PathBuf>>)> {
    let sections: Vec<&str> = config.split("\n[[").skip(1).collect();
    let inputs: BTreeMap<String, PathBuf> = sections
        .iter()
        .filter(|section| section.starts_with("input_dir]]") || section.starts_with("input_file]]"))
        .filter_map(|section| {
            let name = setting(section, "name").into_iter().next()?;
            let path = setting(section, "path").into_iter().next()?;
            Some((name, normal(&dir.join(path))))
        })
        .collect();
    sections
        .iter()
        .filter(|section| section.starts_with("lean_lib]]"))
        .filter_map(|section| {
            let needs = setting(section, "needs");
            if needs.is_empty() {
                return None;
            }
            let name = setting(section, "name").into_iter().next()?;
            let paths = needs
                .iter()
                .map(|need| inputs.get(need).cloned())
                .collect::<Option<Vec<_>>>();
            Some((name, paths))
        })
        .collect()
}

/// The paths of the `include_str` and `include_bytes` terms in `source`.
fn includes(source: &str) -> Vec<&str> {
    let mut found = Vec::new();
    for keyword in ["include_str", "include_bytes"] {
        for (at, _) in source.match_indices(keyword) {
            let rest = source[at + keyword.len()..].trim_start();
            if let Some(literal) = rest.strip_prefix('"')
                && let Some(end) = literal.find('"')
            {
                found.push(&literal[..end]);
            }
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git(root: &Path, args: &[&str]) {
        let status = Command::new("git")
            .current_dir(root)
            .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
            .args(args)
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?}");
    }

    fn write(root: &Path, name: &str, text: &str) {
        let path = root.join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    /// A committed workspace: library `Demo` (`Demo.*`), whose `Demo.Data`
    /// reads `data/table.txt`; library `Self` with a `needs` input; tool
    /// `tool` on `Demo.A`; and path dependency `dep`, which `Demo.A` imports.
    fn workspace() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let files = [
            (
                "lakefile.toml",
                "name = \"demo\"\n[[require]]\nname = \"dep\"\npath = \"dep\"\n[[lean_lib]]\nname = \"Demo\"\nglobs = [\"Demo.*\"]\n[[lean_lib]]\nname = \"Self\"\nsrcDir = \"self\"\nneeds = [\"records\"]\n[[input_dir]]\nname = \"records\"\npath = \"records\"\n[[lean_exe]]\nname = \"tool\"\nroot = \"Main\"\nsrcDir = \"tools\"\n",
            ),
            (
                "lake-manifest.json",
                r#"{"version": "1.1.0", "packagesDir": ".lake/packages", "name": "demo", "packages": [{"type": "path", "name": "dep", "dir": "dep", "inherited": false, "scope": ""}]}"#,
            ),
            ("lean-toolchain", "leanprover/lean4:v4.0.0\n"),
            ("Demo.lean", "import Demo.A\nimport Demo.Data\n"),
            (
                "Demo/A.lean",
                "module\npublic import Demo.B\nimport Dep.X\n",
            ),
            ("Demo/B.lean", "import Demo.Gone\n"),
            ("Demo/Gone.lean", "-- leaf\n"),
            (
                "Demo/Data.lean",
                "def table := include_str \"../data/table.txt\"\n",
            ),
            ("Demo/Leaf.lean", "import Demo.B\n"),
            ("data/table.txt", "1 2 3\n"),
            ("self/Self.lean", "import Demo.B\n"),
            ("records/a.toml", "x = 1\n"),
            ("tools/Main.lean", "import Demo.A\n"),
            ("dep/lakefile.toml", "name = \"dep\"\n"),
            ("dep/Dep/X.lean", "-- leaf\n"),
            ("docs/guide.md", "# Guide\n"),
        ];
        for (name, text) in files {
            write(root, name, text);
        }
        git(root, &["init", "-q"]);
        git(root, &["add", "-A"]);
        git(root, &["commit", "-qm", "base"]);
        dir
    }

    fn run(dir: &tempfile::TempDir, excludes: &[&str]) -> Vec<String> {
        let root = dir.path().canonicalize().unwrap();
        let project = Project {
            record: root.join("build/lake-cache"),
            root,
            repository: "example/demo".into(),
            workspace: String::new(),
            lean: Default::default(),
            lookups: crate::project::Lookups::None,
        };
        let excludes: Vec<String> = excludes.iter().map(|e| (*e).to_owned()).collect();
        let targets: Vec<String> = ["Demo", "Self", "tool"].map(String::from).into();
        let (narrowed, _) = narrow(
            &project,
            &changed(&project.root, "HEAD", &excludes).unwrap(),
            &targets,
        )
        .unwrap();
        narrowed
    }

    const ALL: [&str; 3] = ["Demo", "Self", "tool"];

    #[test]
    fn a_module_reaches_its_importers_and_no_further() {
        let dir = workspace();
        write(dir.path(), "Demo/Leaf.lean", "import Demo.B\n-- edit\n");
        assert_eq!(run(&dir, &[]), ["+Demo.Leaf"]);
        write(dir.path(), "Demo/Gone.lean", "-- edit\n");
        assert_eq!(run(&dir, &[]), ["+Demo", "+Demo.Leaf", "+Self", "tool"]);
    }

    #[test]
    fn a_path_dependency_reaches_the_root_package() {
        let dir = workspace();
        write(dir.path(), "dep/Dep/X.lean", "-- edit\n");
        assert_eq!(run(&dir, &[]), ["+Demo", "tool"]);
    }

    #[test]
    fn a_deleted_module_reaches_its_importers() {
        let dir = workspace();
        fs::remove_file(dir.path().join("Demo/Gone.lean")).unwrap();
        assert_eq!(run(&dir, &[]), ["+Demo", "+Demo.Leaf", "+Self", "tool"]);
    }

    #[test]
    fn an_included_file_reaches_its_reader() {
        let dir = workspace();
        write(dir.path(), "data/table.txt", "4\n");
        assert_eq!(run(&dir, &[]), ["+Demo"]);
    }

    #[test]
    fn a_needs_input_reaches_its_library() {
        let dir = workspace();
        write(dir.path(), "records/b.toml", "y = 2\n");
        assert_eq!(run(&dir, &[]), ["+Self"]);
    }

    #[test]
    fn configuration_and_unknown_files_select_every_target() {
        for (name, text) in [
            ("lakefile.toml", "name = \"demo\"\n"),
            ("lake-manifest.json", "{}"),
            ("lean-toolchain", "leanprover/lean4:v4.1.0\n"),
            ("dep/lakefile.toml", "name = \"dep\"\n# edit\n"),
            ("docs/guide.md", "# Guide, edited\n"),
        ] {
            let dir = workspace();
            write(dir.path(), name, text);
            assert_eq!(run(&dir, &[]), ALL, "{name}");
        }
    }

    #[test]
    fn excluded_files_select_nothing() {
        let dir = workspace();
        write(dir.path(), "docs/guide.md", "# Guide, edited\n");
        write(dir.path(), "docs/new.md", "# New\n");
        assert!(run(&dir, &["docs/**"]).is_empty());
    }

    #[test]
    fn options_are_rejected() {
        let dir = workspace();
        let root = dir.path().canonicalize().unwrap();
        let project = Project {
            record: root.join("build/lake-cache"),
            root,
            repository: "example/demo".into(),
            workspace: String::new(),
            lean: Default::default(),
            lookups: crate::project::Lookups::None,
        };
        assert!(scope(&project, "HEAD", &[], &["--wfail".into()]).is_err());
        assert!(scope(&project, "HEAD", &[], &[]).is_err());
    }

    #[test]
    fn include_terms_name_their_paths() {
        assert_eq!(
            includes("def a := include_str \"x.txt\"\ndef b := include_bytes  \"../y.bin\""),
            ["x.txt", "../y.bin"]
        );
    }
}

//! Module indexes and import closures, for prefetching a targeted restore.
//!
//! A revision map is keyed by Lake's input hash, which only Lake computes,
//! during the build. A module index names, per package revision, the input
//! each module had in the recorded build: `build` reads it from the module
//! traces Lake leaves in the build folder, and `put` publishes it beside the
//! map, under `lake/<scope>/modules/<owner>/<repo>/<commit>/<package>`, as
//! Zstandard JSON `{"modules": {<module>: <input>}}`.
//!
//! A targeted restore then reads the targets' import closure from the
//! sources alone, looks each module up in the indexes of the revision it
//! restores, and places those entries' files before Lake runs. The prefetch
//! is only a guess: a module whose input changed since is not mapped by its
//! entry, so Lake fetches or builds it as before.

use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    fs, io,
    path::{Path, PathBuf},
};

use covalence_kv_trait::Kv;
use covalence_lib_data::Bytes;
use covalence_lib_serde::{Deserialize, Serialize};

use crate::{
    error::{Error, IoResult, Result, encode, json},
    mapping,
    project::{Package, Project},
};

/// A package revision's modules, by name, and the input each had.
#[derive(Default, Deserialize, Serialize)]
#[serde(crate = "covalence_lib_serde::serde", deny_unknown_fields)]
pub(crate) struct Index {
    pub modules: BTreeMap<String, String>,
}

fn store_error(operation: impl Into<String>) -> impl FnOnce(covalence_kv_trait::Error) -> Error {
    move |source| Error::Store {
        operation: operation.into(),
        source,
    }
}

impl Index {
    /// The modules whose traces under a package's `build` folder name an
    /// input of its recorded `mapping`. A module's trace is
    /// `lib/lean/<A>/<B>.trace` for module `A.B`.
    pub fn record(build: &Path, mapping: &Path) -> Result<Self> {
        let inputs = mapping::rows(mapping)?;
        let mut modules = BTreeMap::new();
        // A module `A.B`'s trace is `lib/lean/A/B.trace`; its native objects'
        // are `ir/A/B.c.o.export.trace` and the like, indexed as `A.B:c.o.export`.
        for (folder, facets) in [("lib/lean", false), ("ir", true)] {
            let root = build.join(folder);
            for trace in crate::project::files(&root, "trace")? {
                let Some(relative) = trace
                    .strip_prefix(&root)
                    .ok()
                    .and_then(|relative| relative.with_extension("").to_str().map(str::to_owned))
                else {
                    continue;
                };
                let name = match relative.split_once('.') {
                    Some((path, facet)) if facets => format!("{}:{facet}", path.replace('/', ".")),
                    None if !facets => relative.replace('/', "."),
                    _ => continue,
                };
                let bytes = fs::read(&trace).operation(format!("read {}", trace.display()))?;
                // Traces of other files, such as old stubs, name no input.
                let Ok(trace) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
                    continue;
                };
                if let Some(input) = trace["depHash"].as_str()
                    && inputs.contains_key(input)
                {
                    modules.insert(name, input.to_owned());
                }
            }
        }
        Ok(Self { modules })
    }

    /// The inputs of `module` and of its facets, such as its native objects.
    fn inputs<'a>(&'a self, module: &'a str) -> impl Iterator<Item = &'a String> + 'a {
        let facets = format!("{module}:");
        self.modules.get(module).into_iter().chain(
            self.modules
                .range(facets.clone()..)
                .take_while(move |(name, _)| name.starts_with(&facets))
                .map(|(_, input)| input),
        )
    }

    pub fn load(path: &Path) -> Result<Option<Self>> {
        match fs::read(path) {
            Ok(bytes) => json(&bytes).map(Some),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(source) => Err(Error::Io {
                operation: format!("read {}", path.display()),
                source,
            }),
        }
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).operation("create module index directory")?;
        }
        fs::write(path, encode(self)?).operation("save module index")
    }

    /// The published index at `key`, if any.
    pub fn get(kv: &dyn Kv, key: &str) -> Result<Option<Self>> {
        let Some(bytes) = kv
            .get(key.as_bytes())
            .map_err(store_error("read module index"))?
        else {
            return Ok(None);
        };
        let raw = zstd::decode_all(&bytes[..]).operation("decode module index")?;
        json(&raw).map(Some)
    }

    pub fn put(&self, kv: &dyn Kv, key: &str) -> Result<()> {
        let bytes = zstd::encode_all(&encode(self)?[..], 3).operation("encode module index")?;
        kv.put(key.as_bytes(), Bytes::from(bytes))
            .map_err(store_error("publish module index"))
    }
}

/// `source` without its comments, line breaks kept.
fn uncommented(source: &str) -> String {
    let mut text = String::with_capacity(source.len());
    let mut depth = 0usize;
    let mut chars = source.chars().peekable();
    while let Some(c) = chars.next() {
        let next = chars.peek().copied();
        if depth > 0 {
            match (c, next) {
                ('-', Some('/')) => {
                    chars.next();
                    depth -= 1;
                }
                ('/', Some('-')) => {
                    chars.next();
                    depth += 1;
                }
                ('\n', _) => text.push('\n'),
                _ => {}
            }
            continue;
        }
        match (c, next) {
            ('/', Some('-')) => {
                chars.next();
                depth = 1;
            }
            ('-', Some('-')) => while chars.next_if(|&c| c != '\n').is_some() {},
            _ => text.push(c),
        }
    }
    text
}

/// The modules a Lean file imports, from its header.
pub(crate) fn imports(source: &str) -> Vec<String> {
    let mut modules = Vec::new();
    for line in uncommented(source).lines() {
        let line = line.trim();
        if line.is_empty() || line == "module" || line == "prelude" {
            continue;
        }
        let mut words = line.split_whitespace().peekable();
        while words
            .next_if(|word| matches!(*word, "public" | "private" | "meta"))
            .is_some()
        {}
        if words.next() != Some("import") {
            break;
        }
        modules.extend(
            words
                .filter(|word| *word != "all")
                .map(|word| word.replace(['«', '»'], "")),
        );
    }
    modules
}

/// Where a workspace's modules live: each package's source folders, from
/// its directory and the `srcDir` settings of a `lakefile.toml`.
pub(crate) struct Sources {
    roots: Vec<(String, PathBuf)>,
}

impl Sources {
    pub fn new(project: &Project, packages: &[Package]) -> Result<Self> {
        let mut roots = Vec::new();
        for package in packages {
            let dir = project.root.join(&package.dir);
            let mut dirs = BTreeSet::from([PathBuf::new()]);
            match fs::read_to_string(dir.join("lakefile.toml")) {
                Ok(config) => dirs.extend(
                    config
                        .lines()
                        .filter_map(|line| line.trim().strip_prefix("srcDir"))
                        .filter_map(|rest| quoted(rest).into_iter().next())
                        .map(PathBuf::from),
                ),
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(source) => {
                    return Err(Error::Io {
                        operation: format!("read {}", dir.join("lakefile.toml").display()),
                        source,
                    });
                }
            }
            roots.extend(
                dirs.into_iter()
                    .map(|sub| (package.name.clone(), dir.join(sub))),
            );
        }
        Ok(Self { roots })
    }

    /// The package and file of `module`, if a source folder holds it.
    fn find(&self, module: &str) -> Option<(&str, PathBuf)> {
        let relative = format!("{}.lean", module.replace('.', "/"));
        self.roots.iter().find_map(|(package, root)| {
            let path = root.join(&relative);
            path.is_file().then_some((package.as_str(), path))
        })
    }

    /// The modules `targets` name and everything they import, by package.
    /// A target that is no module of these sources is looked up as a
    /// library or executable of the root `lakefile.toml`, else skipped.
    pub fn closure(
        &self,
        root: &Path,
        targets: &[String],
    ) -> Result<BTreeMap<String, BTreeSet<String>>> {
        let config = match fs::read_to_string(root.join("lakefile.toml")) {
            Ok(config) => config,
            Err(error) if error.kind() == io::ErrorKind::NotFound => String::new(),
            Err(source) => {
                return Err(Error::Io {
                    operation: "read lakefile.toml".into(),
                    source,
                });
            }
        };
        let mut queue = VecDeque::new();
        for target in targets.iter().filter(|target| !target.starts_with('-')) {
            let target = target
                .trim_start_matches('+')
                .split(':')
                .next()
                .unwrap_or_default();
            if self.find(target).is_some() {
                queue.push_back(target.to_owned());
            } else {
                queue.extend(self.target_modules(&config, root, target));
            }
        }
        let mut seen = BTreeSet::new();
        let mut closure: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        while let Some(module) = queue.pop_front() {
            if !seen.insert(module.clone()) {
                continue;
            }
            // Lean's own modules come with the toolchain.
            let Some((package, path)) = self.find(&module) else {
                continue;
            };
            let source = fs::read_to_string(&path).operation(format!("read {}", path.display()))?;
            closure
                .entry(package.to_owned())
                .or_default()
                .insert(module);
            queue.extend(imports(&source));
        }
        Ok(closure)
    }

    /// The modules of library or executable `name` in a `lakefile.toml`:
    /// its `root` or `roots` (the name by default) and the modules its
    /// `globs` match (`X.+` the submodules of `X`, `X.*` also `X`).
    fn target_modules(&self, config: &str, root: &Path, name: &str) -> Vec<String> {
        let Some(section) = config.split("\n[[").skip(1).find(|section| {
            (section.starts_with("lean_lib]]") || section.starts_with("lean_exe]]"))
                && setting(section, "name").first().map(String::as_str) == Some(name)
        }) else {
            return Vec::new();
        };
        let source = setting(section, "srcDir")
            .into_iter()
            .next()
            .map_or_else(|| root.to_owned(), |dir| root.join(dir));
        let mut modules = setting(section, "roots");
        modules.extend(setting(section, "root"));
        let globs = setting(section, "globs");
        if modules.is_empty() && globs.is_empty() {
            modules.push(name.to_owned());
        }
        for glob in globs {
            let (prefix, itself) = match (glob.strip_suffix(".+"), glob.strip_suffix(".*")) {
                (Some(prefix), _) => (prefix, false),
                (_, Some(prefix)) => (prefix, true),
                _ => {
                    modules.push(glob);
                    continue;
                }
            };
            if itself {
                modules.push(prefix.to_owned());
            }
            let dir = source.join(prefix.replace('.', "/"));
            if let Ok(files) = crate::project::files(&dir, "lean") {
                modules.extend(files.into_iter().filter_map(|file| {
                    let relative = file.strip_prefix(&source).ok()?.with_extension("");
                    Some(relative.to_str()?.replace('/', "."))
                }));
            }
        }
        modules
    }
}

/// The inputs to place before a targeted build, by package: each module of
/// the `targets`' import closure that the index of the revision restored
/// (`commit` for this repository's packages, its pin for a Git dependency)
/// names.
pub(crate) fn prefetch(
    project: &Project,
    kv: &dyn Kv,
    scope: &str,
    packages: &[Package],
    commit: Option<&str>,
    targets: &[String],
) -> Result<BTreeMap<String, BTreeSet<String>>> {
    let closure = Sources::new(project, packages)?.closure(&project.root, targets)?;
    let wanted = packages
        .iter()
        .filter(|package| closure.contains_key(&package.name))
        .filter_map(|package| {
            let commit = match &package.source {
                crate::project::Source::Repository => commit?,
                crate::project::Source::Git { commit, .. } => commit.as_str(),
            };
            Some(
                project
                    .modules_key(scope, package, commit)
                    .map(|key| (package, key)),
            )
        })
        .collect::<Result<Vec<_>>>()?;
    let indexes = crate::storage::parallel(&wanted, |_, (_, key)| Index::get(kv, key))?;
    let mut inputs = BTreeMap::new();
    let (mut found, mut total) = (0, 0);
    for ((package, _), index) in wanted.iter().zip(indexes) {
        let modules = &closure[&package.name];
        total += modules.len();
        let Some(index) = index else {
            continue;
        };
        let chosen = modules
            .iter()
            .flat_map(|module| index.inputs(module).cloned())
            .collect::<BTreeSet<_>>();
        found += modules
            .iter()
            .filter(|module| index.modules.contains_key(*module))
            .count();
        inputs.insert(package.name.clone(), chosen);
    }
    println!("Lake cache: prefetching {found} of the {total} workspace modules the targets import");
    Ok(inputs)
}

/// The quoted strings in `text`.
fn quoted(text: &str) -> Vec<String> {
    text.split('"')
        .skip(1)
        .step_by(2)
        .map(str::to_owned)
        .collect()
}

/// The string values of `key` in one TOML table, a string or an array.
fn setting(section: &str, key: &str) -> Vec<String> {
    let mut lines = section.lines();
    while let Some(line) = lines.next() {
        let Some(rest) = line.trim().strip_prefix(key) else {
            continue;
        };
        let Some(value) = rest.trim_start().strip_prefix('=') else {
            continue;
        };
        let value = value.split('#').next().unwrap_or_default();
        if !value.trim_start().starts_with('[') || value.contains(']') {
            return quoted(value);
        }
        let mut text = value.to_owned();
        for line in lines.by_ref() {
            let line = line.split('#').next().unwrap_or_default();
            text.push_str(line);
            if line.contains(']') {
                break;
            }
        }
        return quoted(&text);
    }
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn headers_name_every_import_form() {
        let source = "/- Copyright\n  notice -/\nmodule -- the module system\n\npublic import Mathlib.Data.Nat\nimport all Moria.A Moria.B\npublic meta import «riscv-zkvm».X\n-- a comment\nimport Std\n\nnamespace Moria\nimport Not.Header\n";
        assert_eq!(
            imports(source),
            [
                "Mathlib.Data.Nat",
                "Moria.A",
                "Moria.B",
                "riscv-zkvm.X",
                "Std"
            ]
        );
    }

    #[test]
    fn closures_follow_imports_through_packages_and_toml_targets() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let files: [(&str, &str); 7] = [
            (
                "lakefile.toml",
                "name = \"demo\"\n[[lean_lib]]\nname = \"Demo\"\nglobs = [\n  \"Demo.+\", # all\n]\n[[lean_exe]]\nname = \"tool\"\nroot = \"Main\"\nsrcDir = \"tools\"\n",
            ),
            (
                "Demo/A.lean",
                "module\npublic import Demo.B\nimport Dep.X\n",
            ),
            ("Demo/B.lean", "import Init\n"),
            ("Demo/Unused.lean", "import Dep.Y\n"),
            ("tools/Main.lean", "import Demo.A\n"),
            ("dep/lakefile.toml", "name = \"dep\"\n"),
            ("dep/Dep/X.lean", "-- leaf\n"),
        ];
        for (name, text) in files {
            let path = root.join(name);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, text).unwrap();
        }
        let sources = Sources {
            roots: vec![
                ("demo".into(), root.to_owned()),
                ("demo".into(), root.join("tools")),
                ("dep".into(), root.join("dep")),
            ],
        };
        let set = |items: &[&str]| {
            items
                .iter()
                .map(|item| (*item).to_owned())
                .collect::<BTreeSet<_>>()
        };
        let tool = sources.closure(root, &["tool".into()]).unwrap();
        assert_eq!(tool["demo"], set(&["Demo.A", "Demo.B", "Main"]));
        assert_eq!(tool["dep"], set(&["Dep.X"]));
        let module = sources.closure(root, &["Demo.B".into()]).unwrap();
        assert_eq!(module["demo"], set(&["Demo.B"]));
        let library = sources
            .closure(root, &["Demo".into(), "--wfail".into()])
            .unwrap();
        assert_eq!(library["demo"], set(&["Demo.A", "Demo.B", "Demo.Unused"]));
        assert!(
            sources
                .closure(root, &["missing".into()])
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn indexes_record_modules_whose_trace_names_a_recorded_input() {
        let dir = tempfile::tempdir().unwrap();
        let build = dir.path().join("build");
        fs::create_dir_all(build.join("lib/lean/Demo")).unwrap();
        let trace = |name: &str, input: &str| {
            fs::write(
                build.join(name),
                format!(r#"{{"depHash": "{input}", "outputs": {{}}}}"#),
            )
            .unwrap();
        };
        trace("lib/lean/Demo/A.trace", "00000000000000aa");
        trace("lib/lean/Demo.trace", "00000000000000bb");
        trace("lib/lean/Demo/Stale.trace", "00000000000000cc");
        fs::create_dir_all(build.join("ir/Demo")).unwrap();
        trace("ir/Demo/A.c.o.export.trace", "00000000000000dd");
        let mapping = dir.path().join("demo.jsonl");
        fs::write(
            &mapping,
            "\"2026-03-17\"\n[\"00000000000000aa\",\"00000000000000a1.ltar\"]\n[\"00000000000000bb\",\"00000000000000b1.ltar\"]\n[\"00000000000000dd\",\"00000000000000d1.o\"]\n",
        )
        .unwrap();
        let index = Index::record(&build, &mapping).unwrap();
        assert_eq!(
            index.modules,
            BTreeMap::from([
                ("Demo".into(), "00000000000000bb".into()),
                ("Demo.A".into(), "00000000000000aa".into()),
                ("Demo.A:c.o.export".into(), "00000000000000dd".into()),
            ])
        );
        assert_eq!(
            index.inputs("Demo.A").collect::<Vec<_>>(),
            ["00000000000000aa", "00000000000000dd"]
        );
        assert_eq!(
            index.inputs("Demo").collect::<Vec<_>>(),
            ["00000000000000bb"]
        );
        let kv = covalence_kv_memory::MemoryKv::default();
        index.put(&kv, "lake/t/modules/o/r/c/demo").unwrap();
        let read = Index::get(&kv, "lake/t/modules/o/r/c/demo")
            .unwrap()
            .unwrap();
        assert_eq!(read.modules, index.modules);
        assert!(Index::get(&kv, "absent").unwrap().is_none());
    }
}

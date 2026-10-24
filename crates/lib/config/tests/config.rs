use std::path::PathBuf;

use covalence_lib_config::{
    Error, cache_dir, config_dir, data_dir, parse, read, read_required, runtime_dir, state_dir,
};
use covalence_lib_serde::Deserialize;

#[derive(Debug, Default, PartialEq, Deserialize)]
#[serde(crate = "covalence_lib_serde::serde", deny_unknown_fields)]
struct Config {
    name: Option<String>,
}

#[test]
fn override_dir_wins() {
    let path = PathBuf::from("/tmp/cov-test");
    assert_eq!(config_dir("cov", Some(path.clone())).unwrap(), path);
    assert_eq!(data_dir("cov", Some(path.clone())).unwrap(), path);
    assert_eq!(cache_dir("cov", Some(path.clone())).unwrap(), path);
    assert_eq!(state_dir("cov", Some(path.clone())).unwrap(), path);
    assert_eq!(runtime_dir("cov", Some(path.clone())).unwrap(), path);
}

/// Runs this test binary again with only the given environment and returns the
/// default directories it reports.
#[cfg(unix)]
fn default_dirs_with(env: &[(&str, &str)]) -> [PathBuf; 5] {
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "print_default_dirs",
            "--nocapture",
            "--include-ignored",
        ])
        .env_clear()
        .envs(env.iter().copied())
        .env("COVALENCE_CONFIG_TEST_CHILD", "1")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    let line = stdout
        .lines()
        .find_map(|line| line.strip_prefix("dirs "))
        .unwrap();
    let dirs: Vec<PathBuf> = line.split('\t').map(PathBuf::from).collect();
    dirs.try_into().unwrap()
}

#[test]
#[ignore = "run by default_dirs_follow_xdg in a child process"]
fn print_default_dirs() {
    if std::env::var_os("COVALENCE_CONFIG_TEST_CHILD").is_some() {
        let dirs = [
            config_dir("cov", None).unwrap(),
            data_dir("cov", None).unwrap(),
            cache_dir("cov", None).unwrap(),
            state_dir("cov", None).unwrap(),
            runtime_dir("cov", None).unwrap(),
        ];
        println!("dirs {}", dirs.map(|d| d.display().to_string()).join("\t"));
    }
}

#[cfg(unix)]
#[test]
fn default_dirs_follow_xdg() {
    let home = "/home/test";
    assert_eq!(
        default_dirs_with(&[("HOME", home)]),
        [
            ".config/cov",
            ".local/share/cov",
            ".cache/cov",
            ".local/state/cov",
            ".local/state/cov"
        ]
        .map(|d| PathBuf::from(home).join(d))
    );
    let shared = [
        ("HOME", home),
        ("XDG_CONFIG_HOME", "/xdg"),
        ("XDG_DATA_HOME", "/xdg"),
        ("XDG_CACHE_HOME", "/xdg"),
        ("XDG_STATE_HOME", "/xdg"),
        ("XDG_RUNTIME_DIR", "/xdg"),
    ];
    assert_eq!(
        default_dirs_with(&shared),
        [(); 5].map(|()| PathBuf::from("/xdg/cov"))
    );
}

#[cfg(unix)]
#[test]
fn relative_and_empty_xdg_variables_are_ignored() {
    let expected = default_dirs_with(&[("HOME", "/home/test")]);
    for value in ["", "relative/path", ".", ".."] {
        assert_eq!(
            default_dirs_with(&[
                ("HOME", "/home/test"),
                ("XDG_CONFIG_HOME", value),
                ("XDG_DATA_HOME", value),
                ("XDG_CACHE_HOME", value),
                ("XDG_STATE_HOME", value),
                ("XDG_RUNTIME_DIR", value),
            ]),
            expected
        );
    }
}

#[cfg(unix)]
#[test]
fn runtime_falls_back_to_state_not_config_or_cache() {
    assert_eq!(
        default_dirs_with(&[
            ("HOME", "/home/test"),
            ("XDG_CONFIG_HOME", "/config"),
            ("XDG_DATA_HOME", "/data"),
            ("XDG_CACHE_HOME", "/cache"),
            ("XDG_STATE_HOME", "/state"),
        ]),
        [
            "/config/cov",
            "/data/cov",
            "/cache/cov",
            "/state/cov",
            "/state/cov"
        ]
        .map(PathBuf::from)
    );
}

#[test]
fn application_names_cannot_escape_the_base() {
    for lookup in [config_dir, data_dir, cache_dir, state_dir, runtime_dir] {
        for name in [
            "", ".", "..", "../other", "/other", "x/y", "x\\y", "C:other", "x\0y",
        ] {
            assert!(matches!(lookup(name, None), Err(Error::Name { .. })));
            let explicit = PathBuf::from("relative/override");
            assert_eq!(lookup(name, Some(explicit.clone())).unwrap(), explicit);
        }
        assert!(lookup("valid-name.with_dots", None).is_ok());
    }
}

#[test]
fn lookups_do_not_create_paths() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("absent");
    for lookup in [config_dir, data_dir, cache_dir, state_dir, runtime_dir] {
        assert_eq!(lookup("cov", Some(path.clone())).unwrap(), path);
        assert!(!path.exists());
    }
}

#[test]
fn parses_toml() {
    let config: Config = parse("name = \"x\"").unwrap();
    assert_eq!(config.name.as_deref(), Some("x"));
    assert!(parse::<Config>("name = ").is_err());
    assert!(parse::<Config>("name = 1").is_err());
    assert!(parse::<Config>("unexpected = true").is_err());
}

#[test]
fn missing_file_is_default() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("missing-parent/missing.toml");
    assert_eq!(read::<Config>(&path).unwrap(), Config::default());
    assert!(matches!(
        read_required::<Config>(&path),
        Err(Error::Read { source, .. }) if source.kind() == std::io::ErrorKind::NotFound
    ));
    assert!(!path.exists());
}

#[test]
fn parse_error_names_the_file() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("bad.toml");
    std::fs::write(&path, "name = ").unwrap();
    let error = read::<Config>(&path).unwrap_err();
    assert!(matches!(error, Error::Parse { .. }));
    assert!(error.to_string().contains("bad.toml"));
}

#[test]
fn existing_files_are_typed_and_read_errors_are_preserved() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("config.toml");
    std::fs::write(&path, "name = \"x\"").unwrap();
    let config = Config {
        name: Some("x".into()),
    };
    assert_eq!(read::<Config>(&path).unwrap(), config);
    assert_eq!(read_required::<Config>(&path).unwrap(), config);
    std::fs::write(&path, [0xff]).unwrap();
    assert!(matches!(read::<Config>(&path), Err(Error::Read { .. })));
    assert!(matches!(
        read::<Config>(temp.path()),
        Err(Error::Read { .. })
    ));
}

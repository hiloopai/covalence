//! Opt-in execution evidence for checker regressions, independent of cache identity.
use covalence_lib_error::miette::{IntoDiagnostic, Result, WrapErr};
use serde_json::Value;
use std::{fs::File, io::Write, path::Path};

pub(super) struct Trace(Option<File>);

impl Trace {
    pub(super) fn open(path: Option<&Path>) -> Result<Self> {
        Ok(Self(
            path.map(|path| {
                File::create(path)
                    .into_diagnostic()
                    .wrap_err_with(|| format!("create execution trace {}", path.display()))
            })
            .transpose()?,
        ))
    }

    /// Disabled tracing must not allocate fingerprints or materialize event payloads.
    pub(super) fn event(&mut self, make_event: impl FnOnce() -> Value) -> Result<()> {
        if let Some(file) = &mut self.0 {
            serde_json::to_writer(&mut *file, &make_event())
                .into_diagnostic()
                .wrap_err("serialize execution trace event")?;
            writeln!(file)
                .into_diagnostic()
                .wrap_err("write execution trace event")?;
            file.flush()
                .into_diagnostic()
                .wrap_err("flush execution trace event")?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn disabled_trace_does_not_evaluate_events() {
        Trace::open(None)
            .unwrap()
            .event(|| panic!("disabled event evaluated"))
            .unwrap();
    }

    #[test]
    fn enabled_trace_replaces_old_evidence_and_flushes_each_event() {
        let old = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(old.path(), "old run\n").unwrap();
        let mut trace = Trace::open(Some(old.path())).unwrap();
        trace
            .event(|| json!({"operation":"kernel_check","outcome":"checked"}))
            .unwrap();
        let text = std::fs::read_to_string(old.path()).unwrap();
        assert_eq!(text.lines().count(), 1);
        assert_eq!(
            serde_json::from_str::<Value>(&text).unwrap()["outcome"],
            "checked"
        );
        trace
            .event(|| json!({"operation":"proof_group","outcome":"reused"}))
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(old.path()).unwrap().lines().count(),
            2
        );
    }

    #[test]
    fn trace_creation_failure_is_not_ignored() {
        let directory = tempfile::tempdir().unwrap();
        assert!(Trace::open(Some(directory.path())).is_err());
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn trace_write_failure_is_not_ignored() {
        let mut trace = Trace::open(Some(Path::new("/dev/full"))).unwrap();
        assert!(trace.event(|| json!({"operation":"kernel_check"})).is_err());
    }
}

//! Durable, private evidence from real CLI/daemon scenarios.

use std::error::Error;
use std::fs;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;

use rusqlite::backup::Backup;
use serde_json::{Value, json};

type EvidenceResult<T = ()> = Result<T, Box<dyn Error>>;

pub(crate) struct Evidence {
    pub(crate) dir: PathBuf,
    scenario: String,
    fake_binary: PathBuf,
    fixture: PathBuf,
    finalized: bool,
    /// Cleared by a scenario that, by design, opens no Store and runs no
    /// turn: `finish` then requires neither the Store's backup, envelopes
    /// and events nor a turn's evidence folder, and the summary says so.
    pub(crate) store_expected: bool,
    /// Cleared by a scenario whose turns launch no vendor, so they have no
    /// evidence folder: `finish` then waives only `evidence/*`; the
    /// collector checks that every launched turn still has its folder.
    pub(crate) folders_expected: bool,
    /// Why collecting the State or proving cleanup failed, if it did:
    /// recorded beside the scenario's outcome, never in its place.
    cleanup_failure: Option<String>,
    /// The scenario's outcome and detail, once `finish` began: a
    /// finalization that fails partway keeps them in the fallback artifact.
    finishing: Option<(String, String)>,
}

impl Evidence {
    pub(crate) fn new(scenario: &str, fake_binary: &Path, fixture: &Path) -> EvidenceResult<Self> {
        if !scenario
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_')
        {
            return Err("invalid evidence scenario name".into());
        }
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../scratchpad/execution/rust-foundation-release/s1-harness/runs");
        fs::create_dir_all(&root)?;
        // Evidence holds Store backups and vendor stderr: private whatever the umask.
        let dir = tempfile::Builder::new()
            .prefix(&format!("{scenario}-"))
            .permissions(fs::Permissions::from_mode(0o700))
            .tempdir_in(root)?
            .keep();
        fs::DirBuilder::new()
            .mode(0o700)
            .create(dir.join("evidence"))?;
        Ok(Self {
            dir,
            scenario: scenario.to_owned(),
            fake_binary: fake_binary.to_owned(),
            fixture: fixture.to_owned(),
            finalized: false,
            store_expected: true,
            folders_expected: true,
            cleanup_failure: None,
            finishing: None,
        })
    }

    pub(crate) fn write(&self, name: &str, bytes: &[u8]) -> EvidenceResult {
        let path = self.dir.join(name);
        if path.parent() != Some(self.dir.as_path()) || name.starts_with('.') {
            return Err("evidence name must be a plain file name".into());
        }
        fs::write(path, bytes)?;
        Ok(())
    }

    pub(crate) fn backup_store(&self, live_path: &Path) -> EvidenceResult {
        let source = rusqlite::Connection::open_with_flags(
            live_path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )?;
        let mut destination = rusqlite::Connection::open(self.dir.join("store.sqlite3"))?;
        Backup::new(&source, &mut destination)?.run_to_completion(
            128,
            std::time::Duration::from_millis(10),
            None,
        )?;
        Ok(())
    }

    /// Copies the State's `evidence/<session>/<turn>/` folders, regular
    /// files only, into the artifact's private `evidence/`.
    pub(crate) fn copy_evidence(&self, root: &Path) -> EvidenceResult {
        copy_tree(root, &self.dir.join("evidence"))
    }

    /// Records that collecting the State or proving cleanup failed: `finish`
    /// then keeps the scenario's outcome, marks the evidence incomplete and
    /// fails.
    pub(crate) fn cleanup_failed(&mut self, detail: String) {
        self.cleanup_failure = Some(detail);
    }

    /// Finalizes the artifact with the scenario's own `outcome` (runtime
    /// §11.2): missing required evidence and a recorded cleanup failure are
    /// added beside it, never in its place, and make finalization fail.
    pub(crate) fn finish(mut self, outcome: &str, detail: &str) -> EvidenceResult<PathBuf> {
        if !matches!(
            outcome,
            "pass" | "fail" | "timeout" | "infrastructure_failure"
        ) {
            return Err("invalid scenario outcome".into());
        }
        self.finishing = Some((outcome.to_owned(), detail.to_owned()));
        let required: &[&str] = if self.store_expected {
            &[
                "envelopes.ndjson",
                "events.ndjson",
                "daemon.trace",
                "cleanup.json",
                "store.sqlite3",
            ]
        } else {
            &["daemon.trace", "cleanup.json"]
        };
        let missing: Vec<_> = required
            .iter()
            .filter(|name| !self.dir.join(name).exists())
            .copied()
            .collect();
        let mut missing = missing;
        if self.store_expected
            && self.folders_expected
            && fs::read_dir(self.dir.join("evidence"))?.next().is_none()
        {
            missing.push("evidence/*");
        }
        let evidence_failure = (!missing.is_empty())
            .then(|| format!("missing required evidence: {}", missing.join(", ")));
        let failures: Vec<&str> = [&evidence_failure, &self.cleanup_failure]
            .into_iter()
            .flatten()
            .map(String::as_str)
            .collect();
        let mut summary = self.summary(outcome, detail, &missing)?;
        summary["evidence_complete"] = json!(failures.is_empty());
        summary["evidence_failure"] = json!(evidence_failure);
        summary["cleanup_failure"] = json!(self.cleanup_failure);
        fs::write(
            self.dir.join("summary.json"),
            serde_json::to_vec_pretty(&summary)?,
        )?;
        fs::write(
            self.dir.join("REPORT.md"),
            format!(
                "# {}\n\nOutcome: `{outcome}`. {detail}\n\nEvidence complete: {}.\n\n\
                 Missing evidence: {}.\n\nCleanup failure: {}.\n",
                self.scenario,
                if failures.is_empty() { "yes" } else { "no" },
                if missing.is_empty() {
                    "none".to_owned()
                } else {
                    missing.join(", ")
                },
                self.cleanup_failure.as_deref().unwrap_or("none"),
            ),
        )?;
        self.write_manifest()?;
        self.verify_manifest()?;
        self.finalized = true;
        if failures.is_empty() {
            Ok(self.dir.clone())
        } else {
            Err(failures.join("; ").into())
        }
    }

    fn summary(&self, outcome: &str, detail: &str, missing: &[&str]) -> EvidenceResult<Value> {
        let workspace = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let status = command_output("git", &["status", "--porcelain"], &workspace)?;
        // The `via-cli` features this test binary, and so its `via`, was
        // built with; `test-failpoints` is the one there is.
        let features: &[&str] = if cfg!(feature = "test-failpoints") {
            &["test-failpoints"]
        } else {
            &[]
        };
        let mut summary = json!({
            "scenario":self.scenario,
            "outcome":outcome,
            "detail":detail,
            "missing_evidence":missing,
            "seed":0,
            "via_version":env!("CARGO_PKG_VERSION"),
            "git_commit":command_output("git", &["rev-parse", "HEAD"], &workspace)?.trim(),
            "dirty_tree":!status.is_empty(),
            "cargo_lock_sha256":sha256(&workspace.join("Cargo.lock"))?,
            "toolchain":command_output("rustc", &["--version"], &workspace)?.trim(),
            "os":std::env::consts::OS,
            "architecture":std::env::consts::ARCH,
            "features":features,
            "fake_binary_sha256":sha256(&self.fake_binary)?,
            "fixture_sha256":sha256(&self.fixture)?,
            "normalization_version":1,
        });
        if !self.folders_expected {
            summary["folders_expected"] = json!(false);
        }
        if !self.store_expected {
            summary["store_expected"] = json!(false);
        }
        Ok(summary)
    }

    fn write_manifest(&self) -> EvidenceResult {
        let mut files = Vec::new();
        collect_files(&self.dir, &self.dir, &mut files)?;
        files.sort();
        let mut lines = String::new();
        for path in files {
            if path == Path::new("sha256.manifest") {
                continue;
            }
            lines.push_str(&sha256(&self.dir.join(&path))?);
            lines.push_str("  ");
            lines.push_str(path.to_str().ok_or("non-UTF-8 evidence path")?);
            lines.push('\n');
        }
        fs::write(self.dir.join("sha256.manifest"), lines)?;
        Ok(())
    }

    fn verify_manifest(&self) -> EvidenceResult {
        let output = Command::new("sha256sum")
            .args(["--check", "sha256.manifest"])
            .current_dir(&self.dir)
            .output()?;
        if !output.status.success() {
            return Err("evidence manifest verification failed".into());
        }
        Ok(())
    }
}

impl Drop for Evidence {
    /// A best-effort artifact when `finish` did not complete: the scenario's
    /// outcome, if `finish` began with one, with the evidence failure beside
    /// it (runtime §11.2); otherwise, a crash or panic before any outcome,
    /// `infrastructure_failure`.
    fn drop(&mut self) {
        if !self.finalized {
            let (outcome, detail) = self.finishing.clone().unwrap_or_else(|| {
                (
                    "infrastructure_failure".to_owned(),
                    "scenario did not finalize evidence".to_owned(),
                )
            });
            let summary = json!({
                "scenario":self.scenario,
                "outcome":outcome,
                "detail":detail,
                "evidence_complete":false,
                "evidence_failure":"evidence finalization did not complete",
                "cleanup_failure":self.cleanup_failure,
            });
            let _ = fs::write(self.dir.join("summary.json"), summary.to_string());
            let _ = fs::write(
                self.dir.join("REPORT.md"),
                format!(
                    "# {}\n\nOutcome: `{outcome}`. {detail}\n\nEvidence complete: no \
                     (evidence finalization did not complete); see summary.json.\n",
                    self.scenario
                ),
            );
            let _ = self.write_manifest();
        }
    }
}

fn copy_tree(source: &Path, destination: &Path) -> EvidenceResult {
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let target = destination.join(entry.file_name());
        let kind = entry.file_type()?;
        if kind.is_dir() {
            fs::DirBuilder::new().mode(0o700).create(&target)?;
            copy_tree(&entry.path(), &target)?;
        } else if kind.is_file() {
            fs::copy(entry.path(), target)?;
        }
    }
    Ok(())
}

fn collect_files(root: &Path, dir: &Path, output: &mut Vec<PathBuf>) -> EvidenceResult {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if entry.file_type()?.is_dir() {
            collect_files(root, &path, output)?;
        } else if entry.file_type()?.is_file() {
            output.push(path.strip_prefix(root)?.to_owned());
        }
    }
    Ok(())
}

fn command_output(program: &str, args: &[&str], cwd: &Path) -> EvidenceResult<String> {
    let output = Command::new(program).args(args).current_dir(cwd).output()?;
    if !output.status.success() {
        return Err(format!("{program} failed with {}", output.status).into());
    }
    Ok(String::from_utf8(output.stdout)?)
}

fn sha256(path: &Path) -> EvidenceResult<String> {
    let output = Command::new("sha256sum").arg(path).output()?;
    if !output.status.success() {
        return Err(format!("sha256sum failed for {}", path.display()).into());
    }
    let output = String::from_utf8(output.stdout)?;
    Ok(output
        .split_whitespace()
        .next()
        .ok_or("sha256sum returned no digest")?
        .to_owned())
}

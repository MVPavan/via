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

    pub(crate) fn finish(mut self, outcome: &str, detail: &str) -> EvidenceResult<PathBuf> {
        if !matches!(
            outcome,
            "pass" | "fail" | "timeout" | "infrastructure_failure"
        ) {
            return Err("invalid scenario outcome".into());
        }
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
        if self.store_expected && fs::read_dir(self.dir.join("evidence"))?.next().is_none() {
            missing.push("evidence/*");
        }
        let final_outcome = outcome;
        let summary = self.summary(final_outcome, detail, &missing)?;
        fs::write(
            self.dir.join("summary.json"),
            serde_json::to_vec_pretty(&summary)?,
        )?;
        fs::write(
            self.dir.join("REPORT.md"),
            format!(
                "# {}\n\nOutcome: `{final_outcome}`. {detail}\n\nMissing evidence: {}.\n",
                self.scenario,
                if missing.is_empty() {
                    "none".to_owned()
                } else {
                    missing.join(", ")
                }
            ),
        )?;
        self.write_manifest()?;
        self.verify_manifest()?;
        self.finalized = true;
        if missing.is_empty() {
            Ok(self.dir.clone())
        } else {
            Err(format!("missing required evidence: {}", missing.join(", ")).into())
        }
    }

    fn summary(&self, outcome: &str, detail: &str, missing: &[&str]) -> EvidenceResult<Value> {
        let workspace = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let status = command_output("git", &["status", "--porcelain"], &workspace)?;
        // The default harness has no feature switches. The runtime failpoint
        // owner adds feature reporting with the controller in its increment.
        let features: Vec<&str> = Vec::new();
        let mut summary = json!({
            "scenario":self.scenario,
            "outcome":outcome,
            "detail":detail,
            "missing_evidence":missing,
            "evidence_complete":missing.is_empty(),
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
    fn drop(&mut self) {
        if !self.finalized {
            // Best-effort crash/panic artifact; the gate treats it as infrastructure failure.
            let summary = json!({"scenario":self.scenario,"outcome":"infrastructure_failure","detail":"scenario did not finalize evidence"});
            let _ = fs::write(self.dir.join("summary.json"), summary.to_string());
            let _ = fs::write(
                self.dir.join("REPORT.md"),
                "Scenario did not finalize evidence; see summary.json.\n",
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

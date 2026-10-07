//! Boolean-only OC12/`OC12b` leak detection (`opencode.md` §4.3 and §13).

use std::fs;
use std::path::Path;

use serde_json::Value;

use crate::scenario::ScenarioError;

/// Synthetic needles stay in memory; failures never include bytes or payloads (§4.3).
pub(super) struct Scan {
    needles: Vec<Vec<u8>>,
}

impl Scan {
    /// Keep synthetic needles in memory, never in diagnostics (§13 `OC12b`).
    pub(super) fn new(needles: Vec<Vec<u8>>) -> Self {
        Self { needles }
    }

    /// Add the actual owned server password without writing it (§13 OC12).
    pub(super) fn add(&mut self, needle: Vec<u8>) -> Result<(), ScenarioError> {
        if needle.is_empty() {
            return Err(failure("empty secrecy needle"));
        }
        self.needles.push(needle);
        Ok(())
    }

    /// Boolean-only detection; the matching needle is never returned (§4.3).
    pub(super) fn contains(&self, bytes: &[u8]) -> bool {
        self.needles.iter().any(|needle| {
            !needle.is_empty() && bytes.windows(needle.len()).any(|part| part == needle)
        })
    }

    /// Refuse a leaking surface with fixed, VIA-owned test detail (§4.3).
    pub(super) fn bytes(&self, bytes: &[u8]) -> Result<(), ScenarioError> {
        if self.contains(bytes) {
            return Err(failure("synthetic secret present in VIA surface"));
        }
        Ok(())
    }

    /// Check a complete C1 value without printing it (§13 OC12/`OC12b`).
    pub(super) fn value(&self, value: &Value) -> Result<(), ScenarioError> {
        let bytes = serde_json::to_vec(value)
            .map_err(|_| failure("could not encode secrecy scan surface"))?;
        self.bytes(&bytes)
    }

    /// Scan regular VIA-owned files, including raw SQLite bytes; never follow links (§4.3).
    pub(super) fn tree(&self, root: &Path) -> Result<(), ScenarioError> {
        if !root.exists() {
            return Ok(());
        }
        for entry in fs::read_dir(root).map_err(|_| failure("secrecy scan directory failed"))? {
            let entry = entry.map_err(|_| failure("secrecy scan entry failed"))?;
            let kind = entry
                .file_type()
                .map_err(|_| failure("secrecy scan file type failed"))?;
            if kind.is_dir() {
                self.tree(&entry.path())?;
            } else if kind.is_file() {
                let name = entry.file_name();
                if name == "stderr.log" || name == "undecoded.bin" {
                    return Err(failure("forbidden OpenCode payload capture exists"));
                }
                let bytes =
                    fs::read(entry.path()).map_err(|_| failure("secrecy scan file read failed"))?;
                self.bytes(&bytes)?;
            }
        }
        Ok(())
    }
}

/// Only static test descriptions are passed; no vendor payload (§4.3).
pub(super) fn failure(message: &str) -> ScenarioError {
    ScenarioError::Failure(message.to_owned())
}

/// The positive control never creates a secret-bearing evidence file (§13 `OC12b`).
#[test]
fn oc12b_boolean_scanner_detects_in_memory_positive_control() {
    let sentinel = b"synthetic-memory-only-provider-sentinel".to_vec();
    let scan = Scan::new(vec![sentinel.clone()]);
    assert!(scan.contains(&sentinel));
    assert!(!scan.contains(b"VIA-owned diagnostic"));
    assert!(scan.bytes(&sentinel).is_err());
}

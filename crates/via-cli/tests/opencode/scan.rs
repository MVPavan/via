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

    /// Track an owned server password for OC12 (opencode.md §13).
    pub(super) fn add_password(&mut self, password: &[u8]) -> Result<(), ScenarioError> {
        self.add(password.to_vec())?;
        let mut credentials = b"opencode:".to_vec();
        credentials.extend_from_slice(password);
        let encoded = base64(&credentials);
        // OC12: ignore the last quartet to catch truncated/padding-altered headers.
        self.add(encoded[..encoded.len() - 4].to_vec())?;
        self.add(encoded)
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
            return Err(failure("secrecy scan root missing"));
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

/// Local RFC 4648 encoder for the OC12 HTTP Basic credential representation (§13).
fn base64(bytes: &[u8]) -> Vec<u8> {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut encoded = Vec::new();
    for group in bytes.chunks(3) {
        let n = (u32::from(group[0]) << 16)
            | (u32::from(group.get(1).copied().unwrap_or(0)) << 8)
            | u32::from(group.get(2).copied().unwrap_or(0));
        for (index, shift) in [18, 12, 6, 0].into_iter().enumerate() {
            encoded.push(if index <= group.len() {
                ALPHABET[((n >> shift) & 63) as usize]
            } else {
                b'='
            });
        }
    }
    encoded
}

/// Independent RFC 4648 vectors keep the password representation test honest (§13 OC12).
#[test]
fn oc12_basic_encoder_matches_independent_vectors() {
    for (input, encoded) in [
        ("", ""),
        ("f", "Zg=="),
        ("fo", "Zm8="),
        ("foo", "Zm9v"),
        ("foob", "Zm9vYg=="),
        ("fooba", "Zm9vYmE="),
        ("foobar", "Zm9vYmFy"),
    ] {
        assert!(
            base64(input.as_bytes()) == encoded.as_bytes(),
            "Basic encoder mismatch"
        );
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

/// OC12: both complete and truncated Basic headers must fail a file scan (§13).
#[test]
fn oc12_password_scanner_rejects_planted_basic_headers() -> crate::daemon::TestResult {
    use std::os::unix::fs::PermissionsExt as _;
    let root = tempfile::Builder::new()
        .permissions(fs::Permissions::from_mode(0o700))
        .tempdir()?;
    let password = b"0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    // Independent encoded fixture, not the encoder under test.
    let encoded = b"b3BlbmNvZGU6MDEyMzQ1Njc4OWFiY2RlZjAxMjM0NTY3ODlhYmNkZWYwMTIzNDU2Nzg5YWJjZGVmMDEyMzQ1Njc4OWFiY2RlZg==";
    let mut scan = Scan::new(Vec::new());
    scan.add_password(password)?;
    let raw_only = Scan::new(vec![password.to_vec()]);
    let full_only = Scan::new(vec![password.to_vec(), encoded.to_vec()]);
    let file = root.path().join("headers.txt");
    fs::write(&file, b"VIA-owned metadata")?;
    assert!(scan.tree(root.path()).is_ok());
    for value in [encoded.as_slice(), &encoded[..encoded.len() - 4]] {
        let mut header = b"Authorization: Basic ".to_vec();
        header.extend_from_slice(value);
        fs::write(&file, header)?;
        assert!(
            raw_only.tree(root.path()).is_ok(),
            "raw-only control found encoded bytes"
        );
        if value.len() < encoded.len() {
            assert!(
                full_only.tree(root.path()).is_ok(),
                "full-only control found truncated bytes"
            );
        }
        assert!(
            scan.tree(root.path()).is_err(),
            "planted Basic header escaped the scan"
        );
    }
    fs::write(&file, b"VIA-owned metadata")?;
    assert!(scan.tree(root.path()).is_ok());
    Ok(())
}

/// §4.3: a missing scan root is missing coverage, never a clean tree.
#[test]
fn oc12b_scanner_refuses_a_missing_root() -> crate::daemon::TestResult {
    use std::os::unix::fs::PermissionsExt as _;
    let root = tempfile::Builder::new()
        .permissions(fs::Permissions::from_mode(0o700))
        .tempdir()?;
    let scan = Scan::new(Vec::new());
    assert!(
        scan.tree(&root.path().join("absent")).is_err(),
        "missing root passed the scan"
    );
    Ok(())
}

/// §4.3: nested files and SQLite WAL bytes are observable scan surfaces.
#[test]
fn oc12b_scanner_rejects_nested_and_wal_file_positive_controls() -> crate::daemon::TestResult {
    use std::os::unix::fs::PermissionsExt as _;
    let root = tempfile::Builder::new()
        .permissions(fs::Permissions::from_mode(0o700))
        .tempdir()?;
    let needle = b"synthetic-file-only-provider-sentinel";
    let scan = Scan::new(vec![needle.to_vec()]);
    let nested = root.path().join("nested");
    crate::daemon::private_dir(&nested)?;
    let file = nested.join("observation.json");
    fs::write(&file, b"VIA-owned metadata")?;
    assert!(scan.tree(root.path()).is_ok());
    fs::write(&file, needle)?;
    assert!(
        scan.tree(root.path()).is_err(),
        "nested planted secret escaped the scan"
    );
    fs::remove_file(file)?;
    assert!(scan.tree(root.path()).is_ok());
    let wal = root.path().join("store.sqlite3-wal");
    fs::write(&wal, b"VIA-owned metadata")?;
    assert!(scan.tree(root.path()).is_ok());
    fs::write(wal, needle)?;
    assert!(
        scan.tree(root.path()).is_err(),
        "WAL planted secret escaped the scan"
    );
    Ok(())
}

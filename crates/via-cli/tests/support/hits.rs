//! Counting a failpoint's hits without acting on any (design §10): a
//! command under another token is refused at every hit, and each refusal
//! leaves `<point>.<occurrence>.refused`, so a test can arm exactly the
//! next occurrence of a point hit an unknown number of times before.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

/// Starts counting `point` in the failpoint directory `dir`; write it before
/// the daemon's first hit. Arming the point replaces it and clears the
/// markers.
pub(crate) fn count(dir: &Path, point: &str) -> Result<(), String> {
    let temporary = dir.join(format!(".{point}.json.tmp"));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temporary)
        .map_err(|error| error.to_string())?;
    file.write_all(br#"{"token":"counting","occurrence":1,"action":"pause"}"#)
        .and_then(|()| file.sync_all())
        .map_err(|error| error.to_string())?;
    fs::rename(&temporary, dir.join(format!("{point}.json"))).map_err(|error| error.to_string())
}

/// The hits of `point` counted so far.
pub(crate) fn hits(dir: &Path, point: &str) -> Result<u64, String> {
    let prefix = format!("{point}.");
    let mut most = 0;
    for entry in fs::read_dir(dir).map_err(|error| error.to_string())? {
        let name = entry.map_err(|error| error.to_string())?.file_name();
        let occurrence = name
            .to_string_lossy()
            .strip_prefix(&prefix)
            .and_then(|rest| rest.strip_suffix(".refused"))
            .and_then(|number| number.parse::<u64>().ok());
        if let Some(occurrence) = occurrence {
            most = most.max(occurrence);
        }
    }
    Ok(most)
}

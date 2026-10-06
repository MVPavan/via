//! x.3.2 X5 (X0 item 9.2): the measured peak of one maximal Codex decode
//! against the design's `DECODE_ALLOWANCE`, two maximal messages (an
//! escaped string's scratch and its owned copy) plus 65,536 nodes at 64 B
//! each (20 MiB), which `codex_rss_leases` charges
//! per session for the normalizer's decode in flight. Each shape a server
//! line can reach within the Codex route's 8 MiB message
//! ([`MESSAGE_BYTES`], via-5lr.3.5) and the 65,536-node structure limit
//! is decoded in a fresh child process (no freed page of
//! an earlier decode to reuse): the line is built, the peak RSS is reset
//! (`/proc/self/clear_refs`), and the peak (`VmHWM`) less the RSS before
//! `decode` is its measured peak. The workspace forbids `unsafe`, so no
//! counting allocator. RSS is an estimate, not a bound: it counts pages
//! touched, to the kernel's counter granularity (256 KiB here), and an
//! allocation that reuses a page already resident is not counted (on
//! glibc the child fixes the mmap threshold to narrow that).
#![cfg(target_os = "linux")]
#![expect(
    clippy::unwrap_used,
    reason = "test fixtures and assertions fail loudly"
)]

use std::{env, fs, process::Command};

use via_routes::codex::{MESSAGE_BYTES, decode};

/// The design's allowance (X0 item 9.2 table; review cfix-1 #2): two
/// maximal messages, as a string with an escape is unescaped into serde's
/// scratch buffer and then copied into the owned string while the scratch
/// is held, plus 65,536 nodes at 64 B: 20 MiB.
const DECODE_ALLOWANCE: u64 = 2 * MESSAGE_BYTES as u64 + 65_536 * 64;

/// The longest line the Codex route admits, its LF excluded.
const LINE: usize = MESSAGE_BYTES - 1;

/// The child's shape, by index.
const SHAPE: &str = "VIA_DECODE_PEAK_SHAPE";

/// `template` with `FILL` replaced by `unit` repeated until the line is as
/// long as it can be within [`LINE`].
fn filled(template: &str, unit: &str) -> String {
    let room = LINE - (template.len() - "FILL".len());
    template.replace("FILL", &unit.repeat(room / unit.len()))
}

/// A `/proc/self/status` field in bytes.
fn status(field: &str) -> u64 {
    let status = fs::read_to_string("/proc/self/status").unwrap();
    let kib: u64 = status
        .lines()
        .find_map(|line| line.strip_prefix(field))
        .and_then(|rest| rest.trim().trim_end_matches("kB").trim().parse().ok())
        .unwrap();
    kib * 1024
}

/// The child's measure: the peak RSS during `decode(line)` above the RSS
/// before it.
fn measured(line: &str) -> u64 {
    fs::write("/proc/self/clear_refs", "5").unwrap();
    let before = status("VmRSS:");
    let decoded = decode(line.as_bytes());
    let peak = status("VmHWM:").saturating_sub(before);
    assert!(decoded.is_ok(), "{:?}", decoded.err());
    drop(decoded);
    peak
}

/// The maximal shapes: the longest owned string each decoded member can
/// keep, and the most nodes a decoded list can hold.
fn shapes() -> Vec<(&'static str, String)> {
    let ids = r#""threadId":"019a0000-0000-7000-8000-000000100001","turnId":"019a0000-0000-7000-8000-000000200001""#;
    let item = |body: &str| {
        format!(
            r#"{{"method":"item/completed","params":{{"item":{{{body}}},{ids},"completedAtMs":1}}}}"#
        )
    };
    vec![
        (
            "agentMessage final_answer text",
            filled(
                &item(r#""type":"agentMessage","id":"m","text":"FILL","phase":"final_answer""#),
                "x",
            ),
        ),
        (
            // Review cfix-1 #2: one escape makes serde unescape into its
            // scratch buffer, then copy the text out: both are held.
            "agentMessage final_answer text, escaped",
            filled(
                &item(r#""type":"agentMessage","id":"m","text":"\nFILL","phase":"final_answer""#),
                "x",
            ),
        ),
        (
            "agentMessage delta",
            filled(
                &format!(
                    r#"{{"method":"item/agentMessage/delta","params":{{{ids},"itemId":"m","delta":"FILL"}}}}"#
                ),
                "x",
            ),
        ),
        (
            "fileChange changes, most nodes",
            item(&format!(
                r#""type":"fileChange","id":"f","status":"completed","changes":[{}{{"path":"p"}}]"#,
                r#"{"path":"p"},"#.repeat(21_820)
            )),
        ),
        (
            "fileChange changes, most nodes and bytes",
            item(&format!(
                r#""type":"fileChange","id":"f","status":"completed","changes":[{}{{"path":"p"}}]"#,
                format!(r#"{{"path":"{}"}},"#, "p".repeat(34)).repeat(21_820)
            )),
        ),
        (
            "turn/completed error message",
            filled(
                r#"{"method":"turn/completed","params":{"threadId":"t","turn":{"id":"u","status":"failed","items":[],"error":{"message":"FILL","codexErrorInfo":null,"additionalDetails":null}}}}"#,
                "x",
            ),
        ),
        (
            "turn/completed items, most nodes",
            format!(
                r#"{{"method":"turn/completed","params":{{"threadId":"t","turn":{{"id":"u","status":"completed","items":[{}0]}}}}}}"#,
                "0,".repeat(65_520)
            ),
        ),
        (
            "response result",
            filled(r#"{"id":7,"result":{"text":"FILL"}}"#, "x"),
        ),
        (
            "unknown notification",
            filled(
                r#"{"method":"thread/name/updated","params":{"threadId":"t","name":"FILL"}}"#,
                "x",
            ),
        ),
    ]
}

/// X0 item 9.2: one maximal decode's measured peak, reported against
/// `DECODE_ALLOWANCE`; above it is a design review, not a silent fix.
#[test]
#[expect(
    clippy::print_stdout,
    reason = "the child reports its measure; the parent records each"
)]
fn codex_decode_peak_within_allowance() {
    let shapes = shapes();
    if let Ok(index) = env::var(SHAPE) {
        let (name, line) = &shapes[index.parse::<usize>().unwrap()];
        assert!(line.len() <= LINE, "{name}: {} bytes", line.len());
        let scanned = via_wire::json_limits::scan(line.as_bytes());
        assert!(scanned.is_ok(), "{name}: {scanned:?}");
        let peak = measured(line);
        println!(
            "measured {peak} {} {}",
            line.len(),
            scanned.map_or(0, |scanned| scanned.nodes)
        );
        return;
    }
    let mut worst = (0, "");
    for (index, (name, _)) in shapes.iter().enumerate() {
        let output = Command::new(env::current_exe().unwrap())
            .args([
                "--exact",
                "codex_decode_peak_within_allowance",
                "--nocapture",
            ])
            .env(SHAPE, index.to_string())
            // glibc: a fixed mmap threshold, so no buffer of 128 KiB or
            // more reuses a page an earlier free left resident.
            .env("MALLOC_MMAP_THRESHOLD_", "131072")
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success(),
            "{name}: {stdout}{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let fields: Vec<u64> = stdout
            .lines()
            .find_map(|line| line.strip_prefix("measured "))
            .unwrap_or_else(|| panic!("{name}: {stdout}"))
            .split(' ')
            .map(|field| field.parse().unwrap())
            .collect();
        let [peak, bytes, nodes] = fields[..] else {
            panic!("{name}: {stdout}");
        };
        println!("decode peak {name}: {peak} B over the RSS before; line {bytes} B, {nodes} nodes");
        if peak > worst.0 {
            worst = (peak, *name);
        }
    }
    println!(
        "decode peak worst: {} B ({}) against DECODE_ALLOWANCE {DECODE_ALLOWANCE} B",
        worst.0, worst.1
    );
    assert!(
        worst.0 <= DECODE_ALLOWANCE,
        "{} decodes with a {} B peak, over DECODE_ALLOWANCE {DECODE_ALLOWANCE} B",
        worst.1,
        worst.0
    );
}

/// The peek child's shape, by index.
const PEEK_SHAPE: &str = "VIA_PEEK_PEAK_SHAPE";

/// The most the routing peek may add to RSS (review cfix-crit #1): it
/// borrows from the line and builds no value of the vendor's choosing, so
/// only counter granularity and small fixed buffers remain.
const PEEK_ALLOWANCE: u64 = 1024 * 1024;

/// Lines whose correlation members are as large as an admitted line
/// allows, in each place the peek reads one; past the structure limits,
/// as the peek runs before any full decode.
fn peek_shapes() -> Vec<(&'static str, String)> {
    vec![
        (
            "response id, an array",
            filled(r#"{"id":[FILL0],"result":null}"#, "0,"),
        ),
        (
            "request id, an array",
            filled(r#"{"id":[FILL0],"method":"x/y"}"#, "0,"),
        ),
        (
            "request threadId, an array",
            filled(
                r#"{"id":1,"method":"x/y","params":{"threadId":[FILL0]}}"#,
                "0,",
            ),
        ),
        (
            "notification threadId, an array",
            filled(
                r#"{"method":"item/completed","params":{"threadId":[FILL0],"turnId":"u"}}"#,
                "0,",
            ),
        ),
        (
            "turn/completed turn.id, an array",
            filled(
                r#"{"method":"turn/completed","params":{"threadId":"t","turn":{"id":[FILL0]}}}"#,
                "0,",
            ),
        ),
        (
            "unknown notification, a wide array",
            filled(
                r#"{"method":"x/unknown","params":{"threadId":"t","many":[FILL0]}}"#,
                "0,",
            ),
        ),
    ]
}

/// Review cfix-crit #1: the routing peek of an admitted line never builds
/// a value of the vendor's choosing: its measured peak stays within
/// [`PEEK_ALLOWANCE`] for each shape, a 4-million-element correlation
/// member included.
#[test]
#[expect(
    clippy::print_stdout,
    reason = "the child reports its measure; the parent records each"
)]
fn codex_peek_peak_within_allowance() {
    let shapes = peek_shapes();
    if let Ok(index) = env::var(PEEK_SHAPE) {
        let (name, line) = &shapes[index.parse::<usize>().unwrap()];
        assert!(line.len() <= LINE, "{name}: {} bytes", line.len());
        fs::write("/proc/self/clear_refs", "5").unwrap();
        let before = status("VmRSS:");
        let peeked = via_routes::codex::peek(line.as_bytes());
        let peak = status("VmHWM:").saturating_sub(before);
        drop(peeked);
        println!("measured {peak}");
        return;
    }
    for (index, (name, _)) in shapes.iter().enumerate() {
        let output = Command::new(env::current_exe().unwrap())
            .args(["--exact", "codex_peek_peak_within_allowance", "--nocapture"])
            .env(PEEK_SHAPE, index.to_string())
            .env("MALLOC_MMAP_THRESHOLD_", "131072")
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success(),
            "{name}: {stdout}{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let peak: u64 = stdout
            .lines()
            .find_map(|line| line.strip_prefix("measured "))
            .unwrap_or_else(|| panic!("{name}: {stdout}"))
            .trim()
            .parse()
            .unwrap();
        println!("peek peak {name}: {peak} B over the RSS before");
        assert!(
            peak <= PEEK_ALLOWANCE,
            "{name}: the peek's peak is {peak} B, over {PEEK_ALLOWANCE} B"
        );
    }
}

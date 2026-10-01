//! C2 conformance cases for the Claude Code adapter, red by construction
//! (`via-p98.3.1`; adapters design §6 step 6, §7).
//!
//! Each case pairs `crates/via-adapters/tests/fixtures/claude/<case>.replay.json`
//! (the recorded vendor side, replayed by `via-fake-agent`) with
//! `<case>.expect.json` (what the C2 driver must produce, in the unified
//! expectation schema checked by the shared `support/conformance_expect.rs`).
//! [`drive`] is the one seam: today it returns `adapter not implemented`, so
//! every case is ignored; `via-p98.3.2` replaces it with a driver over the
//! real adapter and removes the `ignore`s.
//!
//! [`VENDOR_RECORDS`] are fixtures with an expectation that are not adapter
//! conformance cases: they have no case test, but their expectation still
//! validates, and `via-fake-agent`'s `fixtures.rs` still replays and scans
//! them.
//!
//! # Fixture argv
//!
//! The argv is the adapter's own recipe (vendor packet §4) in this order:
//! `-p --input-format stream-json --output-format stream-json --verbose
//! --model M (--session-id {capture sid} | --resume {capture resume})
//! --restricted --strict-mcp-config --permission-mode dontAsk
//! --permission-prompts none --tools T --allowedTools T`, then
//! `--append-system-prompt`, `--effort` and `--json-schema` (compact, sorted
//! keys) when set. For a resume, the driver passes `sessions.main.resume`
//! and asserts the fake's captured `--resume` value equals it.

#[path = "support/conformance_expect.rs"]
mod conformance_expect;

use std::path::{Path, PathBuf};

use conformance_expect::Outcome;
use serde_json::{Value, json};

/// The bead whose adapter makes these cases pass.
const ADAPTER_BEAD: &str = "via-p98.3.2";

/// Vendor-behaviour records: fixtures with an expectation file that are not
/// adapter conformance cases, so they have no case test.
const VENDOR_RECORDS: [(&str, &str); 1] = [(
    "c10_early_eof",
    "stdin EOF right after the prompt still completes the turn: the evidence \
     behind AD15/VC7. No C2 verb makes a correct adapter close stdin early; \
     c7's AD19 order pins the adapter side",
)];

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../via-adapters/tests/fixtures/claude")
}

/// Runs the case's sessions and turns through the Claude C2 driver against
/// the fake agent replaying `<case>.replay.json`, and collects the outcome
/// (see the checker's module docs for its obligations: `launches` from
/// `<case>.launches`, the replay's exit status, and stdin EOF after the
/// result at an `await_eof` step). Replaced by `via-p98.3.2`.
fn drive(name: &str, _expect: &Value, _replay: &Path) -> Result<Outcome, String> {
    Err(format!(
        "adapter not implemented: {ADAPTER_BEAD} (case {name})"
    ))
}

fn check(name: &str) -> Result<(), String> {
    let dir = fixtures();
    let expect = conformance_expect::load(&dir, name)?;
    let outcome = drive(name, &expect, &dir.join(format!("{name}.replay.json")))?;
    conformance_expect::check(&expect, &outcome).map_err(|wrong| format!("{name}:\n{wrong}"))
}

macro_rules! cases {
    ($($name:ident),* $(,)?) => {
        /// Every case with a test below.
        const CASES: &[&str] = &[$(stringify!($name)),*];

        mod conformance_claude_cases {
            $(
                #[test]
                #[ignore = "red until via-p98.3.2"]
                fn $name() {
                    super::check(stringify!($name)).unwrap();
                }
            )*
        }
    };
}

cases! {
    c0_isolated,
    c0_bad_model,
    c0_bad_effort,
    c0_invalid_resume,
    c1a,
    c1b,
    c1c,
    c1b_resume_mismatch,
    c3_queue,
    c4_never_ask,
    c5_read_only,
    c7_interrupt,
    c9a,
    c9b,
    c11b_stdio_prompt,
}

/// Every case and vendor record, sorted.
fn listed() -> Vec<&'static str> {
    let mut listed: Vec<&str> = CASES.to_vec();
    listed.extend(VENDOR_RECORDS.iter().map(|(name, _)| *name));
    listed.sort_unstable();
    listed
}

/// Green now: every expectation file is a case test or a vendor record (not
/// both, each record with its reason), has a replay fixture, names this
/// harness and validates against the unified schema.
#[test]
fn conformance_claude_cases_match_fixture_files() {
    for (name, why) in VENDOR_RECORDS {
        assert!(!why.is_empty(), "{name}: a vendor record needs its reason");
        assert!(
            !CASES.contains(&name),
            "{name}: both a case and a vendor record"
        );
    }
    let dir = fixtures();
    assert_eq!(conformance_expect::case_names(&dir).unwrap(), listed());
    for name in listed() {
        let expect = conformance_expect::load(&dir, name).unwrap();
        assert_eq!(expect["harness"], "claude", "{name}: harness");
        conformance_expect::validate(&expect).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert!(
            dir.join(format!("{name}.replay.json")).is_file(),
            "{name}: replay fixture missing"
        );
    }
}

/// A named change to an ideal outcome.
type Mutation = (&'static str, fn(&mut Outcome));

/// Green now: the checker accepts the ideal outcome of every case and
/// reports a change in any part of it the case states.
#[test]
fn conformance_claude_checker_detects_each_difference() {
    let dir = fixtures();
    for name in CASES {
        let expect = conformance_expect::load(&dir, name).unwrap();
        conformance_expect::check(&expect, &conformance_expect::ideal(&expect))
            .unwrap_or_else(|e| panic!("{name}: ideal outcome refused:\n{e}"));
        let mutations: [Mutation; 9] = [
            ("usage", |o| {
                o.turns[0].usage = Some(json!({"input_tokens": 1}));
            }),
            ("final_text", |o| {
                o.turns[0].final_text = Some(vec!["other".to_owned()]);
            }),
            ("steer", |o| o.turns[0].steer.push("injected".to_owned())),
            ("launches", |o| o.launches += 1),
            ("accepted", |o| o.turns[0].accepted = !o.turns[0].accepted),
            ("terminal", |o| {
                o.turns[0].terminal = Some(json!({"status": "other"}));
            }),
            ("stop_facts", |o| {
                o.turns[0].stop_facts = Some(json!({"acknowledged": false}));
            }),
            ("observations", |o| {
                o.turns[0].observations.push("turn.accepted".to_owned());
                o.turns[0].observations.push("action.denied".to_owned());
            }),
            ("turns", |o| {
                o.turns.pop();
            }),
        ];
        for (what, mutate) in mutations {
            let mut outcome = conformance_expect::ideal(&expect);
            mutate(&mut outcome);
            let first = &expect["turns"][0]["expect"];
            let stated = match what {
                "launches" => expect.get("launches").is_some(),
                "observations" => {
                    first.get("observation_counts").is_some()
                        || first.get("observations_exclude").is_some()
                }
                "turns" | "steer" => true,
                field => first.get(field).is_some(),
            };
            if stated {
                assert!(
                    conformance_expect::check(&expect, &outcome).is_err(),
                    "{name}: a changed {what} passed"
                );
            }
        }
    }
}

/// Green: `resume_mismatch` is an `AdapterError` kind (C2 §2 identity), and
/// a start rejection is never stated as an error: it is `rejected`.
#[test]
fn conformance_claude_error_kinds_follow_c2() {
    let base = conformance_expect::load(&fixtures(), "c1b_resume_mismatch").unwrap();
    assert_eq!(base["turns"][0]["expect"]["error"], "resume_mismatch");
    conformance_expect::validate(&base).unwrap();
    for wrong in ["rejected", "session_gone", "resume.mismatch"] {
        let mut expect = base.clone();
        expect["turns"][0]["expect"]["error"] = json!(wrong);
        let refused = conformance_expect::validate(&expect);
        assert!(
            refused.as_ref().is_err_and(|e| e.contains("expect.error:")),
            "error {wrong}: {refused:?}"
        );
    }
}

/// Green: an accepted turn that asserts identity confirmation must order it
/// before acceptance (C2 §2 identity), so an expectation cannot drop the
/// order and still validate.
#[test]
fn conformance_claude_identity_order_is_required() {
    let base = conformance_expect::load(&fixtures(), "c1a").unwrap();
    conformance_expect::validate(&base).unwrap();
    for order in [json!([]), json!(["turn.accepted"])] {
        let mut expect = base.clone();
        expect["turns"][0]["expect"]["observations_order"] = order.clone();
        let refused = conformance_expect::validate(&expect);
        assert!(
            refused.as_ref().is_err_and(|e| e.contains("must precede")),
            "order {order}: {refused:?}"
        );
    }
}

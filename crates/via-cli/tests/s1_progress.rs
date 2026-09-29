//! Task 4 R2–R5 (design §2–§4.2, §11.3) through the real `via` binary and
//! daemon: vendor text, tool and usage messages are not events; they feed
//! the in-memory progress snapshot and one `steps` row per model step, and
//! `status` describes one moment of one turn with a single Store read.
//! Every scenario activates the failpoint controller. Written before the
//! observation set, the step tracker, the step rows and `status`.
#![cfg(feature = "test-failpoints")]

#[path = "support/daemon.rs"]
#[expect(dead_code, reason = "shared support; this file uses part of it")]
mod daemon;
#[path = "support/failpoints.rs"]
#[expect(dead_code, reason = "shared support; this file uses part of it")]
mod failpoints;
#[path = "support/hits.rs"]
mod hits;
#[path = "support/outer_cleanup.rs"]
mod outer_cleanup;
#[path = "support/scenario.rs"]
mod scenario;
mod support;

use std::fs;
use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant};

use daemon::{Daemon, Raw, Sandbox, TestResult, cli, events, failure, infra, request};
use failpoints::Failpoints;
use scenario::{ScenarioError, collect_available, run_scenario};
use serde_json::{Value, json};
use support::evidence::Evidence;

const HANDLE: &str = "h_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
const STEP: &str = "store.commit.step";
const PUBLISH: &str = "core.progress.publish";
const FINISH: &str = "core.finish_running.pause";
const OBSERVE: &str = "core.observations.pause";
const READ_DELAY: &str = "store.read.delay_ms";
const EXIT: &str = "wire.exit.observed";

// ---------------------------------------------------------------- fixtures

fn vendor_turn(turn: u32) -> String {
    format!("fake-turn-{turn}")
}

fn emit(message: &Value) -> Value {
    json!({"action":"emit","message":message})
}

fn accepted(turn: u32) -> Value {
    emit(&json!({"type":"accepted","id":1,"vendor_turn_id":vendor_turn(turn)}))
}

fn text(turn: u32) -> Value {
    json!({"type":"text","vendor_turn_id":vendor_turn(turn),"text":"model output"})
}

fn tool_started(turn: u32, id: &str, name: &str) -> Value {
    json!({"type":"tool_started","vendor_turn_id":vendor_turn(turn),"tool_id":id,"name":name,
        "input_summary":"input"})
}

fn tool_ended(turn: u32, id: &str) -> Value {
    json!({"type":"tool_ended","vendor_turn_id":vendor_turn(turn),"tool_id":id,
        "status":"completed","output_summary":"output"})
}

fn usage(turn: u32, total: u64) -> Value {
    json!({"type":"usage","vendor_turn_id":vendor_turn(turn),"total_tokens":total})
}

fn completed(turn: u32) -> Value {
    emit(
        &json!({"type":"terminal","vendor_turn_id":vendor_turn(turn),"status":"completed",
        "final_text":"done","stop_reason":"end_turn"}),
    )
}

fn gate(name: &str) -> Value {
    json!({"action":"gate","name":name})
}

/// Emits `messages` in order.
fn emits(messages: &[Value]) -> Vec<Value> {
    messages.iter().map(emit).collect()
}

/// One tool round that ends the current step at its next model output.
fn tool_round(turn: u32, id: &str, name: &str) -> Vec<Value> {
    emits(&[tool_started(turn, id, name), tool_ended(turn, id)])
}

fn script(turn: u32, prompt: &str, steps: &[Value]) -> Value {
    json!({"expected_request":{"type":"start","id":1,"turn":turn,"prompt":prompt},"steps":steps})
}

fn scripts(scripts: &[Value]) -> Value {
    json!({ "scripts": scripts })
}

fn check(condition: bool, detail: impl FnOnce() -> String) -> Result<(), ScenarioError> {
    if condition {
        Ok(())
    } else {
        Err(failure(detail()))
    }
}

// ----------------------------------------------------------------- harness

/// One scenario's deployment: the sandbox and its failpoint directory.
struct Setup {
    sandbox: Sandbox,
    failpoints: Failpoints,
    dir: PathBuf,
}

impl Setup {
    fn new(fixture: &Value) -> TestResult<Self> {
        let sandbox = Sandbox::new(fixture)?;
        let root = sandbox
            .state
            .parent()
            .ok_or("sandbox state has no parent")?
            .to_owned();
        let failpoints = Failpoints::new(&root)?;
        let dir = root.join("failpoints");
        Ok(Self {
            sandbox,
            failpoints,
            dir,
        })
    }

    fn evidence(&self, name: &str) -> TestResult<Evidence> {
        Evidence::new(name, &self.sandbox.fake, &self.sandbox.fixture)
    }

    fn start(&self, evidence: &Evidence) -> Result<Daemon<'_>, ScenarioError> {
        Daemon::start_with(&self.sandbox, evidence, |command| {
            self.failpoints.activate(command);
        })
    }

    /// [`Self::start`] with the observation stall lowered to 500 ms.
    fn start_stalling(&self, evidence: &Evidence) -> Result<Daemon<'_>, ScenarioError> {
        Daemon::start_with(&self.sandbox, evidence, |command| {
            self.failpoints.activate(command);
            command.env("VIA_TEST_EVENT_STALL_MS", "500");
        })
    }

    /// Scenario cleanup: records every stored envelope and event as
    /// evidence, then collects the Store and the evidence folders.
    fn collect(&self, evidence: &Evidence) -> Result<(), ScenarioError> {
        let path = self.sandbox.state.join("store.sqlite3");
        let (mut envelopes, mut events) = (String::new(), String::new());
        if path.is_file() {
            let store = rusqlite::Connection::open_with_flags(
                &path,
                rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
            )
            .map_err(infra)?;
            for (sql, out) in [
                (
                    "SELECT envelope FROM turns WHERE envelope IS NOT NULL \
                     ORDER BY session_id,number",
                    &mut envelopes,
                ),
                (
                    "SELECT event FROM events ORDER BY session_id,seq",
                    &mut events,
                ),
            ] {
                let mut statement = store.prepare(sql).map_err(infra)?;
                let rows = statement
                    .query_map([], |row| row.get::<_, String>(0))
                    .map_err(infra)?;
                for row in rows {
                    out.push_str(&row.map_err(infra)?);
                    out.push('\n');
                }
            }
        }
        evidence
            .write("envelopes.ndjson", envelopes.as_bytes())
            .map_err(infra)?;
        evidence
            .write("events.ndjson", events.as_bytes())
            .map_err(infra)?;
        collect_available(evidence, &self.sandbox.state)
    }

    /// Spawns a fake session with `prompt`; returns its receipt.
    fn spawn(&self, evidence: &Evidence, prompt: &str) -> Result<Value, ScenarioError> {
        cli(
            &self.sandbox,
            evidence,
            &format!("spawn_{prompt}"),
            &[
                "spawn",
                "--harness",
                "fake",
                "--model",
                "fake",
                "--prompt",
                prompt,
                "--handle",
                HANDLE,
                "--wall-ms",
                "120000",
                "--background",
                "--json",
            ],
        )
    }

    fn session(&self, evidence: &Evidence, prompt: &str) -> Result<String, ScenarioError> {
        let receipt = self.spawn(evidence, prompt)?;
        receipt["session_id"]
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| failure(format!("receipt has no session: {receipt}")))
    }

    fn resume(
        &self,
        evidence: &Evidence,
        session: &str,
        prompt: &str,
    ) -> Result<Value, ScenarioError> {
        cli(
            &self.sandbox,
            evidence,
            &format!("resume_{prompt}"),
            &[
                "resume",
                session,
                "--prompt",
                prompt,
                "--handle",
                HANDLE,
                "--wall-ms",
                "120000",
                "--json",
            ],
        )
    }

    fn wait(&self, evidence: &Evidence, address: &str) -> Result<Value, ScenarioError> {
        cli(
            &self.sandbox,
            evidence,
            "wait",
            &["wait", address, "--timeout-ms", "60000", "--json"],
        )
    }

    /// `via status <session> [extra…]`.
    fn status(
        &self,
        evidence: &Evidence,
        session: &str,
        extra: &[&str],
    ) -> Result<Value, ScenarioError> {
        let mut args = vec!["status", session];
        args.extend_from_slice(extra);
        args.push("--json");
        cli(&self.sandbox, evidence, "status", &args)
    }

    /// Polls `status` until `done` holds.
    fn status_until(
        &self,
        evidence: &Evidence,
        session: &str,
        extra: &[&str],
        what: &str,
        done: impl Fn(&Value) -> bool,
    ) -> Result<Value, ScenarioError> {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let status = self.status(evidence, session, extra)?;
            if done(&status) {
                return Ok(status);
            }
            if Instant::now() >= deadline {
                return Err(ScenarioError::Timeout(format!("{what}: last {status}")));
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn daemon_pid(&self, evidence: &Evidence) -> Result<u32, ScenarioError> {
        let status = cli(
            &self.sandbox,
            evidence,
            "daemon_status",
            &["daemon", "status", "--json"],
        )?;
        status["pid"]
            .as_u64()
            .and_then(|pid| u32::try_from(pid).ok())
            .ok_or_else(|| failure(format!("daemon status has no pid: {status}")))
    }

    /// Waits until `last_activity_at` moved past `last`'s and then held
    /// still across two reads 100 ms apart: Route took the burst.
    fn activity_settles(
        &self,
        evidence: &Evidence,
        session: &str,
        last: &Value,
    ) -> Result<Value, ScenarioError> {
        let activity = |status: &Value| status["progress"]["last_activity_at"].clone();
        let mut previous = self.status_until(evidence, session, &[], "activity", |status| {
            activity(status).as_str() > activity(last).as_str()
        })?;
        loop {
            thread::sleep(Duration::from_millis(100));
            let next = self.status(evidence, session, &[])?;
            if activity(&next) == activity(&previous) {
                return Ok(next);
            }
            previous = next;
        }
    }

    /// Waits until `daemon status` shows no active turn and no connection
    /// slot in use.
    fn settled(&self, evidence: &Evidence) -> Result<(), ScenarioError> {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let status = cli(
                &self.sandbox,
                evidence,
                "daemon_status",
                &["daemon", "status", "--json"],
            )?;
            if status["sessions"]["active"] == 0 && status["connections"]["in_use"] == 0 {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(ScenarioError::Timeout(format!("not settled: {status}")));
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn arm(&self, point: &str, occurrence: u64, action: &str) -> Result<(), ScenarioError> {
        self.failpoints
            .arm(point, occurrence, action)
            .map_err(infra)
    }

    fn ack(
        &self,
        point: &str,
        occurrence: u64,
        action: &str,
        pid: u32,
    ) -> Result<(), ScenarioError> {
        self.failpoints
            .wait_ack(point, occurrence, action, pid, Duration::from_secs(30))
            .map(drop)
            .map_err(failure)
    }

    fn release(&self, point: &str, occurrence: u64) -> Result<(), ScenarioError> {
        self.failpoints.release(point, occurrence).map_err(infra)
    }

    /// `(step, tokens)` of the turn's committed rows.
    fn rows(&self, session: &str, turn: u32) -> Result<Vec<(i64, Option<i64>)>, ScenarioError> {
        let store = rusqlite::Connection::open_with_flags(
            self.sandbox.state.join("store.sqlite3"),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .map_err(infra)?;
        let mut statement = store
            .prepare("SELECT step,tokens FROM steps WHERE session_id=?1 AND turn=?2 ORDER BY step")
            .map_err(infra)?;
        statement
            .query_map(rusqlite::params![session, turn], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })
            .map_err(infra)?
            .collect::<Result<_, _>>()
            .map_err(infra)
    }

    /// The session's durable events recorded as evidence; their types.
    fn event_types(
        &self,
        evidence: &Evidence,
        session: &str,
    ) -> Result<Vec<String>, ScenarioError> {
        let page = events(&self.sandbox, evidence, "events", session)?;
        evidence
            .write(
                &format!("events_{session}.json"),
                serde_json::to_vec(&page).map_err(infra)?.as_slice(),
            )
            .map_err(infra)?;
        Ok(page
            .iter()
            .filter_map(|event| event["type"].as_str().map(str::to_owned))
            .collect())
    }
}

/// The `step` numbers of a `status` page.
fn page_steps(status: &Value) -> Vec<u64> {
    status["steps"]["items"]
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item["step"].as_u64())
                .collect()
        })
        .unwrap_or_default()
}

fn current_step(status: &Value) -> Option<u64> {
    status["progress"]["current_step"].as_u64()
}

/// Only durable event types: no model text, tool, usage or unknown vendor
/// message is an event (design §2.1).
fn durable_only(types: &[String]) -> Result<(), ScenarioError> {
    const GONE: [&str; 7] = [
        "assistant.text",
        "reasoning.summary",
        "tool.started",
        "tool.ended",
        "usage.updated",
        "file.changed",
        "vendor.other",
    ];
    check(
        !types.iter().any(|kind| GONE.contains(&kind.as_str())),
        || format!("observations became events: {types:?}"),
    )
}

/// Waits until process `pid` has ended (a zombie counts: it is not reaped
/// until its `Daemon` drops).
fn await_exit(pid: u32) -> Result<(), ScenarioError> {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let ended = fs::read_to_string(format!("/proc/{pid}/stat")).map_or(true, |stat| {
            stat.rsplit(") ")
                .next()
                .is_some_and(|rest| rest.starts_with('Z'))
        });
        if ended {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(ScenarioError::Timeout(format!(
                "process {pid} is still running"
            )));
        }
        thread::sleep(Duration::from_millis(10));
    }
}

// ------------------------------------------------------------ step rule

/// Design §2.4, §3.2, §13.2: `text`, `tool_started`, `tool_ended`, `text`,
/// `text`, `tool_started`, `tool_ended`, `text` makes `current_step` 3. At each boundary
/// the new step is published before its row is enqueued (`store.commit.step`
/// held shows the new step and no row); rows 1 and 2 commit before the
/// terminal and row 3 with it; the envelope's `steps` is `null`.
#[test]
fn s1_progress_step_rule_counts_output_after_tool_results() -> TestResult {
    let mut steps = vec![accepted(1), emit(&text(1))];
    steps.extend(tool_round(1, "t1", "shell"));
    steps.extend(emits(&[text(1), text(1)]));
    steps.extend(tool_round(1, "t2", "read"));
    steps.extend([emit(&text(1)), gate("before_end"), completed(1)]);
    let setup = Setup::new(&script(1, "rule", &steps))?;
    let evidence = setup.evidence("s1_progress_step_rule")?;
    let report = run_scenario(
        evidence,
        |evidence| {
            setup.arm(STEP, 1, "pause")?;
            let _daemon = setup.start(evidence)?;
            let pid = setup.daemon_pid(evidence)?;
            let session = setup.session(evidence, "rule")?;
            setup.ack(STEP, 1, "pause", pid)?;
            let held = setup.status(evidence, &session, &[])?;
            check(
                current_step(&held) == Some(2) && page_steps(&held).is_empty(),
                || format!("row 1 held: {held}"),
            )?;
            setup.arm(STEP, 2, "pause")?;
            setup.release(STEP, 1)?;
            setup.ack(STEP, 2, "pause", pid)?;
            let held = setup.status(evidence, &session, &[])?;
            check(
                current_step(&held) == Some(3) && page_steps(&held) == [1],
                || format!("row 2 held: {held}"),
            )?;
            setup.release(STEP, 2)?;
            setup.sandbox.await_gate("before_end")?;
            let before = setup.status_until(evidence, &session, &[], "rows 1-2", |status| {
                page_steps(status) == [1, 2]
            })?;
            check(
                current_step(&before) == Some(3)
                    && before["progress"]["turn"] == 1
                    && before["progress"]["phase"] == "model"
                    && before["progress"]["running_tools"] == json!([])
                    && before["steps"]["turn"] == 1,
                || format!("before the terminal: {before}"),
            )?;
            setup.sandbox.release_gate("before_end")?;
            let envelope = setup.wait(evidence, &format!("{session}/1"))?;
            check(
                envelope["state"] == "completed" && envelope["steps"].is_null(),
                || format!("envelope: {envelope}"),
            )?;
            let after = setup.status(evidence, &session, &["--turn", "1"])?;
            check(
                after["progress"].is_null() && page_steps(&after) == [1, 2, 3],
                || format!("after the terminal: {after}"),
            )?;
            check(setup.rows(&session, 1)?.len() == 3, || "rows".to_owned())?;
            durable_only(&setup.event_types(evidence, &session)?)
        },
        |evidence| setup.collect(evidence),
    );
    report.require_pass()
}

// ------------------------------------------------------------- one read

/// Design §2.4, §4.2, §13.2: `status` on a running turn and on an idle
/// session each make exactly one Store read: the progress snapshot adds
/// none.
#[test]
fn s1_progress_snapshot_adds_no_store_read() -> TestResult {
    let mut busy = vec![accepted(1), emit(&text(1))];
    busy.extend([gate("hold"), completed(1)]);
    let setup = Setup::new(&scripts(&[
        script(1, "idle", &[accepted(1), completed(1)]),
        script(1, "busy", &busy),
    ]))?;
    let evidence = setup.evidence("s1_progress_no_store_read")?;
    let report = run_scenario(
        evidence,
        |evidence| {
            hits::count(&setup.dir, READ_DELAY).map_err(infra)?;
            let _daemon = setup.start(evidence)?;
            let idle = setup.session(evidence, "idle")?;
            setup.wait(evidence, &format!("{idle}/1"))?;
            let busy = setup.session(evidence, "busy")?;
            setup.sandbox.await_gate("hold")?;
            setup.status_until(evidence, &busy, &[], "busy accepted", |status| {
                current_step(status) == Some(1)
            })?;
            let reads = || hits::hits(&setup.dir, READ_DELAY).map_err(infra);
            let start = reads()?;
            let quiet = setup.status(evidence, &idle, &[])?;
            let middle = reads()?;
            let running = setup.status(evidence, &busy, &[])?;
            let end = reads()?;
            check(quiet["progress"].is_null(), || format!("idle: {quiet}"))?;
            check(current_step(&running) == Some(1), || {
                format!("busy: {running}")
            })?;
            check(middle - start == 1 && end - middle == 1, || {
                format!("reads: idle {}, running {}", middle - start, end - middle)
            })?;
            setup.sandbox.release_gate("hold")?;
            setup.wait(evidence, &format!("{busy}/1")).map(drop)
        },
        |evidence| setup.collect(evidence),
    );
    report.require_pass()
}

/// Design §4.2, §13.2 (Q-R5-5): with every Store read delayed 200 ms,
/// `status` answers within 300 ms while the turn makes steps: it waits for
/// one read and nothing else.
#[test]
fn s1_c1_status_latency_under_bounded_store_delay() -> TestResult {
    let mut steps = vec![accepted(1), emit(&text(1))];
    for round in 0..3 {
        let mut block = String::new();
        for index in 0..100 {
            let id = format!("t{round}_{index}");
            for message in [tool_started(1, &id, "shell"), tool_ended(1, &id), text(1)] {
                block.push_str(&message.to_string());
                block.push('\n');
            }
        }
        steps.push(json!({"action":"flood","text":block,"count":1}));
        steps.push(gate(&format!("round{round}")));
    }
    steps.push(completed(1));
    let setup = Setup::new(&script(1, "slow", &steps))?;
    let evidence = setup.evidence("s1_c1_status_latency")?;
    let report = run_scenario(
        evidence,
        |evidence| {
            let _daemon = setup.start(evidence)?;
            let session = setup.session(evidence, "slow")?;
            setup.sandbox.await_gate("round0")?;
            setup.arm(READ_DELAY, 1, "delay_persist:200")?;
            let mut raw = Raw::open(&setup.sandbox)?;
            let mut seen = Vec::new();
            for round in 0..3 {
                if round > 0 {
                    setup.sandbox.await_gate(&format!("round{round}"))?;
                }
                setup.sandbox.release_gate(&format!("round{round}"))?;
                for call in 0..3_u64 {
                    let line =
                        request(round * 10 + call + 1, "status", &json!({"session":session}));
                    let sent = Instant::now();
                    let reply = raw.exchange(&line)?;
                    let took = sent.elapsed();
                    check(took < Duration::from_millis(300), || {
                        format!("status took {took:?}: {reply}")
                    })?;
                    check(reply["result"]["session_id"] == session.as_str(), || {
                        format!("status reply: {reply}")
                    })?;
                    seen.push(reply["result"]["progress"]["current_step"].as_u64());
                }
            }
            evidence
                .write("current_steps.json", json!(seen).to_string().as_bytes())
                .map_err(infra)?;
            check(
                seen.iter().flatten().max() > seen.iter().flatten().min(),
                || format!("the turn made no steps while status was read: {seen:?}"),
            )?;
            setup.failpoints.disarm(READ_DELAY).map_err(infra)?;
            let envelope = setup.wait(evidence, &format!("{session}/1"))?;
            check(envelope["state"] == "completed", || format!("{envelope}"))
        },
        |evidence| setup.collect(evidence),
    );
    report.require_pass()
}

// ---------------------------------------------------------- one moment

/// Design §4.2, §13.2 [t4r16.5.2, t4r17.4, t4r18.2]: `status` describes one
/// turn. With turn 2 running, `--turn 1` has `progress: null` and turn 1's
/// rows, and the default has turn 2's progress, whose `current_step` has no
/// row. A step that ends while its publish is held appears on a later call;
/// with a step row's commit held, the new `current_step` shows with no row
/// for it, and once released the row appears while `current_step` is
/// already past it. Held between the terminal commit and `finish_running`,
/// `status` shows the terminal turn and its last row with `progress: null`.
#[test]
fn s1_c1_status_progress_only_for_the_selected_turn() -> TestResult {
    let mut one = vec![accepted(1), emit(&text(1))];
    one.extend(tool_round(1, "a", "shell"));
    one.extend([emit(&text(1)), completed(1)]);
    let mut two = vec![accepted(2), emit(&text(2))];
    two.extend(tool_round(2, "b", "shell"));
    two.extend([emit(&text(2)), gate("g1")]);
    two.extend(tool_round(2, "c", "edit"));
    two.extend([emit(&text(2)), gate("g2"), completed(2)]);
    let setup = Setup::new(&scripts(&[script(1, "one", &one), script(2, "two", &two)]))?;
    let evidence = setup.evidence("s1_c1_status_selected_turn")?;
    let report = run_scenario(
        evidence,
        |evidence| {
            let _daemon = setup.start(evidence)?;
            let pid = setup.daemon_pid(evidence)?;
            let session = setup.session(evidence, "one")?;
            setup.wait(evidence, &format!("{session}/1"))?;
            // Turn 1 published one boundary and committed one row.
            setup.arm(PUBLISH, 2, "pause")?;
            setup.resume(evidence, &session, "two")?;
            setup.ack(PUBLISH, 2, "pause", pid)?;
            let held = setup.status(evidence, &session, &[])?;
            check(
                held["progress"]["turn"] == 2
                    && current_step(&held) == Some(1)
                    && held["steps"]["turn"] == 2
                    && page_steps(&held).is_empty(),
                || format!("publish held: {held}"),
            )?;
            let first = setup.status(evidence, &session, &["--turn", "1"])?;
            check(
                first["progress"].is_null()
                    && first["steps"]["turn"] == 1
                    && page_steps(&first) == [1, 2]
                    && first["active_turn"]["n"] == 2,
                || format!("turn 1 while 2 runs: {first}"),
            )?;
            setup.release(PUBLISH, 2)?;
            setup.sandbox.await_gate("g1")?;
            setup.status_until(evidence, &session, &[], "step 1 of turn 2", |status| {
                current_step(status) == Some(2) && page_steps(status) == [1]
            })?;
            // Row 2 of turn 2 is the third step commit.
            setup.arm(STEP, 3, "pause")?;
            setup.sandbox.release_gate("g1")?;
            setup.ack(STEP, 3, "pause", pid)?;
            let held = setup.status(evidence, &session, &[])?;
            check(
                current_step(&held) == Some(3) && page_steps(&held) == [1],
                || format!("row held: {held}"),
            )?;
            setup.release(STEP, 3)?;
            let moved = setup.status_until(evidence, &session, &[], "row 2", |status| {
                page_steps(status) == [1, 2]
            })?;
            check(current_step(&moved) == Some(3), || format!("{moved}"))?;
            setup.sandbox.await_gate("g2")?;
            setup.arm(FINISH, 2, "pause")?;
            setup.sandbox.release_gate("g2")?;
            setup.ack(FINISH, 2, "pause", pid)?;
            let ended = setup.status(evidence, &session, &[])?;
            check(
                ended["progress"].is_null()
                    && ended["active_turn"].is_null()
                    && ended["steps"]["turn"] == 2
                    && page_steps(&ended) == [1, 2, 3]
                    && ended["turns"][0] == json!({"n":2,"state":"completed","revision":0}),
                || format!("terminal before finish_running: {ended}"),
            )?;
            setup.release(FINISH, 2)?;
            let envelope = setup.wait(evidence, &format!("{session}/2"))?;
            check(envelope["state"] == "completed", || format!("{envelope}"))?;
            durable_only(&setup.event_types(evidence, &session)?)
        },
        |evidence| setup.collect(evidence),
    );
    report.require_pass()
}

// ------------------------------------------------------------- tokens

/// Design §2.4, §13.2: two keyless usage samples in one step supersede;
/// steps add; `tokens.scope` is the fake's declared `usage.tokens`, `turn`.
/// The envelope's `usage` reports the turn's total, the sum of its rows.
#[test]
fn s1_progress_tokens_sum_per_step_and_label_scope() -> TestResult {
    let mut steps = vec![accepted(1)];
    steps.extend(emits(&[text(1), usage(1, 100), usage(1, 120)]));
    steps.extend(tool_round(1, "a", "shell"));
    steps.extend(emits(&[text(1), usage(1, 50)]));
    steps.extend(tool_round(1, "b", "shell"));
    steps.extend([emit(&text(1)), gate("hold"), completed(1)]);
    let setup = Setup::new(&script(1, "tokens", &steps))?;
    let evidence = setup.evidence("s1_progress_tokens")?;
    let report = run_scenario(
        evidence,
        |evidence| {
            let _daemon = setup.start(evidence)?;
            let receipt = setup.spawn(evidence, "tokens")?;
            let session = receipt["session_id"]
                .as_str()
                .unwrap_or_default()
                .to_owned();
            let scope = receipt["capabilities"]["usage"]["tokens"].clone();
            check(scope == "turn", || format!("receipt: {receipt}"))?;
            setup.sandbox.await_gate("hold")?;
            let status = setup.status_until(evidence, &session, &[], "step 3", |status| {
                page_steps(status) == [1, 2]
            })?;
            check(
                current_step(&status) == Some(3)
                    && status["progress"]["tokens"] == json!({"total":170,"scope":scope})
                    && status["steps"]["items"][0]["tokens"] == 120
                    && status["steps"]["items"][1]["tokens"] == 50,
                || format!("tokens: {status}"),
            )?;
            setup.sandbox.release_gate("hold")?;
            let envelope = setup.wait(evidence, &format!("{session}/1"))?;
            let rows = setup.rows(&session, 1)?;
            check(rows == [(1, Some(120)), (2, Some(50)), (3, None)], || {
                format!("rows: {rows:?}")
            })?;
            let sum: i64 = rows.iter().filter_map(|(_, tokens)| *tokens).sum();
            check(
                envelope["usage"]
                    == json!({"input_tokens":null,"cached_input_tokens":null,
                        "output_tokens":null,"reasoning_output_tokens":null,
                        "total_tokens":sum,"scope":"turn","provenance":"reported"}),
                || format!("usage: {envelope}"),
            )
        },
        |evidence| setup.collect(evidence),
    );
    report.require_pass()
}

/// Review r1: a token count Store cannot hold fails the turn `protocol`
/// before any row is built, and `turn.ended` commits: a sample of 2^63,
/// and a sample that takes the turn's sum past `i64::MAX`. The vendor
/// hangs, so Core's stop ends the turn.
#[test]
fn s1_progress_unrepresentable_tokens_fail_protocol() -> TestResult {
    let max = u64::try_from(i64::MAX)?;
    let hang = json!({"action":"hang"});
    let mut sample = vec![accepted(1)];
    sample.extend(emits(&[text(1), usage(1, max + 1)]));
    sample.push(hang.clone());
    let mut sum = vec![accepted(1)];
    sum.extend(emits(&[text(1), usage(1, max)]));
    sum.extend(tool_round(1, "a", "shell"));
    sum.extend(emits(&[text(1), usage(1, 1)]));
    sum.push(hang);
    for (name, steps, rows) in [
        ("sample", sample, vec![(1, None)]),
        ("sum", sum, vec![(1, Some(i64::MAX)), (2, None)]),
    ] {
        let setup = Setup::new(&script(1, name, &steps))?;
        let evidence = setup.evidence(&format!("s1_progress_unrepresentable_{name}"))?;
        let report = run_scenario(
            evidence,
            |evidence| {
                let _daemon = setup.start(evidence)?;
                let receipt = setup.spawn(evidence, name)?;
                let session = receipt["session_id"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned();
                let envelope = setup.wait(evidence, &format!("{session}/1"))?;
                check(
                    envelope["state"] == "failed" && envelope["failure"]["class"] == "protocol",
                    || format!("{name}: {envelope}"),
                )?;
                let types = setup.event_types(evidence, &session)?;
                check(
                    types.last().is_some_and(|last| last == "turn.ended"),
                    || format!("{name}: {types:?}"),
                )?;
                let committed = setup.rows(&session, 1)?;
                check(committed == rows, || format!("{name} rows: {committed:?}"))
            },
            |evidence| setup.collect(evidence),
        );
        report.require_pass()?;
    }
    Ok(())
}

/// Review r2: a refused sample's `protocol` failure survives a daemon
/// force. Core is held at `core.observations.pause` on the 2^63 sample
/// while the daemon force-stops, so Route ends the turn under the force
/// (the vendor is gone) before Core refuses the sample and orders its
/// `protocol` stop: the turn reaches final shutdown's forced terminal,
/// which commits it `failed(protocol)` with its `turn.ended`.
#[test]
fn s1_progress_unrepresentable_tokens_survive_a_forced_stop() -> TestResult {
    const PAUSE: &str = "core.observations.pause";
    let max = u64::try_from(i64::MAX)?;
    let mut steps = vec![accepted(1)];
    steps.extend(emits(&[text(1)]));
    steps.push(json!({"action":"report_pids"}));
    steps.extend(emits(&[usage(1, max + 1)]));
    steps.push(json!({"action":"hang"}));
    let setup = Setup::new(&script(1, "forced", &steps))?;
    let evidence = setup.evidence("s1_progress_unrepresentable_forced")?;
    let report = run_scenario(
        evidence,
        |evidence| {
            // Hits: the acceptance, the text, then the sample.
            setup.arm(PAUSE, 3, "pause")?;
            let _daemon = setup.start(evidence)?;
            let pid = setup.daemon_pid(evidence)?;
            let session = setup.session(evidence, "forced")?;
            setup.ack(PAUSE, 3, "pause", pid)?;
            let agent: u32 = fs::read_to_string(setup.sandbox.sync.join("agent.pid"))
                .map_err(infra)?
                .trim()
                .parse()
                .map_err(infra)?;
            cli(
                &setup.sandbox,
                evidence,
                "stop",
                &["daemon", "stop", "--force", "--json"],
            )?;
            // Route closed the vendor under the force while Core is held.
            await_exit(agent)?;
            setup.release(PAUSE, 3)?;
            await_exit(pid)?;
            let failed = setup.sandbox.count(&format!(
                "SELECT count(*) FROM turns WHERE session_id='{session}' AND number=1 \
                 AND state='failed' \
                 AND json_extract(envelope,'$.failure.class')='protocol'"
            ))?;
            let ended = setup.sandbox.count(&format!(
                "SELECT count(*) FROM events WHERE session_id='{session}' AND type='turn.ended'"
            ))?;
            check(failed == 1 && ended == 1, || {
                format!("failed(protocol) {failed}, turn.ended {ended}")
            })
        },
        |evidence| setup.collect(evidence),
    );
    report.require_pass()
}

// ---------------------------------------------------------- tool bounds

/// Design §2.4 rules 3 and 4, §13.2: 70 concurrent tool starts keep 64
/// names and set `tools_overflow`; an end of an untracked tool, then model
/// output, advances `current_step` and writes a row.
#[test]
fn s1_progress_tools_overflow_and_untracked_end_count() -> TestResult {
    let mut steps = vec![accepted(1), emit(&text(1))];
    for index in 0..70 {
        steps.push(emit(&tool_started(
            1,
            &format!("t{index}"),
            &format!("tool{index}"),
        )));
    }
    steps.push(gate("started"));
    steps.extend(emits(&[tool_ended(1, "untracked"), text(1)]));
    steps.extend([gate("advanced"), completed(1)]);
    let setup = Setup::new(&script(1, "tools", &steps))?;
    let evidence = setup.evidence("s1_progress_tools_overflow")?;
    let report = run_scenario(
        evidence,
        |evidence| {
            let _daemon = setup.start(evidence)?;
            let session = setup.session(evidence, "tools")?;
            setup.sandbox.await_gate("started")?;
            let expected: Vec<String> = (0..64).map(|index| format!("tool{index}")).collect();
            let started = setup.status_until(evidence, &session, &[], "70 starts", |status| {
                status["progress"]["tools_overflow"] == true
            })?;
            check(
                started["progress"]["running_tools"] == json!(expected)
                    && started["progress"]["phase"] == "tools"
                    && current_step(&started) == Some(1),
                || format!("70 starts: {started}"),
            )?;
            setup.sandbox.release_gate("started")?;
            setup.sandbox.await_gate("advanced")?;
            let advanced = setup.status_until(evidence, &session, &[], "a row", |status| {
                page_steps(status) == [1]
            })?;
            check(
                current_step(&advanced) == Some(2)
                    && advanced["progress"]["running_tools"] == json!([])
                    && advanced["progress"]["tools_overflow"] == false
                    && advanced["progress"]["phase"] == "model",
                || format!("after the boundary: {advanced}"),
            )?;
            setup.sandbox.release_gate("advanced")?;
            setup.wait(evidence, &format!("{session}/1")).map(drop)
        },
        |evidence| setup.collect(evidence),
    );
    report.require_pass()
}

// ------------------------------------------------------ unknown messages

/// Design §2.3, §13.2: unknown messages send no C2 item, yet move
/// `last_activity_at`. Core is held on the turn's first model output while
/// 3,000 unknown messages flow: had they been items, the channel's 1,024
/// would fill and the lowered stall would fail the turn `overflow`. The
/// vendor writes them in bursts of 1,000, each after Route took the last
/// (its arrivals stopped moving the clock): one burst past the Wire queue's
/// 1,024 fails `overflow` by design (A47, runtime §8.2).
#[test]
fn s1_progress_unknown_messages_send_no_observation() -> TestResult {
    const BURSTS: usize = 3;
    let unknown =
        json!({"type":"vendor_note","vendor_turn_id":"fake-turn-1","note":"x".repeat(40)});
    let line = format!("{unknown}\n");
    let mut steps = vec![accepted(1), emit(&text(1)), gate("before")];
    for burst in 0..BURSTS {
        steps.push(json!({"action":"flood","text":line,"count":1000}));
        steps.push(gate(&format!("burst{burst}")));
    }
    steps.push(completed(1));
    let setup = Setup::new(&script(1, "unknown", &steps))?;
    let evidence = setup.evidence("s1_progress_unknown_messages")?;
    let report = run_scenario(
        evidence,
        |evidence| {
            // Observation 1 is the acceptance, 2 the model output.
            setup.arm(OBSERVE, 2, "pause")?;
            let _daemon = setup.start_stalling(evidence)?;
            let pid = setup.daemon_pid(evidence)?;
            let session = setup.session(evidence, "unknown")?;
            setup.ack(OBSERVE, 2, "pause", pid)?;
            setup.sandbox.await_gate("before")?;
            let before = setup.status(evidence, &session, &[])?;
            check(
                before["progress"]["last_activity_at"].is_string()
                    && current_step(&before) == Some(1),
                || format!("before: {before}"),
            )?;
            setup.sandbox.release_gate("before")?;
            let mut seen = vec![before];
            for burst in 0..BURSTS {
                let name = format!("burst{burst}");
                setup.sandbox.await_gate(&name)?;
                let last = seen.last().cloned().unwrap_or_default();
                let moved = setup.activity_settles(evidence, &session, &last)?;
                seen.push(moved);
                setup.sandbox.release_gate(&name)?;
            }
            evidence
                .write("activity.json", json!(seen).to_string().as_bytes())
                .map_err(infra)?;
            setup.release(OBSERVE, 2)?;
            let envelope = setup.wait(evidence, &format!("{session}/1"))?;
            check(envelope["state"] == "completed", || {
                format!("unknown messages failed the turn: {envelope}")
            })?;
            durable_only(&setup.event_types(evidence, &session)?)
        },
        |evidence| setup.collect(evidence),
    );
    report.require_pass()
}

// --------------------------------------------------------- durability

/// Design §3.3, §13.2: killed after row 2's commit, before row 3's, the
/// restarted daemon shows rows 1 and 2 and the turn `unknown`.
#[test]
fn s1_progress_step_rows_survive_crash_to_last_commit() -> TestResult {
    let mut steps = vec![accepted(1), emit(&text(1))];
    for (id, _) in [("a", 1), ("b", 2), ("c", 3)] {
        steps.extend(tool_round(1, id, "shell"));
        steps.push(emit(&text(1)));
    }
    steps.extend([gate("hold"), completed(1)]);
    let setup = Setup::new(&script(1, "crash", &steps))?;
    let evidence = setup.evidence("s1_progress_rows_survive_crash")?;
    let report = run_scenario(
        evidence,
        |evidence| {
            setup.arm(STEP, 3, "crash")?;
            let _crashed = setup.start(evidence)?;
            let pid = setup.daemon_pid(evidence)?;
            let session = setup.session(evidence, "crash")?;
            setup.ack(STEP, 3, "crash", pid)?;
            await_exit(pid)?;
            check(setup.rows(&session, 1)?.len() == 2, || {
                "rows at the crash".to_owned()
            })?;
            setup.failpoints.disarm(STEP).map_err(infra)?;
            let _restarted = setup.start(evidence)?;
            let status = setup.status(evidence, &session, &["--turn", "1"])?;
            check(
                status["progress"].is_null()
                    && page_steps(&status) == [1, 2]
                    && status["turns"][0] == json!({"n":1,"state":"unknown","revision":0}),
                || format!("restarted: {status}"),
            )
        },
        |evidence| setup.collect(evidence),
    );
    report.require_pass()
}

/// Design §3.2, §13.2 (Q-R5-4): row 2's commit is refused (known): the turn
/// fails `store`, and rows 2, 3 and the open step's row 4 commit with
/// `turn.ended`. Core is held on row 2's boundary until Route decoded the
/// terminal, so steps 3 and 4 reach Core.
#[test]
fn s1_progress_step_commit_refused_rows_ride_in_terminal() -> TestResult {
    let mut steps = vec![accepted(1), emit(&text(1))];
    for id in ["a", "b", "c"] {
        steps.extend(tool_round(1, id, "shell"));
        steps.push(emit(&text(1)));
    }
    steps.push(emit(&tool_started(1, "d", "shell")));
    steps.push(completed(1));
    let setup = Setup::new(&script(1, "refused", &steps))?;
    let evidence = setup.evidence("s1_progress_refused_rows")?;
    let report = run_scenario(
        evidence,
        |evidence| {
            // Observations: 1 acceptance, 2 text, 3-4 tools, 5 text (row 1),
            // 6-7 tools, 8 text (row 2).
            setup.arm(OBSERVE, 8, "pause")?;
            setup.arm(STEP, 2, "fail_io")?;
            setup.arm(EXIT, 1, "pause")?;
            let _daemon = setup.start(evidence)?;
            let pid = setup.daemon_pid(evidence)?;
            let session = setup.session(evidence, "refused")?;
            setup.ack(OBSERVE, 8, "pause", pid)?;
            // Route is past the terminal: its exit wait follows it.
            setup.ack(EXIT, 1, "pause", pid)?;
            setup.release(OBSERVE, 8)?;
            setup.ack(STEP, 2, "fail_io", pid)?;
            setup.release(EXIT, 1)?;
            let envelope = setup.wait(evidence, &format!("{session}/1"))?;
            check(
                envelope["state"] == "failed" && envelope["failure"]["class"] == "store",
                || format!("envelope: {envelope}"),
            )?;
            let rows: Vec<i64> = setup
                .rows(&session, 1)?
                .into_iter()
                .map(|(step, _)| step)
                .collect();
            check(rows == [1, 2, 3, 4], || format!("rows {rows:?}"))?;
            let types = setup.event_types(evidence, &session)?;
            check(
                types.last().map(String::as_str) == Some("turn.ended"),
                || format!("{types:?}"),
            )
        },
        |evidence| setup.collect(evidence),
    );
    report.require_pass()
}

/// Design §3.2, §13.2 [t4r16.5.6]: 2,000 fake steps give 2,000 rows, more
/// than one page. The vendor writes 100 steps between gates so the Wire
/// queue is never outrun.
#[test]
fn s1_progress_many_steps_all_have_rows() -> TestResult {
    const ROUNDS: usize = 20;
    let mut steps = vec![accepted(1), emit(&text(1))];
    for round in 0..ROUNDS {
        let blocks = if round + 1 == ROUNDS { 99 } else { 100 };
        let mut block = String::new();
        for message in [tool_started(1, "t", "shell"), tool_ended(1, "t"), text(1)] {
            block.push_str(&message.to_string());
            block.push('\n');
        }
        steps.push(json!({"action":"flood","text":block,"count":blocks}));
        steps.push(gate(&format!("round{round}")));
    }
    steps.push(completed(1));
    let setup = Setup::new(&script(1, "many", &steps))?;
    let evidence = setup.evidence("s1_progress_many_steps")?;
    let report = run_scenario(
        evidence,
        |evidence| {
            let _daemon = setup.start(evidence)?;
            let session = setup.session(evidence, "many")?;
            for round in 0..ROUNDS {
                let name = format!("round{round}");
                setup.sandbox.await_gate(&name)?;
                let step = u64::try_from(round * 100 + 101)
                    .unwrap_or(u64::MAX)
                    .min(2000);
                setup.status_until(evidence, &session, &[], &name, |status| {
                    current_step(status) == Some(step)
                })?;
                setup.sandbox.release_gate(&name)?;
            }
            let envelope = setup.wait(evidence, &format!("{session}/1"))?;
            check(envelope["state"] == "completed", || format!("{envelope}"))?;
            let first = setup.status(evidence, &session, &["--turn", "1", "--limit", "1000"])?;
            let second = setup.status(
                evidence,
                &session,
                &["--turn", "1", "--after-step", "1000", "--limit", "1000"],
            )?;
            let expected_first: Vec<u64> = (1..=1000).collect();
            let expected_second: Vec<u64> = (1001..=2000).collect();
            check(
                page_steps(&first) == expected_first
                    && first["steps"]["more"] == true
                    && first["steps"]["next_after"] == 1000,
                || format!("first page: {}", first["steps"]["next_after"]),
            )?;
            check(
                page_steps(&second) == expected_second
                    && second["steps"]["more"] == false
                    && second["steps"]["next_after"] == 2000,
                || format!("second page: {}", second["steps"]["next_after"]),
            )?;
            check(setup.rows(&session, 1)?.len() == 2000, || "rows".to_owned())
        },
        |evidence| setup.collect(evidence),
    );
    report.require_pass()
}

/// Design §3.2, §13.2 [t4r6.8]: a turn that final shutdown forces in step 3
/// commits row 3 with its `turn.ended`.
#[test]
fn s1_progress_forced_shutdown_terminal_carries_open_row() -> TestResult {
    let mut steps = vec![accepted(1), emit(&text(1))];
    for id in ["a", "b"] {
        steps.extend(tool_round(1, id, "shell"));
        steps.push(emit(&text(1)));
    }
    steps.extend([gate("step3"), json!({"action":"hang"})]);
    let setup = Setup::new(&script(1, "forced", &steps))?;
    let evidence = setup.evidence("s1_progress_forced_open_row")?;
    let report = run_scenario(
        evidence,
        |evidence| {
            let _daemon = setup.start(evidence)?;
            let pid = setup.daemon_pid(evidence)?;
            let session = setup.session(evidence, "forced")?;
            setup.sandbox.await_gate("step3")?;
            setup.status_until(evidence, &session, &[], "step 3", |status| {
                current_step(status) == Some(3) && page_steps(status) == [1, 2]
            })?;
            cli(
                &setup.sandbox,
                evidence,
                "stop",
                &["daemon", "stop", "--force", "--json"],
            )?;
            await_exit(pid)?;
            let rows: Vec<i64> = setup
                .rows(&session, 1)?
                .into_iter()
                .map(|(step, _)| step)
                .collect();
            check(rows == [1, 2, 3], || format!("rows {rows:?}"))?;
            let ended = setup.sandbox.count(&format!(
                "SELECT count(*) FROM events WHERE session_id='{session}' AND type='turn.ended'"
            ))?;
            check(ended == 1, || "no turn.ended".to_owned())
        },
        |evidence| setup.collect(evidence),
    );
    report.require_pass()
}

// -------------------------------------------------------- status members

/// Design §11.3, §13.2: `status`'s durable members are the same before and
/// after the session's slot is evicted and after a restart; `progress` is
/// `null` after the restart for a turn that was running.
#[test]
fn s1_c1_status_every_member_after_eviction_and_restart() -> TestResult {
    let mut idle = vec![accepted(1), emit(&text(1))];
    idle.extend(tool_round(1, "a", "shell"));
    idle.extend([emit(&text(1)), completed(1)]);
    let busy = vec![
        accepted(1),
        emit(&text(1)),
        gate("hold"),
        json!({"action":"hang"}),
    ];
    let setup = Setup::new(&scripts(&[
        script(1, "idle", &idle),
        script(1, "busy", &busy),
    ]))?;
    let evidence = setup.evidence("s1_c1_status_every_member")?;
    let report = run_scenario(
        evidence,
        |evidence| {
            let daemon = setup.start(evidence)?;
            let session = setup.session(evidence, "idle")?;
            setup.wait(evidence, &format!("{session}/1"))?;
            let first = setup.status(evidence, &session, &[])?;
            // The dispatcher exits and retires the slot once nothing is left:
            // no active turn and no connection slot in use.
            setup.settled(evidence)?;
            let evicted = setup.status(evidence, &session, &[])?;
            check(evicted == first, || {
                format!("members changed after eviction: {first} / {evicted}")
            })?;
            let busy = setup.session(evidence, "busy")?;
            setup.sandbox.await_gate("hold")?;
            let running = setup.status_until(evidence, &busy, &[], "busy", |status| {
                current_step(status) == Some(1)
            })?;
            check(
                running["process"]["alive"] == true
                    && running["active_turn"]["phase"] == "accepted"
                    && running["active_turn"]["n"] == 1,
                || format!("running: {running}"),
            )?;
            cli(
                &setup.sandbox,
                evidence,
                "stop",
                &["daemon", "stop", "--force", "--json"],
            )?;
            drop(daemon);
            let _restarted = setup.start(evidence)?;
            let restarted = setup.status(evidence, &session, &[])?;
            let busy_after = setup.status(evidence, &busy, &[])?;
            evidence
                .write(
                    "statuses.json",
                    json!([first, evicted, restarted, running, busy_after])
                        .to_string()
                        .as_bytes(),
                )
                .map_err(infra)?;
            check(restarted == first, || {
                format!("members changed across restart: {first} / {restarted}")
            })?;
            check(
                first["session_id"] == session.as_str()
                    && first["state"] == "idle"
                    && first["admission"] == "open"
                    && first["harness"] == "fake"
                    && first["model"] == "fake"
                    && first["route"].is_string()
                    && first["vendor_session_id"].is_null()
                    && first["vendor_identity_verified"] == false
                    && first["process"]
                        == json!({"alive":false,"cleanup":"quiescent","idle_since":null})
                    && first["active_turn"].is_null()
                    && first["queue"] == json!([])
                    && first["turns"] == json!([{"n":1,"state":"completed","revision":0}])
                    && first["progress"].is_null()
                    && page_steps(&first) == [1, 2]
                    && first["label"].is_null()
                    && first["created_at"].is_string()
                    && first["updated_at"].is_string(),
                || format!("members: {first}"),
            )?;
            check(
                busy_after["progress"].is_null() && busy_after["active_turn"].is_null(),
                || format!("after restart: {busy_after}"),
            )
        },
        |evidence| setup.collect(evidence),
    );
    report.require_pass()
}

/// Design §11.3, §13.2: `process.alive` is positive evidence only. With the
/// vendor's exit recorded and Route not yet past it (`wire.exit.observed`
/// held), the control is still held and the turn still running, yet
/// `alive` is `false`.
#[test]
fn s1_c1_status_alive_false_after_exit_before_control_drop() -> TestResult {
    let steps = vec![accepted(1), gate("live"), completed(1)];
    let setup = Setup::new(&script(1, "alive", &steps))?;
    let evidence = setup.evidence("s1_c1_status_alive")?;
    let report = run_scenario(
        evidence,
        |evidence| {
            let _daemon = setup.start(evidence)?;
            let pid = setup.daemon_pid(evidence)?;
            let session = setup.session(evidence, "alive")?;
            setup.sandbox.await_gate("live")?;
            let live = setup.status_until(evidence, &session, &[], "alive", |status| {
                status["process"]["alive"] == true
            })?;
            check(live["active_turn"]["n"] == 1, || format!("{live}"))?;
            setup.arm(EXIT, 1, "pause")?;
            setup.sandbox.release_gate("live")?;
            setup.ack(EXIT, 1, "pause", pid)?;
            let exited = setup.status(evidence, &session, &[])?;
            check(
                exited["process"]["alive"] == false && exited["active_turn"]["n"] == 1,
                || format!("exit recorded: {exited}"),
            )?;
            setup.release(EXIT, 1)?;
            setup.wait(evidence, &format!("{session}/1")).map(drop)
        },
        |evidence| setup.collect(evidence),
    );
    report.require_pass()
}

// ------------------------------------------------------------ decode rules

/// Design §2.2 rules 1 and 3, §7.3: a tool name of 1 KiB + 1 bytes, a
/// message nested 65 deep and one of 65,537 nodes each fail the turn
/// `protocol` with the message saved to `undecoded.bin`, named by the
/// failure; a 1 KiB name is accepted.
#[test]
fn s1_progress_decode_limits_fail_protocol_with_message_saved() -> TestResult {
    let long_name = serde_json::to_string(&tool_started(1, "t", &"n".repeat(1025)))?;
    let kept_name = tool_started(1, "t", &"n".repeat(1024));
    let deep = format!(
        r#"{{"type":"text","vendor_turn_id":"fake-turn-1","x":{}{}}}"#,
        "[".repeat(64),
        "]".repeat(64)
    );
    let nodes = format!(
        r#"{{"type":"text","vendor_turn_id":"fake-turn-1","x":[{}]}}"#,
        vec!["0"; 65_536].join(",")
    );
    let bad = |prompt: &str, line: &str| {
        script(
            1,
            prompt,
            &[
                accepted(1),
                json!({"action":"emit_raw","text":format!("{line}\n")}),
                json!({"action":"hang"}),
            ],
        )
    };
    let setup = Setup::new(&scripts(&[
        bad("long", &long_name),
        bad("deep", &deep),
        bad("nodes", &nodes),
        script(1, "kept", &[accepted(1), emit(&kept_name), completed(1)]),
    ]))?;
    let evidence = setup.evidence("s1_progress_decode_limits")?;
    let report = run_scenario(
        evidence,
        |evidence| {
            let _daemon = setup.start(evidence)?;
            for (prompt, line) in [("long", &long_name), ("deep", &deep), ("nodes", &nodes)] {
                let session = setup.session(evidence, prompt)?;
                let envelope = setup.wait(evidence, &format!("{session}/1"))?;
                check(
                    envelope["state"] == "failed" && envelope["failure"]["class"] == "protocol",
                    || format!("{prompt}: {envelope}"),
                )?;
                let file = setup
                    .sandbox
                    .state
                    .join("evidence")
                    .join(&session)
                    .join("1")
                    .join("undecoded.bin");
                let saved = fs::read(&file).map_err(infra)?;
                let whole = format!("{line}\n");
                let expected = &whole.as_bytes()[..whole.len().min(64 * 1024)];
                check(
                    saved == expected.strip_suffix(b"\n").unwrap_or(expected) || saved == expected,
                    || format!("{prompt}: undecoded.bin holds {} bytes", saved.len()),
                )?;
                let message = envelope["failure"]["message"].as_str().unwrap_or_default();
                check(message.contains("undecoded.bin"), || {
                    format!("{prompt}: the failure does not name the file: {message}")
                })?;
            }
            let kept = setup.session(evidence, "kept")?;
            let envelope = setup.wait(evidence, &format!("{kept}/1"))?;
            check(envelope["state"] == "completed", || {
                format!("1 KiB name: {envelope}")
            })
        },
        |evidence| setup.collect(evidence),
    );
    report.require_pass()
}

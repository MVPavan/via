//! Task 4 design §4.3 and §4.5 (A25, A38) through the real `via` binary and
//! daemon: `events` pages a session or one turn with no follow stream, and
//! `list` pages sessions in creation order with `last_active_at`. Written
//! before the `events` page and `list`.

#[path = "support/daemon.rs"]
#[expect(dead_code, reason = "shared support; this file uses part of it")]
mod daemon;
#[path = "support/outer_cleanup.rs"]
mod outer_cleanup;
#[path = "support/scenario.rs"]
mod scenario;
mod support;

use std::collections::HashSet;
use std::thread;
use std::time::{Duration, Instant};

use daemon::{
    Daemon, Raw, Sandbox, TestResult, cli, collect_available, failure, infra, refused, request,
};
use scenario::{ScenarioError, run_scenario};
use serde_json::{Value, json};
use support::evidence::Evidence;

const HANDLE: &str = "h_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";

fn check(condition: bool, detail: impl FnOnce() -> String) -> Result<(), ScenarioError> {
    if condition {
        Ok(())
    } else {
        Err(failure(detail()))
    }
}

/// A script for `prompt` as turn `turn`: accepted, then completed.
fn completes(prompt: &str, turn: u32) -> Value {
    let vendor = format!("fake-turn-{turn}");
    json!({"expected_request":{"type":"start","id":1,"turn":turn,"prompt":prompt},"steps":[
        {"action":"emit","message":{"type":"accepted","id":1,"vendor_turn_id":vendor}},
        {"action":"emit","message":{"type":"terminal","vendor_turn_id":vendor,
            "status":"completed","final_text":"done","stop_reason":"end_turn"}},
    ]})
}

fn spawn(
    sandbox: &Sandbox,
    evidence: &Evidence,
    prompt: &str,
    label: Option<&str>,
) -> Result<String, ScenarioError> {
    let mut args = vec![
        "spawn",
        "--harness",
        "fake",
        "--model",
        "fake",
        "--prompt",
        prompt,
        "--handle",
        HANDLE,
        "--background",
        "--json",
    ];
    if let Some(label) = label {
        args.extend(["--label", label]);
    }
    let receipt = cli(sandbox, evidence, &format!("spawn_{prompt}"), &args)?;
    receipt["session_id"]
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| failure(format!("receipt has no session: {receipt}")))
}

fn wait(sandbox: &Sandbox, evidence: &Evidence, address: &str) -> Result<Value, ScenarioError> {
    cli(
        sandbox,
        evidence,
        &format!("wait_{}", address.replace('/', "_")),
        &["wait", address, "--timeout-ms", "30000", "--json"],
    )
}

fn resume(
    sandbox: &Sandbox,
    evidence: &Evidence,
    session: &str,
    prompt: &str,
) -> Result<(), ScenarioError> {
    cli(
        sandbox,
        evidence,
        &format!("resume_{prompt}"),
        &[
            "resume", session, "--prompt", prompt, "--handle", HANDLE, "--json",
        ],
    )
    .map(drop)
}

fn seqs(page: &Value) -> Vec<u64> {
    page["events"]
        .as_array()
        .map(|events| {
            events
                .iter()
                .filter_map(|event| event["seq"].as_u64())
                .collect()
        })
        .unwrap_or_default()
}

/// Design §4.3, §13.2 (A25): `follow: true` is an unknown member and so
/// `invalid_params`, and `unsubscribe` is `method_not_found`. The page
/// addresses exactly one of a session and a turn: a turn address pages
/// that turn's events only, `types` filters them, `after` and `limit` move
/// the window, `earliest_seq` is 1 and no event carries `raw_ref`; an
/// unknown type and a `limit` out of 1 to 1000 are refused.
#[test]
#[expect(
    clippy::too_many_lines,
    reason = "one scenario keeps every refused shape and each page read on one daemon"
)]
fn s1_c1_follow_and_unsubscribe_are_refused() -> TestResult {
    let sandbox =
        Sandbox::new(&json!({"scripts":[completes("first", 1), completes("second", 2)]}))?;
    let evidence = Evidence::new("s1_c1_follow_refused", &sandbox.fake, &sandbox.fixture)?;
    let report = run_scenario(
        evidence,
        |evidence| {
            let _daemon = Daemon::start(&sandbox, evidence)?;
            let session = spawn(&sandbox, evidence, "first", None)?;
            wait(&sandbox, evidence, &format!("{session}/1"))?;
            resume(&sandbox, evidence, &session, "second")?;
            wait(&sandbox, evidence, &format!("{session}/2"))?;

            let mut raw = Raw::open(&sandbox)?;
            let followed = raw.exchange(&request(
                1,
                "events",
                &json!({"session":session,"follow":true}),
            ))?;
            check(
                followed["error"]["data"]["kind"] == "invalid_params",
                || format!("follow: {followed}"),
            )?;
            let unsubscribed =
                raw.exchange(&request(2, "unsubscribe", &json!({"session":session})))?;
            check(
                unsubscribed["error"]["data"]["kind"] == "method_not_found",
                || format!("unsubscribe: {unsubscribed}"),
            )?;
            for (id, params) in [
                (3, json!({"session":session,"turn":format!("{session}/1")})),
                (4, json!({})),
                (5, json!({"session":session,"types":["assistant.text"]})),
                (6, json!({"session":session,"limit":0})),
                (7, json!({"session":session,"limit":1001})),
                (8, json!({"turn":session})),
            ] {
                let reply = raw.exchange(&request(id, "events", &params))?;
                check(reply["error"]["data"]["kind"] == "invalid_params", || {
                    format!("events {params}: {reply}")
                })?;
            }

            let all = cli(
                &sandbox,
                evidence,
                "events_all",
                &["events", &session, "--json"],
            )?;
            let events = all["events"].as_array().cloned().unwrap_or_default();
            check(
                all["earliest_seq"] == 1
                    && all["more"] == false
                    && seqs(&all)
                        == (1..=u64::try_from(events.len()).map_err(infra)?).collect::<Vec<_>>()
                    && all["next_after"] == events.len()
                    && events.iter().all(|event| event.get("raw_ref").is_none()),
                || format!("session page: {all}"),
            )?;
            let second = cli(
                &sandbox,
                evidence,
                "events_turn_2",
                &["events", &format!("{session}/2"), "--json"],
            )?;
            let of_second = second["events"].as_array().cloned().unwrap_or_default();
            check(
                !of_second.is_empty() && of_second.iter().all(|event| event["turn"] == 2),
                || format!("turn 2 page: {second}"),
            )?;
            let ended = cli(
                &sandbox,
                evidence,
                "events_ended",
                &[
                    "events",
                    &session,
                    "--types",
                    "turn.ended,turn.queued",
                    "--json",
                ],
            )?;
            let kinds: Vec<&str> = ended["events"]
                .as_array()
                .map(|events| {
                    events
                        .iter()
                        .filter_map(|event| event["type"].as_str())
                        .collect()
                })
                .unwrap_or_default();
            check(
                kinds == ["turn.queued", "turn.ended", "turn.queued", "turn.ended"],
                || format!("typed page: {ended}"),
            )?;
            let window = cli(
                &sandbox,
                evidence,
                "events_window",
                &["events", &session, "--after", "2", "--limit", "2", "--json"],
            )?;
            check(
                seqs(&window) == [3, 4] && window["next_after"] == 4 && window["more"] == true,
                || format!("window page: {window}"),
            )
        },
        |evidence| collect(evidence, &sandbox),
    );
    report.require_pass()
}

/// C1 §3.8 (S1 critic finding 3): closing the connection of a pending
/// `wait` releases only that waiter, and its socket slot. 32 clients, every
/// socket slot (design §10.1), each complete `hello`, start a long `wait`
/// on a running turn and disconnect. A new client then completes `hello`
/// and a request (retried until a slot frees, within a bound far shorter
/// than the waits). The turn, never affected, reaches its terminal, and
/// `result` returns it. Before the fix every slot stayed held until the
/// waits expired.
#[test]
fn s1_c1_disconnected_waits_release_their_slots() -> TestResult {
    const SLOTS: usize = 32;
    let vendor = "fake-turn-1";
    let held = json!({"expected_request":{"type":"start","id":1,"turn":1,"prompt":"slow"},"steps":[
        {"action":"emit","message":{"type":"accepted","id":1,"vendor_turn_id":vendor}},
        {"action":"gate","name":"slow"},
        {"action":"emit","message":{"type":"terminal","vendor_turn_id":vendor,
            "status":"completed","final_text":"done","stop_reason":"end_turn"}},
    ]});
    let sandbox = Sandbox::new(&json!({ "scripts": [held] }))?;
    let evidence = Evidence::new(
        "s1_c1_wait_disconnect_slots",
        &sandbox.fake,
        &sandbox.fixture,
    )?;
    let report = run_scenario(
        evidence,
        |evidence| {
            let _daemon = Daemon::start(&sandbox, evidence)?;
            let session = spawn(&sandbox, evidence, "slow", None)?;
            sandbox.await_gate("slow")?;
            let address = format!("{session}/1");
            let waiting = request(1, "wait", &json!({"address":address,"timeout_ms":600_000}));
            // Every slot is taken before any wait starts.
            let clients = (0..SLOTS)
                .map(|_| Raw::open(&sandbox))
                .collect::<Result<Vec<_>, _>>()?;
            for mut client in clients {
                client.send(&waiting)?;
                drop(client);
            }
            // Recovery is the handshake succeeding: a refused connection is
            // closed without bytes, so each attempt either fails or serves.
            let deadline = Instant::now() + Duration::from_secs(10);
            let mut attempts = 0_u32;
            let status = loop {
                attempts += 1;
                let served = Raw::open(&sandbox)
                    .and_then(|mut raw| raw.exchange(&request(2, "daemon/status", &json!({}))));
                match served {
                    Ok(reply) => break reply,
                    Err(error) if Instant::now() >= deadline => {
                        return Err(failure(format!(
                            "no slot freed after {SLOTS} disconnected waits \
                             ({attempts} attempts): {error:?}"
                        )));
                    }
                    Err(_) => thread::sleep(Duration::from_millis(20)),
                }
            };
            evidence
                .write(
                    "reconnect.json",
                    json!({"attempts":attempts,"status":status})
                        .to_string()
                        .as_bytes(),
                )
                .map_err(infra)?;
            check(status["result"]["pid"].is_u64(), || {
                format!("daemon/status: {status}")
            })?;
            sandbox.release_gate("slow")?;
            let waited = wait(&sandbox, evidence, &address)?;
            let result = cli(
                &sandbox,
                evidence,
                "result",
                &["result", &address, "--json"],
            )?;
            check(result["state"] == "completed" && result == waited, || {
                format!("result {result}, wait {waited}")
            })
        },
        |evidence| collect(evidence, &sandbox),
    );
    report.require_pass()
}

/// A fake turn that is accepted, then held at gate `slow` until released.
fn held_turn() -> Value {
    let vendor = "fake-turn-1";
    json!({"expected_request":{"type":"start","id":1,"turn":1,"prompt":"slow"},"steps":[
        {"action":"emit","message":{"type":"accepted","id":1,"vendor_turn_id":vendor}},
        {"action":"gate","name":"slow"},
        {"action":"emit","message":{"type":"terminal","vendor_turn_id":vendor,
            "status":"completed","final_text":"done","stop_reason":"end_turn"}},
    ]})
}

/// The `type` of each event of `page`.
fn types(page: &Value) -> Vec<String> {
    page["events"]
        .as_array()
        .map(|events| {
            events
                .iter()
                .filter_map(|event| event["type"].as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

/// C1 §3.11 (via-2lp): an `events` long-poll through the real daemon. With
/// nothing after `after` it waits out `--wait-ms` and returns the empty page
/// at `after`. 32 long-polls take every socket slot and disconnect: each
/// releases only its waiter and its slot (a new client is served well
/// before their 30 s bound). A long-poll for `turn.ended` started before the
/// held turn ends returns with it, and the turn, never affected, completes.
#[test]
#[expect(
    clippy::too_many_lines,
    reason = "one scenario keeps the bound, the disconnects and the end on one daemon"
)]
fn s1_c1_events_long_poll_waits_and_a_disconnect_drops_only_its_waiter() -> TestResult {
    const SLOTS: usize = 32;
    let sandbox = Sandbox::new(&json!({ "scripts": [held_turn()] }))?;
    let evidence = Evidence::new("s1_c1_events_long_poll", &sandbox.fake, &sandbox.fixture)?;
    let report = run_scenario(
        evidence,
        |evidence| {
            let _daemon = Daemon::start(&sandbox, evidence)?;
            let session = spawn(&sandbox, evidence, "slow", None)?;
            sandbox.await_gate("slow")?;
            let head = cli(
                &sandbox,
                evidence,
                "events_head",
                &["events", &session, "--json"],
            )?["next_after"]
                .as_u64()
                .ok_or_else(|| failure("the page has no next_after"))?;
            let after = head.to_string();
            let started = Instant::now();
            let idle = cli(
                &sandbox,
                evidence,
                "events_idle",
                &[
                    "events",
                    &session,
                    "--after",
                    &after,
                    "--wait-ms",
                    "1500",
                    "--json",
                ],
            )?;
            let took = started.elapsed();
            check(
                idle == json!({"events":[],"next_after":head,"more":false,"earliest_seq":1})
                    && took >= Duration::from_millis(1500),
                || format!("a 1500 ms long-poll took {took:?}: {idle}"),
            )?;

            let polling = request(
                1,
                "events",
                &json!({"session":session,"after":head,"wait_ms":30_000}),
            );
            let clients = (0..SLOTS)
                .map(|_| Raw::open(&sandbox))
                .collect::<Result<Vec<_>, _>>()?;
            for mut client in clients {
                client.send(&polling)?;
                drop(client);
            }
            let deadline = Instant::now() + Duration::from_secs(10);
            let mut attempts = 0_u32;
            let status = loop {
                attempts += 1;
                let served = Raw::open(&sandbox)
                    .and_then(|mut raw| raw.exchange(&request(2, "daemon/status", &json!({}))));
                match served {
                    Ok(reply) => break reply,
                    Err(error) if Instant::now() >= deadline => {
                        return Err(failure(format!(
                            "no slot freed after {SLOTS} disconnected long-polls \
                             ({attempts} attempts): {error:?}"
                        )));
                    }
                    Err(_) => thread::sleep(Duration::from_millis(20)),
                }
            };
            evidence
                .write(
                    "reconnect.json",
                    json!({"attempts":attempts,"status":status})
                        .to_string()
                        .as_bytes(),
                )
                .map_err(infra)?;
            check(status["result"]["pid"].is_u64(), || {
                format!("daemon/status: {status}")
            })?;

            let ended = thread::scope(|scope| {
                let poll = scope.spawn(|| {
                    cli(
                        &sandbox,
                        evidence,
                        "events_end",
                        &[
                            "events",
                            &session,
                            "--after",
                            &after,
                            "--types",
                            "turn.ended",
                            "--wait-ms",
                            "15000",
                            "--json",
                        ],
                    )
                });
                // The long-poll is pending when the turn ends.
                thread::sleep(Duration::from_millis(300));
                sandbox.release_gate("slow")?;
                poll.join()
                    .map_err(|_| infra("the long-poll thread panicked"))?
            })?;
            check(types(&ended) == ["turn.ended"], || {
                format!("the long-poll for the end: {ended}")
            })?;
            let waited = wait(&sandbox, evidence, &format!("{session}/1"))?;
            check(waited["state"] == "completed", || {
                format!("the turn after the disconnects: {waited}")
            })
        },
        |evidence| collect(evidence, &sandbox),
    );
    report.require_pass()
}

/// Kills and reaps a CLI child left running by a failed scenario.
struct Reaped(std::process::Child);

impl Drop for Reaped {
    fn drop(&mut self) {
        // Safe to ignore: the child may have exited already.
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// C1 §3.11 (via-2lp): `via events --follow` long-polls from each reply's
/// `next_after` and writes each page with events as one JSON line, the
/// format of `via events`; polls that end empty write nothing. The pages
/// carry every event once, in `seq` order, through the held turn's end.
/// Ctrl-C ends it with exit 130.
#[test]
fn s1_c1_events_follow_writes_each_page_until_interrupted() -> TestResult {
    let sandbox = Sandbox::new(&json!({ "scripts": [held_turn()] }))?;
    let evidence = Evidence::new("s1_c1_events_follow", &sandbox.fake, &sandbox.fixture)?;
    let report = run_scenario(
        evidence,
        |evidence| {
            let _daemon = Daemon::start(&sandbox, evidence)?;
            let session = spawn(&sandbox, evidence, "slow", None)?;
            sandbox.await_gate("slow")?;
            let out = evidence.dir.join("follow.stdout");
            let mut command = sandbox.command();
            command
                .args(["events", &session, "--follow", "--wait-ms", "300", "--json"])
                .stdin(std::process::Stdio::null())
                .stdout(std::fs::File::create(&out).map_err(infra)?)
                .stderr(std::fs::File::create(evidence.dir.join("follow.stderr")).map_err(infra)?);
            let mut follow = Reaped(command.spawn().map_err(infra)?);
            let pages = || -> Result<Vec<Value>, ScenarioError> {
                let text = std::fs::read_to_string(&out).map_err(infra)?;
                text.lines()
                    .map(|line| serde_json::from_str(line).map_err(infra))
                    .collect()
            };
            let until = |what: &str, done: &dyn Fn(&[Value]) -> bool| {
                let deadline = Instant::now() + Duration::from_secs(10);
                loop {
                    // A line may be partly written: read again.
                    if let Ok(pages) = pages()
                        && done(&pages)
                    {
                        return Ok(pages);
                    }
                    if Instant::now() >= deadline {
                        return Err(ScenarioError::Timeout(format!("follow never wrote {what}")));
                    }
                    thread::sleep(Duration::from_millis(10));
                }
            };
            let first = until("its first page", &|pages| !pages.is_empty())?;
            // Several 300 ms polls end empty and write nothing.
            thread::sleep(Duration::from_millis(1000));
            check(pages()? == first, || {
                format!("polls without events wrote pages: {:?}", pages())
            })?;
            sandbox.release_gate("slow")?;
            let all = until("the turn's end", &|pages| {
                pages
                    .iter()
                    .any(|page| types(page).iter().any(|kind| kind == "turn.ended"))
            })?;
            let pid = rustix::process::Pid::from_raw(i32::try_from(follow.0.id()).map_err(infra)?)
                .ok_or_else(|| infra("the follow CLI has no pid"))?;
            rustix::process::kill_process(pid, rustix::process::Signal::INT).map_err(infra)?;
            let deadline = Instant::now() + Duration::from_secs(10);
            let status = loop {
                if let Some(status) = follow.0.try_wait().map_err(infra)? {
                    break status;
                }
                if Instant::now() >= deadline {
                    return Err(ScenarioError::Timeout(
                        "follow kept running after Ctrl-C".to_owned(),
                    ));
                }
                thread::sleep(Duration::from_millis(10));
            };
            let followed: Vec<u64> = all.iter().flat_map(seqs).collect();
            let stored = events_head(&sandbox, evidence, &session)?;
            check(
                status.code() == Some(130)
                    && all.iter().all(|page| !seqs(page).is_empty())
                    && followed == (1..=stored).collect::<Vec<_>>(),
                || format!("follow ended {status} with pages {all:?}, head {stored}"),
            )
        },
        |evidence| collect(evidence, &sandbox),
    );
    report.require_pass()
}

/// C1 §3.11 (via-2lp; Sol r1 finding 2): Ctrl-C while `via events
/// --follow` is blocked writing a page into a full pipe never cuts the
/// page when the reader keeps draining. Interrupted mid-page and drained,
/// it exits 130 and everything it wrote is whole JSON lines. Before the
/// fix, the handler flushed a prefix and exited.
#[test]
fn s1_c1_events_follow_interrupted_mid_page_writes_whole_lines() -> TestResult {
    let sandbox = Sandbox::new(&json!({ "scripts": follow_scripts() }))?;
    let evidence = Evidence::new("s1_c1_events_follow_cut", &sandbox.fake, &sandbox.fixture)?;
    let report = run_scenario(
        evidence,
        |evidence| {
            let _daemon = Daemon::start(&sandbox, evidence)?;
            let (mut follow, mut reader, size) = blocked_follow(&sandbox, evidence)?;
            interrupt(&follow)?;
            let drained = thread::spawn(move || {
                let mut out = Vec::new();
                std::io::Read::read_to_end(&mut reader, &mut out).map(|_| out)
            });
            let status = exited(&mut follow, Duration::from_secs(10))?;
            let out = drained
                .join()
                .map_err(|_| infra("the drain thread panicked"))?
                .map_err(infra)?;
            evidence.write("follow.stdout", &out).map_err(infra)?;
            let text = String::from_utf8_lossy(&out);
            let whole = text.lines().all(|line| {
                serde_json::from_str::<Value>(line).is_ok_and(|page| page["events"].is_array())
            });
            check(
                status.code() == Some(130) && out.len() > size && out.ends_with(b"\n") && whole,
                || format!("follow ended {status} after {} B: {text}", out.len()),
            )
        },
        |evidence| collect(evidence, &sandbox),
    );
    report.require_pass()
}

/// C1 §3.11 (via-2lp; critical review finding 1): Ctrl-C ends `via events
/// --follow` at once even when its reader has stopped reading. The CLI is
/// blocked mid-page on a full pipe nobody drains; interrupted, it exits 130
/// within about a second, the last line possibly unfinished. Before the
/// fix, the handler waited forever for the stdout lock the blocked write
/// holds.
#[test]
fn s1_c1_events_follow_interrupted_with_a_stalled_reader_exits_at_once() -> TestResult {
    let sandbox = Sandbox::new(&json!({ "scripts": follow_scripts() }))?;
    let evidence = Evidence::new("s1_c1_events_follow_stall", &sandbox.fake, &sandbox.fixture)?;
    let report = run_scenario(
        evidence,
        |evidence| {
            let _daemon = Daemon::start(&sandbox, evidence)?;
            let (mut follow, reader, _) = blocked_follow(&sandbox, evidence)?;
            let interrupted = Instant::now();
            interrupt(&follow)?;
            let status = exited(&mut follow, Duration::from_secs(5))?;
            let took = interrupted.elapsed();
            // The reader stays open and unread until the CLI has exited.
            drop(reader);
            check(
                status.code() == Some(130) && took < Duration::from_millis(1500),
                || format!("follow ended {status} {took:?} after Ctrl-C"),
            )
        },
        |evidence| collect(evidence, &sandbox),
    );
    report.require_pass()
}

/// Ten turns, each finished, make a first `events` page of about 8 KiB.
fn follow_scripts() -> Vec<Value> {
    (1..=10).map(|n| completes(&format!("p{n}"), n)).collect()
}

/// Runs the ten turns of [`follow_scripts`], then starts `via events
/// --follow` with stdout a 4 KiB pipe the test does not read, and returns
/// once the CLI is blocked mid-page: the page is larger than the pipe, so
/// once more than half of the pipe is queued and nothing more arrives, the
/// CLI waits in a write (a pipe write of one stdout buffer waits until it
/// fits whole). Returns the CLI, the pipe's read end and the pipe's size.
fn blocked_follow(
    sandbox: &Sandbox,
    evidence: &Evidence,
) -> Result<(Reaped, std::io::PipeReader, usize), ScenarioError> {
    let session = spawn(sandbox, evidence, "p1", None)?;
    wait(sandbox, evidence, &format!("{session}/1"))?;
    for n in 2..=10 {
        resume(sandbox, evidence, &session, &format!("p{n}"))?;
        wait(sandbox, evidence, &format!("{session}/{n}"))?;
    }
    let (reader, writer) = std::io::pipe().map_err(infra)?;
    let size = rustix::pipe::fcntl_setpipe_size(&writer, 4096).map_err(infra)?;
    let mut command = sandbox.command();
    command
        .args(["events", &session, "--follow", "--json"])
        .stdin(std::process::Stdio::null())
        .stdout(writer)
        .stderr(std::fs::File::create(evidence.dir.join("follow.stderr")).map_err(infra)?);
    let mut follow = Reaped(command.spawn().map_err(infra)?);
    // Only the child holds the write end now: EOF follows its exit.
    drop(command);
    let deadline = Instant::now() + Duration::from_secs(10);
    let (mut last, mut still) = (0, 0);
    loop {
        let queued =
            usize::try_from(rustix::io::ioctl_fionread(&reader).map_err(infra)?).map_err(infra)?;
        still = if queued == last { still + 1 } else { 0 };
        last = queued;
        if queued > size / 2 && still >= 5 {
            break;
        }
        if Instant::now() >= deadline {
            return Err(ScenarioError::Timeout(format!(
                "follow never blocked on its {size} B pipe ({queued} B queued)"
            )));
        }
        thread::sleep(Duration::from_millis(20));
    }
    check(follow.0.try_wait().map_err(infra)?.is_none(), || {
        "follow exited before it was interrupted".to_owned()
    })?;
    Ok((follow, reader, size))
}

/// Sends SIGINT (Ctrl-C) to `follow`.
fn interrupt(follow: &Reaped) -> Result<(), ScenarioError> {
    let pid = rustix::process::Pid::from_raw(i32::try_from(follow.0.id()).map_err(infra)?)
        .ok_or_else(|| infra("the follow CLI has no pid"))?;
    rustix::process::kill_process(pid, rustix::process::Signal::INT).map_err(infra)
}

/// `follow`'s exit status, once it exits within `bound`.
fn exited(follow: &mut Reaped, bound: Duration) -> Result<std::process::ExitStatus, ScenarioError> {
    let deadline = Instant::now() + bound;
    loop {
        if let Some(status) = follow.0.try_wait().map_err(infra)? {
            return Ok(status);
        }
        if Instant::now() >= deadline {
            return Err(ScenarioError::Timeout(format!(
                "follow kept running {bound:?} after Ctrl-C"
            )));
        }
        thread::sleep(Duration::from_millis(10));
    }
}

/// The session's committed head through a plain `via events` page.
fn events_head(
    sandbox: &Sandbox,
    evidence: &Evidence,
    session: &str,
) -> Result<u64, ScenarioError> {
    let page = cli(
        sandbox,
        evidence,
        "events_final",
        &["events", session, "--limit", "1000", "--json"],
    )?;
    page["next_after"]
        .as_u64()
        .ok_or_else(|| failure(format!("events: {page}")))
}

/// Every session a `via list` paging with `args` returns, in order.
fn list_all(
    sandbox: &Sandbox,
    evidence: &Evidence,
    name: &str,
    args: &[&str],
    between: &mut dyn FnMut(usize) -> Result<(), ScenarioError>,
) -> Result<Vec<Value>, ScenarioError> {
    let mut sessions = Vec::new();
    let mut cursor: Option<String> = None;
    for page in 0.. {
        let mut call = vec!["list", "--json"];
        call.extend_from_slice(args);
        if let Some(cursor) = &cursor {
            call.extend(["--cursor", cursor.as_str()]);
        }
        let listed = cli(sandbox, evidence, &format!("{name}_{page}"), &call)?;
        sessions.extend(listed["sessions"].as_array().cloned().unwrap_or_default());
        match listed["next_cursor"].as_str() {
            Some(next) => cursor = Some(next.to_owned()),
            None => break,
        }
        between(page)?;
    }
    Ok(sessions)
}

fn ids(sessions: &[Value]) -> Vec<String> {
    sessions
        .iter()
        .filter_map(|summary| summary["session_id"].as_str().map(str::to_owned))
        .collect()
}

/// Design §4.5, §6.8, §13.2 [t4r16.4] (A38): sessions page newest first
/// with no repeats while states change, and one created mid-scan never
/// appears; each summary has exactly `{session_id, state, admission,
/// harness, model, label, created_at, last_active_at}`, `last_active_at`
/// the time of the session's latest durable event, which `since` filters
/// on; `label` and `state` filter; a cursor of another version (`l2.`) or
/// malformed is `invalid_params`. The paging bound (1000 sessions examined
/// per page) is `s1_c1_list_page_examines_at_most_1000_sessions_each_once`
/// at the Store level.
#[test]
#[expect(
    clippy::too_many_lines,
    reason = "one scenario keeps the paged walk, the filters and the refused cursors on one daemon"
)]
fn s1_c1_list_creation_order_and_last_active() -> TestResult {
    const SESSIONS: usize = 12;
    let mut scripts: Vec<Value> = (0..SESSIONS)
        .map(|index| completes(&format!("p{index}"), 1))
        .collect();
    scripts.push(completes("again", 2));
    scripts.push(completes("late", 1));
    let sandbox = Sandbox::new(&json!({ "scripts": scripts }))?;
    let evidence = Evidence::new("s1_c1_list_order", &sandbox.fake, &sandbox.fixture)?;
    let report = run_scenario(
        evidence,
        |evidence| {
            let _daemon = Daemon::start(&sandbox, evidence)?;
            let mut created = Vec::new();
            for index in 0..SESSIONS {
                let label = (index == 3).then_some("needle");
                let session = spawn(&sandbox, evidence, &format!("p{index}"), label)?;
                wait(&sandbox, evidence, &format!("{session}/1"))?;
                created.push(session);
            }
            let newest_first: Vec<String> = created.iter().rev().cloned().collect();

            let mut changed = false;
            let mut late = None;
            let listed = list_all(&sandbox, evidence, "list", &["--limit", "5"], &mut |page| {
                if page == 0 {
                    // A state changes and a session is created mid-scan.
                    resume(&sandbox, evidence, &created[0], "again")?;
                    late = Some(spawn(&sandbox, evidence, "late", None)?);
                    changed = true;
                }
                Ok(())
            })?;
            check(changed && ids(&listed) == newest_first, || {
                format!("listed {:?}, created {newest_first:?}", ids(&listed))
            })?;
            let unique: HashSet<String> = ids(&listed).into_iter().collect();
            check(unique.len() == SESSIONS, || "a session repeated".to_owned())?;
            let late = late.ok_or_else(|| failure("no late session"))?;
            wait(&sandbox, evidence, &format!("{late}/1"))?;
            wait(&sandbox, evidence, &format!("{}/2", created[0]))?;

            // Every member, and the latest durable event's time.
            let fresh = list_all(&sandbox, evidence, "fresh", &[], &mut |_| Ok(()))?;
            check(
                ids(&fresh).first() == Some(&late) && fresh.len() == SESSIONS + 1,
                || format!("the late session is not newest: {:?}", ids(&fresh)),
            )?;
            for summary in &fresh {
                let mut keys: Vec<&str> = summary
                    .as_object()
                    .map(|object| object.keys().map(String::as_str).collect())
                    .unwrap_or_default();
                keys.sort_unstable();
                check(
                    keys == [
                        "admission",
                        "created_at",
                        "harness",
                        "label",
                        "last_active_at",
                        "model",
                        "session_id",
                        "state",
                    ] && summary["harness"] == "fake"
                        && summary["model"] == "fake"
                        && summary["admission"] == "open"
                        && summary["state"] == "idle",
                    || format!("summary {summary}"),
                )?;
                let session = summary["session_id"].as_str().unwrap_or_default();
                let page = cli(
                    &sandbox,
                    evidence,
                    &format!("events_{session}"),
                    &["events", session, "--json"],
                )?;
                let last = page["events"]
                    .as_array()
                    .and_then(|events| events.last())
                    .cloned();
                check(
                    last.is_some_and(|event| event["at"] == summary["last_active_at"]),
                    || {
                        format!(
                            "{session}: last_active_at {} is not its latest event's",
                            summary["last_active_at"]
                        )
                    },
                )?;
            }
            let resumed = fresh
                .iter()
                .find(|summary| summary["session_id"] == created[0].as_str())
                .ok_or_else(|| failure("the resumed session is missing"))?;
            let since = resumed["last_active_at"]
                .as_str()
                .unwrap_or_default()
                .to_owned();
            let recent = list_all(
                &sandbox,
                evidence,
                "since",
                &["--since", &since],
                &mut |_| Ok(()),
            )?;
            let expected: Vec<String> = fresh
                .iter()
                .filter(|summary| summary["last_active_at"].as_str() >= Some(since.as_str()))
                .filter_map(|summary| summary["session_id"].as_str().map(str::to_owned))
                .collect();
            check(
                ids(&recent) == expected && expected.contains(&created[0]),
                || format!("since {since}: {:?}, expected {expected:?}", ids(&recent)),
            )?;
            let needle = list_all(
                &sandbox,
                evidence,
                "needle",
                &["--label", "needle"],
                &mut |_| Ok(()),
            )?;
            check(
                ids(&needle) == [created[3].clone()] && needle[0]["label"] == "needle",
                || format!("label filter: {needle:?}"),
            )?;
            let active = list_all(
                &sandbox,
                evidence,
                "active",
                &["--state", "active"],
                &mut |_| Ok(()),
            )?;
            check(active.is_empty(), || format!("state filter: {active:?}"))?;
            for (name, cursor) in [
                ("l2", "l2.5"),
                ("empty", "l3."),
                ("sign", "l3.-1"),
                ("word", "l3.x"),
                // An `ord` is an SQLite integer: above i64::MAX is malformed.
                ("above_i64", "l3.9223372036854775808"),
            ] {
                refused(
                    &sandbox,
                    evidence,
                    &format!("cursor_{name}"),
                    &["list", "--cursor", cursor, "--json"],
                    "invalid_params",
                )?;
            }
            Ok(())
        },
        |evidence| collect(evidence, &sandbox),
    );
    report.require_pass()
}

/// Scenario cleanup: records every stored envelope and event as evidence,
/// then collects the Store and the evidence folders.
fn collect(evidence: &Evidence, sandbox: &Sandbox) -> Result<(), ScenarioError> {
    let path = sandbox.state.join("store.sqlite3");
    let (mut envelopes, mut events) = (String::new(), String::new());
    if path.is_file() {
        let store = rusqlite::Connection::open_with_flags(
            &path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .map_err(infra)?;
        for (sql, out) in [
            (
                "SELECT envelope FROM turns WHERE envelope IS NOT NULL ORDER BY session_id,number",
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
    collect_available(evidence, &sandbox.state, &sandbox.teardown)
}

//! The shared `drive()` harness's run half (x.3.2 plan, C2): the turns of
//! a conformance case that plan, run through the real `SessionDriver` over
//! Route, Wire and Host, against the fake replaying the case's fixture.
//! [`Pure::drive`] finishes what [`Pure::run`] started.
//!
//! In the checker's order (`conformance_expect.rs`, `drive()` obligations):
//! - every session is opened logically with its spawn plan, its frozen
//!   values and a working directory of the case's own (the fixtures' `cwd`
//!   is a recorded path the vendor never reads back);
//! - the turns run in order: a turn waits for the previous turn of the
//!   case to settle, or, with `start_after`, for that event of the named
//!   turn; a session's turns never overlap (Core's FIFO). Each turn is
//!   committed to the Store first, as Core commits it before the driver
//!   runs it;
//! - a turn's `stop`, `steer` attempts and `gates` act at their events: a
//!   turn event (`accepted`, `tool_started`, `handshake`, the first
//!   identity confirmation or acceptance, whichever comes first) seen on
//!   the session's observation channel, or a gate's `at <step> launch <n>`
//!   line on the fake's progress log;
//! - a session's stated `close` runs as soon as its own last turn settled;
//! - `launches` and every checkpoint come from the launch log, and each
//!   launch a per-turn route's turn made is judged by the replay's own
//!   verdict ([`replay_exit`]) from the turn's exit and its `stderr.log`.
//!
//! Time is real, not paused: Host's own waits poll with timers, which a
//! paused clock would never fire. What the checker calls controlled time
//! is kept by the gates' ordering (the fake waits at each `await_signal`
//! until the driver snapshots and signals it) and by a gate's `advance_ms`,
//! which the driver waits out before its snapshot, so every timer armed
//! before it has fired.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::Command;
use std::rc::Rc;
use std::task::Poll;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::sync::{mpsc, watch};
use via_adapters::{
    AdapterError, Admitted, CancellationToken, ClassHint, Cleanup, CloseMode, Deadline, DenialKind,
    DriverFailure, DriverHealth, InheritPlan, Observation, ObservationItem, Prepared, RouteError,
    RoutePlan, SessionCx, SessionDriver, SessionRef, SessionSpec, StartRejected, SteerDelivery,
    SteerError, SteerInput, SteerToken, StopCause, StopOrder, StopReason, TaskTracker,
    TurnActivity, TurnCx, TurnEnd, TurnNumber, TurnSpec, VendorTerminal, VendorTerminalStatus,
    VendorTurnId, observation_channel,
};
use via_store::{ResumeRecord, SessionId, SpawnRecord, SubmissionRecord, TerminalRecord};

use crate::conformance_drive::Pure;
use crate::conformance_expect::{TurnOutcome, replay_exit};

/// The wall of a turn whose case states no deadline.
const WALL: Duration = Duration::from_secs(30);
/// C1 P7's tool-grace window when a turn states none.
const TOOL_GRACE: Duration = Duration::from_secs(60);
/// A stop order's `force_at` after its event: room for the soft stop.
const STOP_FORCE: Duration = Duration::from_secs(5);
/// S1's cleanup allowance after `force_at`.
const CLOSE_BY: Duration = Duration::from_secs(3);
/// A session close's deadline.
const CLOSE_DEADLINE: Duration = Duration::from_secs(10);
/// The bound on any wait for the fake's progress log.
const FIXTURE_WAIT: Duration = Duration::from_secs(10);
/// How often the driver polls the fake's logs.
const POLL: Duration = Duration::from_millis(10);
/// A gate's observations are taken once none arrived for this long.
const QUIET: Duration = Duration::from_millis(200);
/// How long the real opens' interval stays open after the last one: a
/// start or write an open spawned asynchronously counts there. A time
/// window, not a barrier (review r2 #5): `open_session` gives no
/// completion signal for work it might schedule, so an effect delayed past
/// this window would land during the turn, outside `pure_writes` and
/// `after_open`.
const OPEN_SETTLE: Duration = Duration::from_millis(100);

/// How a case is driven beyond its expectation: a test seam for cases the
/// schema cannot state.
#[derive(Clone, Copy, Debug, Default)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "each is one independent seam a case turns on"
)]
pub(crate) struct Knobs {
    /// Core takes no observation of launch 1 until the fake has read its
    /// `n`th input line (`read <n> launch 1`): the session's channel
    /// fills, as when Core's consumer stalls.
    pub(crate) hold_until_read: Option<usize>,
    /// The stop is ordered twice, a fresh order 50 ms after the first: a
    /// duplicate cancel, which must not write a second interrupt.
    pub(crate) repeat_stop: bool,
    /// Every session is opened with `allow_untested` set.
    pub(crate) allow_untested: bool,
    /// Core never takes an observation while the turn runs: its consumer
    /// stalls past the driver's stall bound.
    pub(crate) stall_consumer: bool,
    /// The daemon force is set once the fake logs this progress line (for
    /// example `at 5 launch 1`).
    pub(crate) force_on: Option<&'static str>,
    /// Core takes no observation for this long after the turn starts.
    pub(crate) hold_for: Option<Duration>,
    /// A cancel is ordered this long after the turn starts.
    pub(crate) stop_after: Option<Duration>,
    /// Each session's health is read once it left `open` (within
    /// [`FIXTURE_WAIT`]): a failure an idle driver latches after its last
    /// turn settled (x.3.2 X0 item 13.2).
    pub(crate) await_failure: bool,
}

/// What a running turn has shown so far, for its side actions.
#[derive(Clone, Copy, Debug, Default)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "each is one event a side action waits for"
)]
struct Seen {
    accepted: bool,
    tool_started: bool,
    handshake: bool,
    settled: bool,
}

impl Seen {
    fn has(self, event: &str) -> bool {
        match event {
            "accepted" => self.accepted,
            "tool_started" => self.tool_started,
            "handshake" => self.handshake,
            _ => false,
        }
    }
}

/// One opened session.
struct Session {
    id: SessionId,
    driver: SessionDriver,
    receiver: RefCell<Option<mpsc::Receiver<Admitted>>>,
    plan: RoutePlan,
    /// The turns committed so far.
    turns: RefCell<u32>,
}

/// The shared state of one case's run.
struct Run<'a> {
    pure: &'a Pure,
    expect: &'a Value,
    replay: &'a Value,
    knobs: Knobs,
    sessions: BTreeMap<String, Session>,
    /// Each opened session's launch count right after its real
    /// `open_session()`: its `after_open` checkpoint.
    opened: BTreeMap<String, u64>,
    /// Each turn's progress, by index.
    seen: Vec<watch::Sender<Seen>>,
    /// The connection IDs and acceptance tokens of the case, first seen
    /// first: an observation's ordinal among them.
    ordinals: RefCell<(Vec<String>, Vec<u64>)>,
    tracker: TaskTracker,
    cancel: CancellationToken,
}

impl Pure {
    /// Runs the case: its planned turns through the real driver, then the
    /// outcome. A case whose every turn was refused needs no driver.
    pub(crate) fn drive(
        self,
        expect: &Value,
        replay: &Path,
        knobs: Knobs,
    ) -> Result<crate::conformance_expect::Outcome, String> {
        self.drive_then(expect, replay, knobs, |_| Ok(()))
    }

    /// [`Pure::drive`], then `then` on the adapter set the case ran on,
    /// once every turn settled: a test's checks of what the run left behind
    /// (a cached refusal).
    pub(crate) fn drive_then(
        self,
        expect: &Value,
        replay: &Path,
        knobs: Knobs,
        then: impl FnOnce(&Self) -> Result<(), String>,
    ) -> Result<crate::conformance_expect::Outcome, String> {
        if self.pending.is_empty() {
            then(&self)?;
            return self.planned_only();
        }
        let text = fs::read_to_string(replay).map_err(|e| format!("replay: {e}"))?;
        let replay: Value = serde_json::from_str(&text).map_err(|e| format!("replay: {e}"))?;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| format!("runtime: {e}"))?;
        let mut pure = self;
        let result = runtime.block_on(async {
            let mut run = Run::open(&pure, expect, &replay, knobs)?;
            // The open interval ends once whatever the opens spawned had
            // its chance to run: a start counts at the last open.
            tokio::time::sleep(OPEN_SETTLE).await;
            let settled = pure.launches()?;
            if let Some(last) = run.opened.values_mut().last() {
                *last = settled;
            }
            let opened = (std::mem::take(&mut run.opened), pure.writes()?);
            let turns = run.all().await;
            if knobs.await_failure {
                run.until_failed().await;
            }
            let health = run.health();
            let servers = run.shutdown().await;
            let turns = turns?;
            servers?;
            Ok::<_, String>((turns, health, opened))
        });
        let (turns, health, (opened, writes)) = result?;
        pure.outcome.checkpoints.after_open.extend(opened);
        pure.outcome.pure_writes = writes;
        then(&pure)?;
        let launches = pure.launches()?;
        for (index, (outcome, after)) in turns.into_iter().enumerate() {
            if pure.pending.contains(&index) {
                pure.outcome.turns[index] = outcome.turn;
                if let Some((label, close)) = outcome.close {
                    pure.outcome.closes.insert(label, Some(close));
                }
            }
            pure.outcome.checkpoints.after_turn.push(after);
        }
        pure.outcome.health = health;
        pure.outcome.launches = launches;
        Ok(std::mem::take(&mut pure.outcome))
    }
}

/// One turn's result in the run.
#[derive(Default)]
struct Ran {
    turn: TurnOutcome,
    /// The session's close, when this turn was its last and it states one.
    close: Option<(String, Value)>,
}

impl<'a> Run<'a> {
    /// Opens every planned session, in label order.
    fn open(
        pure: &'a Pure,
        expect: &'a Value,
        replay: &'a Value,
        knobs: Knobs,
    ) -> Result<Self, String> {
        let turns = expect["turns"].as_array().ok_or("turns")?;
        let tracker = TaskTracker::new();
        let cancel = CancellationToken::new();
        let mut sessions = BTreeMap::new();
        let mut opened = BTreeMap::new();
        for (number, (label, plan)) in pure.plans.iter().enumerate() {
            let session = &expect["sessions"][label];
            let first = turns
                .iter()
                .find(|turn| session_of(turn) == label)
                .ok_or_else(|| format!("session {label} has no turn"))?;
            let id = SessionId::try_from(format!("s_{:012}", number + 1).as_str())
                .map_err(str::to_owned)?;
            // A server route sends the session's directory to the vendor,
            // which the fixture pins: the recorded `cwd` itself, never
            // created (x.3.2 X3). A per-turn route gets one of the case's.
            let cwd = if let (Some(_), Some(recorded)) = (&plan.server_key, session["cwd"].as_str())
            {
                PathBuf::from(recorded)
            } else {
                let cwd = pure.case_dir.path().join(format!("work-{label}"));
                fs::create_dir_all(&cwd).map_err(|e| format!("cwd: {e}"))?;
                cwd
            };
            let spec = SessionSpec {
                session_id: id.clone(),
                model: plan.model.resolved.clone(),
                instructions: session["instructions"].as_str().map(str::to_owned),
                initial_bound: optional(&first["params"]["bound"])?,
                cwd,
                vendor: optional(&session["vendor_options"])?.unwrap_or_default(),
                inherit: InheritPlan {
                    requested: plan.inherit.requested,
                    effective: plan.inherit.effective,
                },
                confirmed_vendor_session_id: session["resume"].as_str().map(str::to_owned),
                allow_untested: knobs.allow_untested,
            };
            let session_ref = SessionRef {
                harness: plan.harness.to_owned(),
                route: plan.route.to_owned(),
                adapter_version: plan.adapter_version.clone(),
            };
            let (observations, receiver) = observation_channel();
            let cx = SessionCx {
                observations,
                tracker: tracker.clone(),
                cancel: cancel.clone(),
            };
            let driver = pure.set.open_session(&session_ref, spec, cx);
            opened.insert(label.clone(), pure.launches()?);
            sessions.insert(
                label.clone(),
                Session {
                    id,
                    driver,
                    receiver: RefCell::new(Some(receiver)),
                    plan: plan.clone(),
                    turns: RefCell::new(0),
                },
            );
        }
        let seen = turns
            .iter()
            .map(|_| watch::Sender::new(Seen::default()))
            .collect();
        Ok(Self {
            pure,
            expect,
            replay,
            knobs,
            sessions,
            opened,
            seen,
            ordinals: RefCell::new((Vec::new(), Vec::new())),
            tracker,
            cancel,
        })
    }

    /// Runs every turn, each at its start condition; returns each turn's
    /// result and the launch count once it settled.
    async fn all(&self) -> Result<Vec<(Ran, u64)>, String> {
        let turns = self.expect["turns"].as_array().ok_or("turns")?;
        let futures: Vec<Running<'_>> = turns
            .iter()
            .enumerate()
            .map(|(index, turn)| {
                Box::pin(self.turn(index, turn)) as Pin<Box<dyn Future<Output = _> + '_>>
            })
            .collect();
        join_all(futures).await.into_iter().collect()
    }

    /// One turn: its start condition, then the turn (or its refusal),
    /// then the session's close when it was the session's last.
    async fn turn(&self, index: usize, turn: &Value) -> Result<(Ran, u64), String> {
        self.start_condition(index, turn).await?;
        let label = session_of(turn).to_owned();
        let mut ran = Ran::default();
        if self.pure.pending.contains(&index) {
            let session = self
                .sessions
                .get(&label)
                .ok_or_else(|| format!("turn {index}: session {label} was not opened"))?;
            ran.turn = self.run_turn(index, turn, session).await?;
            let last = self.expect["turns"]
                .as_array()
                .and_then(|turns| turns.iter().rposition(|t| session_of(t) == label));
            if last == Some(index)
                && let Some(close) = self.expect["sessions"][&label]["close"].as_object()
            {
                let mode = match close.get("mode").and_then(Value::as_str) {
                    Some("force") => CloseMode::Force,
                    _ => CloseMode::Graceful,
                };
                let deadline = Deadline::at(tokio::time::Instant::now() + CLOSE_DEADLINE);
                let report = session.driver.close(mode, deadline).await;
                ran.close = Some((
                    label.clone(),
                    json!({
                        "vendor_closed": report.vendor_closed,
                        "cleanup": cleanup_name(report.cleanup),
                    }),
                ));
            }
        }
        let after = self.pure.launches()?;
        self.seen[index].send_modify(|seen| seen.settled = true);
        Ok((ran, after))
    }

    /// A turn starts at its `start_after` event, else once the turn before
    /// it settled; never before its session's previous turn settled.
    async fn start_condition(&self, index: usize, turn: &Value) -> Result<(), String> {
        let label = session_of(turn);
        let turns = self.expect["turns"].as_array().ok_or("turns")?;
        if let Some(after) = turn.get("start_after").filter(|after| !after.is_null()) {
            let earlier = after["turn"]
                .as_u64()
                .and_then(|turn| usize::try_from(turn).ok())
                .filter(|turn| *turn < index)
                .ok_or_else(|| format!("turn {index}: start_after.turn"))?;
            let event = after["event"].as_str().unwrap_or_default().to_owned();
            let mut seen = self.seen[earlier].subscribe();
            let _ = seen.wait_for(|seen| seen.has(&event) || seen.settled).await;
        } else if index > 0 {
            let mut seen = self.seen[index - 1].subscribe();
            let _ = seen.wait_for(|seen| seen.settled).await;
        }
        for earlier in (0..index).rev() {
            if session_of(&turns[earlier]) == label {
                let mut seen = self.seen[earlier].subscribe();
                let _ = seen.wait_for(|seen| seen.settled).await;
                break;
            }
        }
        Ok(())
    }

    /// Commits the session's next turn, as Core does before running it.
    async fn commit(&self, session: &Session, prompt: &str) -> Result<TurnNumber, String> {
        let number = {
            let mut turns = session.turns.borrow_mut();
            *turns += 1;
            *turns
        };
        let turn = TurnNumber::try_from(number).map_err(|e| format!("turn number: {e:?}"))?;
        let at = "2026-01-01T00:00:00.000Z";
        let client = self.pure.store().client();
        let committed = if number == 1 {
            client
                .commit_spawn(SpawnRecord {
                    session_id: session.id.clone(),
                    handle_hash: [7_u8; 32],
                    receipt: json!({"state": "queued"}),
                    params: json!({"harness": session.plan.harness}),
                    label: None,
                    prompt: prompt.into(),
                    effective: json!({"deadlines": {"wall_ms": 1}}),
                    initial_event: json!({"seq": 1, "type": "turn.queued", "turn": 1, "at": at}),
                })
                .await
                .map(drop)
        } else {
            // A server route's turn stays running until its terminal
            // commits, as Core commits it; the harness ends it here so the
            // next submission may run (x.3.2 X3).
            if session.plan.server_key.is_some() {
                let previous =
                    TurnNumber::try_from(number - 1).map_err(|e| format!("turn number: {e:?}"))?;
                let seq = client
                    .next_seq(&session.id)
                    .await
                    .map_err(|e| format!("next seq: {e}"))?
                    .ok_or("no next seq")?;
                client
                    .commit_terminal(TerminalRecord {
                        session_id: session.id.clone(),
                        turn: previous,
                        envelope: json!({"state": "failed"}),
                        event: json!({"seq": seq, "type": "turn.ended", "turn": number - 1, "at": at}),
                        steps: Vec::new(),
                        link_released: true,
                    })
                    .await
                    .map_err(|e| format!("end turn {}: {e}", number - 1))?;
            }
            // A server route's submissions took sequence numbers too.
            let seq = if session.plan.server_key.is_some() {
                client
                    .next_seq(&session.id)
                    .await
                    .map_err(|e| format!("next seq: {e}"))?
                    .ok_or("no next seq")?
            } else {
                u64::from(number)
            };
            client
                .commit_resume(ResumeRecord {
                    session_id: session.id.clone(),
                    turn,
                    prompt: prompt.into(),
                    effective: json!({"deadlines": {"wall_ms": 1}}),
                    event: json!({"seq": seq, "type": "turn.queued", "turn": number, "at": at}),
                    operation: None,
                })
                .await
                .map(drop)
        };
        committed.map_err(|e| format!("commit turn {number}: {e}"))?;
        // A server route links the turn to its server, which needs the
        // turn running, as Core's submission makes it (x.3.2 X3).
        if session.plan.server_key.is_some() {
            let seq = client
                .next_seq(&session.id)
                .await
                .map_err(|e| format!("next seq: {e}"))?
                .ok_or("no next seq")?;
            client
                .commit_submission(SubmissionRecord {
                    session_id: session.id.clone(),
                    turn,
                    event: json!({"seq": seq, "type": "turn.submitted", "turn": number, "at": at}),
                })
                .await
                .map_err(|e| format!("submit turn {number}: {e}"))?;
        }
        Ok(turn)
    }

    /// The turn's spec, from its `params` and its session's vendor
    /// options.
    fn turn_spec(&self, turn: &Value) -> Result<TurnSpec, String> {
        let params = &turn["params"];
        Ok(TurnSpec {
            prompt: params["prompt"].as_str().unwrap_or_default().to_owned(),
            effort: params["effort"].as_str().map(str::to_owned),
            bound: optional(&params["bound"])?,
            output_schema: if params["output_schema"].is_null() {
                None
            } else {
                Some(
                    serde_json::value::to_raw_value(&params["output_schema"])
                        .map_err(|e| format!("output_schema: {e}"))?,
                )
            },
            max_steps: params["max_steps"].as_u64(),
            vendor: optional(&self.expect["sessions"][session_of(turn)]["vendor_options"])?
                .unwrap_or_default(),
        })
    }

    /// Runs one planned turn beside its side actions and collects its
    /// outcome.
    async fn run_turn(
        &self,
        index: usize,
        turn: &Value,
        session: &Session,
    ) -> Result<TurnOutcome, String> {
        let spec = self.turn_spec(turn)?;
        let number = self.commit(session, &spec.prompt).await?;
        let wall = turn["deadlines"]["wall_ms"]
            .as_u64()
            .map_or(WALL, Duration::from_millis);
        let tool_grace = turn["tool_grace_ms"]
            .as_u64()
            .map_or(TOOL_GRACE, Duration::from_millis);
        let (stop, stop_rx) = watch::channel(None);
        let (force, force_rx) = watch::channel(None);
        let prepared = session.driver.prepare();
        let capacity = matches!(prepared, Prepared::NeedsConnection)
            .then(|| Box::new(()) as via_adapters::CapacityToken);
        let now = tokio::time::Instant::now();
        let cx = TurnCx {
            turn: number,
            prepared,
            capacity,
            activity: TurnActivity::new(now),
            wall: Deadline::at(now + wall),
            tool_grace,
            stop: stop_rx,
            force: force_rx,
        };
        let launches_before = self.pure.launches()?;
        let observed = Rc::new(RefCell::new(Vec::<Value>::new()));
        let mut receiver = session
            .receiver
            .borrow_mut()
            .take()
            .ok_or("the session's channel is in use")?;
        let seen = &self.seen[index];
        let (ended_tx, ended) = watch::channel(false);
        let tools = RefCell::new(Tools::default());
        let settled = std::cell::Cell::new(None);
        let drain = async {
            let waiting = self.consumer_hold();
            tokio::pin!(waiting);
            let mut released = None;
            let end = {
                let running = session.driver.run_turn(spec, cx);
                tokio::pin!(running);
                loop {
                    tokio::select! {
                        result = &mut waiting, if released.is_none() => released = Some(result),
                        Some(admitted) = receiver.recv(),
                            if released.is_some() && !self.knobs.stall_consumer => {
                            tools.borrow_mut().track(&admitted.item);
                            self.observe(&admitted.item.observation, seen, &observed);
                        }
                        end = &mut running => {
                            settled.set(Some(tokio::time::Instant::now()));
                            break end;
                        }
                    }
                }
            };
            let released = if let Some(result) = released {
                result
            } else {
                waiting.await
            };
            while let Ok(admitted) = receiver.try_recv() {
                tools.borrow_mut().track(&admitted.item);
                self.observe(&admitted.item.observation, seen, &observed);
            }
            ended_tx.send_replace(true);
            released.map(|()| end)
        };
        let side = self.side(
            turn,
            (session, number),
            (seen, ended.clone()),
            (&stop, &observed),
        );
        let forcing = self.timed(&force, &stop, ended.clone());
        let (end, (steer, gates), ()) = tokio::join!(drain, side, forcing);
        *session.receiver.borrow_mut() = Some(receiver);
        let end = end?;
        let mut outcome = Self::outcome(&end, session, &observed.borrow());
        outcome.group_absent = self.group_absent(&session.id, number).await?;
        outcome.steer = steer;
        outcome.gates = gates?;
        outcome.stop_facts = stop_facts(turn, &end);
        // Only a stopped turn on a server route settles its cleanup apart
        // from its end (C1 P7); elsewhere the field is null.
        if session.plan.server_key.is_some() && !turn["stop"].is_null() {
            outcome.cleanup_settles = settled.get().map(|settled| {
                settles(&end, &tools.borrow(), settled, (now + wall, tool_grace)).to_owned()
            });
        }
        if self.pure.launches()? > launches_before && session.plan.server_key.is_none() {
            self.judge_launch(launches_before + 1, session, number, &end)?;
        }
        Ok(outcome)
    }

    /// Sets the daemon force at [`Knobs::force_on`]'s progress line, and
    /// orders a cancel at [`Knobs::stop_after`], unless the turn ended
    /// first.
    async fn timed(
        &self,
        force: &watch::Sender<Option<tokio::time::Instant>>,
        stop: &watch::Sender<Option<StopOrder>>,
        mut ended: watch::Receiver<bool>,
    ) {
        let forcing = async {
            if let Some(line) = self.knobs.force_on
                && self.until_progress(line).await.is_ok()
            {
                force.send_replace(Some(tokio::time::Instant::now()));
            }
        };
        let stopping = async {
            if let Some(after) = self.knobs.stop_after {
                tokio::time::sleep(after).await;
                let now = tokio::time::Instant::now();
                stop.send_replace(Some(StopOrder {
                    cause: StopCause::Cancel,
                    requested_at: "2026-01-01T00:00:00.000Z".to_owned(),
                    force_at: Deadline::at(now + STOP_FORCE),
                    close_by: Deadline::at(now + STOP_FORCE + CLOSE_BY),
                }));
            }
        };
        tokio::select! {
            ((), ()) = async { tokio::join!(forcing, stopping) } => {}
            _ = ended.wait_for(|ended| *ended) => {}
        }
    }

    /// Resolves once Core may take the turn's observations: at once, or
    /// after [`Knobs::hold_for`], then with [`Knobs::hold_until_read`] once
    /// the fake read that line.
    async fn consumer_hold(&self) -> Result<(), String> {
        if let Some(hold) = self.knobs.hold_for {
            tokio::time::sleep(hold).await;
        }
        if let Some(read) = self.knobs.hold_until_read {
            self.until_progress(&format!("read {read} launch 1"))
                .await?;
        }
        Ok(())
    }

    /// Takes one observation: its checker shape, and the turn's events.
    fn observe(
        &self,
        observation: &Observation,
        seen: &watch::Sender<Seen>,
        observed: &Rc<RefCell<Vec<Value>>>,
    ) {
        seen.send_modify(|seen| match observation {
            Observation::Accepted(_) => {
                seen.accepted = true;
                seen.handshake = true;
            }
            Observation::IdentityConfirmed(_) => seen.handshake = true,
            Observation::Progress(marks) => seen.tool_started |= !marks.tools_started.is_empty(),
            Observation::FinalText(_)
            | Observation::ActionDenied(_)
            | Observation::RequestDeclined(_)
            | Observation::SteerDelivered { .. }
            | Observation::Warning(_)
            | Observation::VendorClosed(_)
            | Observation::ResumeMismatch { .. }
            | Observation::LateTerminal(_) => {}
        });
        let shaped = self.shape(observation);
        observed.borrow_mut().push(shaped);
    }

    /// The checker's shape of an observation (its module docs).
    fn shape(&self, observation: &Observation) -> Value {
        let mut ordinals = self.ordinals.borrow_mut();
        match observation {
            Observation::IdentityConfirmed(identity) => {
                let ids = &mut ordinals.0;
                let generation = ordinal(ids, &identity.connection_id);
                json!({"kind": "session.vendor_identity_confirmed",
                    "vendor_session_id": identity.vendor_session_id, "generation": generation})
            }
            Observation::Accepted(acceptance) => {
                let tokens = &mut ordinals.1;
                let token = acceptance.correlation.get();
                let correlation = if let Some(at) = tokens.iter().position(|seen| *seen == token) {
                    at + 1
                } else {
                    tokens.push(token);
                    tokens.len()
                };
                json!({"kind": "turn.accepted", "correlation": correlation,
                    "vendor_turn_id": acceptance.vendor_turn_id.as_ref().map(VendorTurnId::as_str)})
            }
            Observation::Progress(marks) => json!({"kind": "progress",
                "model": marks.model,
                "tools_started": marks.tools_started.iter()
                    .map(|(id, name)| json!([id, name])).collect::<Vec<_>>(),
                "tools_ended": marks.tools_ended,
                "usage": marks.usage.as_ref().map(usage_sample)}),
            Observation::FinalText(text) => json!({"kind": "final_text", "text": text}),
            Observation::ActionDenied(denial) => json!({"kind": "action.denied",
                "denial_kind": denial_kind(denial.kind), "target": denial.target,
                "reason": denial.reason}),
            Observation::RequestDeclined(decline) => json!({"kind": "vendor.request_declined",
                "vendor_method": decline.vendor_method, "summary": decline.summary,
                "blocking": decline.blocking}),
            Observation::SteerDelivered { delivery, .. } => {
                json!({"kind": "steer.delivered", "delivery": delivery_name(delivery)})
            }
            Observation::Warning(warning) => json!({"kind": "warning", "code": warning.code}),
            Observation::VendorClosed(reason) => {
                json!({"kind": "session.vendor_closed", "reason": reason})
            }
            Observation::ResumeMismatch {
                requested,
                returned,
            } => json!({"kind": "resume.mismatch", "requested": requested, "returned": returned}),
            Observation::LateTerminal(_) => json!({"kind": "turn.late_terminal"}),
        }
    }

    /// The turn's `stop`, `steer` attempts and `gates`, each at its event;
    /// one whose event never comes before the turn ended does nothing (a
    /// steer's result is then `never_attempted`; a gate takes no snapshot).
    async fn side(
        &self,
        turn: &Value,
        (session, number): (&Session, TurnNumber),
        (seen, ended): (&watch::Sender<Seen>, watch::Receiver<bool>),
        (stop_order, observed): (&watch::Sender<Option<StopOrder>>, &Rc<RefCell<Vec<Value>>>),
    ) -> (Vec<String>, Result<Vec<TurnOutcome>, String>) {
        let at_event = |event: String| {
            let mut seen = seen.subscribe();
            let mut ended = ended.clone();
            async move {
                let found = tokio::select! {
                    biased;
                    found = seen.wait_for(|seen| seen.has(&event)) => found.is_ok(),
                    _ = ended.wait_for(|ended| *ended) => false,
                };
                found || seen.borrow().has(&event)
            }
        };
        let stopping = async {
            let Some(order) = turn.get("stop").filter(|stop| !stop.is_null()) else {
                return;
            };
            let after = order["after"].as_str().unwrap_or_default().to_owned();
            if !at_event(after).await {
                return;
            }
            let now = tokio::time::Instant::now();
            if order["kind"].as_str() == Some("close") {
                let deadline = Deadline::at(now + CLOSE_DEADLINE);
                let _report = session.driver.close(CloseMode::Graceful, deadline).await;
            } else {
                // A cancel (`interrupt`); the wall is the turn's own.
                let order = |now| StopOrder {
                    cause: StopCause::Cancel,
                    requested_at: "2026-01-01T00:00:00.000Z".to_owned(),
                    force_at: Deadline::at(now + STOP_FORCE),
                    close_by: Deadline::at(now + STOP_FORCE + CLOSE_BY),
                };
                stop_order.send_replace(Some(order(now)));
                if self.knobs.repeat_stop {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    stop_order.send_replace(Some(order(tokio::time::Instant::now())));
                }
            }
        };
        let steering = async {
            let mut results = Vec::new();
            for (token, attempt) in turn["steer"].as_array().into_iter().flatten().enumerate() {
                let after = attempt["after"].as_str().unwrap_or_default().to_owned();
                if !at_event(after).await {
                    results.push("never_attempted".to_owned());
                    continue;
                }
                let input = SteerInput {
                    turn: number,
                    token: SteerToken::new(u64::try_from(token).unwrap_or(u64::MAX) + 1),
                    text: attempt["text"].as_str().unwrap_or_default().to_owned(),
                    expected_vendor_turn: attempt["expected_vendor_turn"]
                        .as_str()
                        .and_then(|id| VendorTurnId::try_from(id.to_owned()).ok()),
                };
                results.push(match session.driver.steer(input).await {
                    Ok(delivery) => delivery_name(&delivery),
                    Err(error) => steer_error_name(&error).to_owned(),
                });
            }
            results
        };
        let gating = async {
            let mut snapshots = Vec::new();
            for gate in turn["gates"].as_array().into_iter().flatten() {
                let step = gate["step"].as_u64().unwrap_or_default();
                let launch = gate["lifetime"].as_u64().unwrap_or(1);
                let mut ended = ended.clone();
                let at = format!("at {step} launch {launch}");
                let reached = tokio::select! {
                    reached = self.until_progress(&at) => reached,
                    _ = ended.wait_for(|ended| *ended) => break,
                };
                reached?;
                self.quiet(observed).await;
                if let Some(ms) = gate["advance_ms"].as_u64() {
                    tokio::time::sleep(Duration::from_millis(ms)).await;
                }
                snapshots.push(snapshot(&observed.borrow(), &session.plan));
                self.signal(launch)?;
                self.until_progress(&format!("signalled {step} launch {launch}"))
                    .await?;
            }
            Ok(snapshots)
        };
        let ((), steer, gates) = tokio::join!(stopping, steering, gating);
        (steer, gates)
    }

    /// Waits until no observation arrived for [`QUIET`], within
    /// [`FIXTURE_WAIT`].
    async fn quiet(&self, observed: &Rc<RefCell<Vec<Value>>>) {
        let started = tokio::time::Instant::now();
        let mut count = observed.borrow().len();
        loop {
            tokio::time::sleep(QUIET).await;
            let now = observed.borrow().len();
            if now == count || started.elapsed() > FIXTURE_WAIT {
                return;
            }
            count = now;
        }
    }

    /// Waits for `line` on the fake's progress log.
    async fn until_progress(&self, line: &str) -> Result<(), String> {
        let path = self.pure.case_file("progress");
        let started = tokio::time::Instant::now();
        loop {
            if fs::read_to_string(&path).is_ok_and(|text| text.lines().any(|seen| seen == line)) {
                return Ok(());
            }
            if started.elapsed() > FIXTURE_WAIT {
                return Err(format!("the fake never logged {line:?}"));
            }
            tokio::time::sleep(POLL).await;
        }
    }

    /// Sends launch `launch`'s fake its gate signal.
    fn signal(&self, launch: u64) -> Result<(), String> {
        let log = fs::read_to_string(self.pure.case_file("launches"))
            .map_err(|e| format!("launch log: {e}"))?;
        let index = usize::try_from(launch).map_err(|e| e.to_string())?;
        let pid = log
            .lines()
            .nth(index.saturating_sub(1))
            .ok_or_else(|| format!("no launch {launch} to signal"))?;
        let status = Command::new("kill")
            .args(["-USR1", pid.trim()])
            .status()
            .map_err(|e| format!("kill: {e}"))?;
        if status.success() {
            Ok(())
        } else {
            Err(format!("kill -USR1 {pid} failed"))
        }
    }

    /// The replay's own verdict on launch `launch`, which turn `turn` of
    /// `session` made: the fixture's (or the lifetime's) end, from the
    /// turn's exit and its `stderr.log`.
    fn judge_launch(
        &self,
        launch: u64,
        session: &Session,
        turn: TurnNumber,
        end: &TurnEnd,
    ) -> Result<(), String> {
        let fixture = match self.replay.get("lifetimes").and_then(Value::as_array) {
            Some(lifetimes) => usize::try_from(launch)
                .ok()
                .and_then(|launch| lifetimes.get(launch.saturating_sub(1)))
                .ok_or_else(|| format!("launch {launch} has no lifetime"))?,
            None => self.replay,
        };
        let evidence = match &end.outcome {
            Ok(evidence) => evidence.clone(),
            Err(error) => error.evidence(),
        };
        let code = evidence.exit.and_then(|exit| exit.code);
        let stderr = fs::read_to_string(
            self.pure
                .evidence_dir()
                .join(session.id.as_str())
                .join(turn.get().to_string())
                .join("stderr.log"),
        )
        .unwrap_or_default();
        replay_exit(fixture, code, &stderr).map_err(|why| format!("launch {launch}: {why}"))
    }

    /// One settled turn's outcome in the checker's vocabulary.
    fn outcome(end: &TurnEnd, session: &Session, observed: &[Value]) -> TurnOutcome {
        let evidence = match &end.outcome {
            Ok(evidence) => evidence.clone(),
            Err(error) => error.evidence(),
        };
        let cleanup = cleanup_name(evidence.cleanup);
        let (rejected, error) = match &end.outcome {
            Ok(_) => (None, None),
            Err(AdapterError::Rejected { reason, .. }) => (Some(rejected_name(reason)), None),
            Err(error) => (None, error_name(error)),
        };
        let mut outcome = snapshot(observed, &session.plan);
        outcome.rejected = rejected;
        outcome.error = error;
        outcome.terminal = end.terminal.as_ref().map(terminal);
        outcome.usage = end
            .terminal
            .as_ref()
            .and_then(|terminal| terminal.usage.as_ref())
            .map(|sample| {
                let mut usage = usage_sample(sample);
                usage["from"] = json!("terminal");
                usage["scope"] = json!(session.plan.capabilities.usage.tokens);
                usage
            });
        // A server route's terminal carries no usage: Core sums its samples
        // (x.3.2 X3).
        if outcome.usage.is_none() && session.plan.server_key.is_some() {
            outcome.usage = sampled_usage(observed, &session.plan);
        }
        outcome.cleanup = Some(cleanup.to_owned());
        outcome.instance = end.instance.as_ref().map(|instance| {
            json!({"vendor_version": instance.vendor_version,
                "version_status": serde_json::to_value(instance.version_status)
                    .unwrap_or(Value::Null)})
        });
        outcome.exit = evidence
            .exit
            .map(|exit| json!({"code": exit.code, "signal": exit.signal}));
        outcome.journal_uncertain = evidence.journal_uncertain;
        outcome
    }

    /// Whether Host's journal holds positive group-absence evidence for
    /// the turn's own process group: every anchor the turn owns has a
    /// committed absence proof (review r1 #5: never inferred from the
    /// adapter's cleanup claim). A turn with no anchor of its own (nothing
    /// launched, or a server route's turn) has none.
    async fn group_absent(&self, session: &SessionId, turn: TurnNumber) -> Result<bool, String> {
        let mut owned = Vec::new();
        let mut after = None;
        loop {
            let (_, journal) = self.pure.store().runtime_resources().into_wire_parts();
            let page = journal
                .list_anchor_records_page(after.clone(), 256)
                .await
                .map_err(|e| format!("anchor records: {e:?}"))?;
            let full = page.len() == 256;
            after = page.last().map(|record| record.intent.anchor_id.clone());
            owned.extend(
                page.into_iter()
                    .filter(|record| match &record.intent.owner {
                        via_store::ProcessOwner::Turn {
                            session_id,
                            turn: owner_turn,
                        } => session_id == session && owner_turn.get() == turn.get(),
                        // A shared server's anchor is no turn's own group.
                        via_store::ProcessOwner::Server { .. } => false,
                    }),
            );
            if !full {
                break;
            }
        }
        Ok(!owned.is_empty()
            && owned.iter().all(|record| {
                record.absence.as_ref().is_some_and(|proof| {
                    record
                        .identity
                        .as_ref()
                        .is_none_or(|identity| identity.pgid == proof.pgid)
                })
            }))
    }

    /// Each session's health after the case.
    /// Waits until no session's driver is `open`, within [`FIXTURE_WAIT`].
    async fn until_failed(&self) {
        let started = tokio::time::Instant::now();
        while started.elapsed() < FIXTURE_WAIT
            && self
                .sessions
                .values()
                .any(|session| *session.driver.health().borrow() == DriverHealth::Open)
        {
            tokio::time::sleep(POLL).await;
        }
    }

    fn health(&self) -> BTreeMap<String, Value> {
        self.sessions
            .iter()
            .map(|(label, session)| {
                let health = session.driver.health().borrow().clone();
                let value = match health {
                    DriverHealth::Open => json!({"state": "open", "first_cause": null}),
                    DriverHealth::Closed => json!({"state": "closed", "first_cause": null}),
                    DriverHealth::Failed { first_cause } => {
                        json!({"state": "failed", "first_cause": failure_name(&first_cause)})
                    }
                };
                (label.clone(), value)
            })
            .collect()
    }

    /// Closes every server-route session whose case states no close, so
    /// its lease goes and the idle server retires (x.3.2 X3), checking each
    /// report; judges each server launch by the replay's own verdict
    /// ([`Self::judge_servers`]); then ends the sessions' owned work and
    /// the adapter set's.
    async fn shutdown(self) -> Result<(), String> {
        let mut unclean = Vec::new();
        for (label, session) in &self.sessions {
            if session.plan.server_key.is_some()
                && self.expect["sessions"][label]["close"].is_null()
            {
                let failed = matches!(
                    *session.driver.health().borrow(),
                    DriverHealth::Failed { .. }
                );
                let deadline = Deadline::at(tokio::time::Instant::now() + CLOSE_DEADLINE);
                let report = session.driver.close(CloseMode::Graceful, deadline).await;
                // An unstated close of a healthy session must leave nothing
                // uncertain; a case whose close does states it, and a failed
                // session's uncertainty is its health's (x.3.2 X3 fix r1,
                // ruling 21).
                if !failed && report.cleanup != Cleanup::Quiescent {
                    unclean.push(format!(
                        "session {label}'s shutdown close: cleanup {}",
                        cleanup_name(report.cleanup)
                    ));
                }
            }
        }
        let judged = self.judge_servers().await;
        self.cancel.cancel();
        self.tracker.close();
        let _ = tokio::time::timeout(FIXTURE_WAIT, self.tracker.wait()).await;
        let deadline = Deadline::at(tokio::time::Instant::now() + FIXTURE_WAIT);
        self.pure.set.shutdown(deadline, &[]).await;
        judged?;
        match unclean.first() {
            Some(first) => Err(first.clone()),
            None => Ok(()),
        }
    }

    /// Once every server launch ended (its last lease went, so its idle
    /// retirement closed stdin), each is judged by the replay's own verdict
    /// ([`replay_exit`]) from its exit and its `stderr.log`: launch `n`
    /// (the registry's launch ordinal) against lifetime `n` (x.3.2 X3; fix
    /// r1, ruling 21 and minor 24), in every build.
    async fn judge_servers(&self) -> Result<(), String> {
        let launches = usize::try_from(self.pure.launches()?).map_err(|e| e.to_string())?;
        let shared = self
            .sessions
            .values()
            .any(|session| session.plan.server_key.is_some());
        if !shared || launches == 0 {
            return Ok(());
        }
        let started = tokio::time::Instant::now();
        let ended = loop {
            let ended = self.pure.set.ended_servers();
            if ended.len() >= launches {
                break ended;
            }
            if started.elapsed() > FIXTURE_WAIT {
                return Err(format!(
                    "{} of {launches} server launches ended",
                    ended.len()
                ));
            }
            tokio::time::sleep(POLL).await;
        };
        for (server, launch, code) in &ended {
            let index = usize::try_from(launch.saturating_sub(1)).map_err(|e| e.to_string())?;
            let fixture = match self.replay.get("lifetimes").and_then(Value::as_array) {
                Some(lifetimes) => lifetimes
                    .get(index)
                    .ok_or_else(|| format!("launch {launch} has no lifetime"))?,
                None => self.replay,
            };
            let stderr = fs::read_to_string(
                self.pure
                    .evidence_dir()
                    .join("servers")
                    .join(server)
                    .join("stderr.log"),
            )
            .unwrap_or_default();
            replay_exit(fixture, *code, &stderr)
                .map_err(|why| format!("server launch {launch}: {why}"))?;
        }
        Ok(())
    }
}

/// When a turn's reported tools last all ended, from the observations'
/// decode stamps.
#[derive(Default)]
struct Tools {
    open: BTreeSet<String>,
    all_ended: Option<tokio::time::Instant>,
}

impl Tools {
    fn track(&mut self, item: &ObservationItem) {
        let Observation::Progress(marks) = &item.observation else {
            return;
        };
        for (id, _) in &marks.tools_started {
            self.open.insert(id.clone());
        }
        let mut ended = false;
        for id in &marks.tools_ended {
            ended |= self.open.remove(id);
        }
        if ended && self.open.is_empty() {
            self.all_ended = Some(item.at);
        }
    }
}

/// How close two instants must be to count as one moment.
const SETTLE_SLACK: Duration = Duration::from_millis(100);

/// `cleanup_settles` (the checker's module docs): `at_terminal` when the
/// turn settled with its terminal, `when_tools_end` when its last tool
/// ended first, `at_p7_bound` when it settled at `min(ack + tool_grace,
/// wall)` (the acknowledgement is the interrupted terminal), else a name
/// the checker refuses.
fn settles(
    end: &TurnEnd,
    tools: &Tools,
    settled: tokio::time::Instant,
    (wall, tool_grace): (tokio::time::Instant, Duration),
) -> &'static str {
    let near = |at: tokio::time::Instant| {
        settled.saturating_duration_since(at) <= SETTLE_SLACK
            && at.saturating_duration_since(settled) <= SETTLE_SLACK
    };
    let ack = end.terminal.as_ref().map(|terminal| terminal.at);
    if ack.is_some_and(near) {
        return "at_terminal";
    }
    if tools.all_ended.is_some_and(near) {
        return "when_tools_end";
    }
    let bound = ack.map_or(wall, |ack| wall.min(ack + tool_grace));
    if near(bound) {
        "at_p7_bound"
    } else {
        "unclassified"
    }
}

/// One turn's future in the run: its result and the launches after it./// One turn's future in the run: its result and the launches after it.
type Running<'a> = Pin<Box<dyn Future<Output = Result<(Ran, u64), String>> + 'a>>;

/// Polls every future to completion, in place; outputs in input order.
async fn join_all<T>(mut futures: Vec<Pin<Box<dyn Future<Output = T> + '_>>>) -> Vec<T> {
    let mut outputs: Vec<Option<T>> = futures.iter().map(|_| None).collect();
    std::future::poll_fn(|cx| {
        let mut pending = false;
        for (future, output) in futures.iter_mut().zip(outputs.iter_mut()) {
            if output.is_none() {
                match future.as_mut().poll(cx) {
                    Poll::Ready(value) => *output = Some(value),
                    Poll::Pending => pending = true,
                }
            }
        }
        if pending {
            Poll::Pending
        } else {
            Poll::Ready(())
        }
    })
    .await;
    outputs.into_iter().flatten().collect()
}

/// The label of the session a turn runs on; `main` when absent.
fn session_of(turn: &Value) -> &str {
    turn["session"].as_str().unwrap_or("main")
}

/// A typed value, or none when null or absent.
fn optional<T: serde::de::DeserializeOwned>(value: &Value) -> Result<Option<T>, String> {
    if value.is_null() {
        return Ok(None);
    }
    serde_json::from_value(value.clone())
        .map(Some)
        .map_err(|e| e.to_string())
}

/// `id`'s 1-based ordinal among `ids`, added when new.
fn ordinal(ids: &mut Vec<String>, id: &str) -> usize {
    if let Some(at) = ids.iter().position(|seen| seen == id) {
        at + 1
    } else {
        ids.push(id.to_owned());
        ids.len()
    }
}

/// The outcome a running turn shows: its observations, acceptance, final
/// text and warnings so far; cleanup still pending.
fn snapshot(observed: &[Value], plan: &RoutePlan) -> TurnOutcome {
    let pieces: Vec<String> = observed
        .iter()
        .filter(|observation| observation["kind"] == "final_text")
        .filter_map(|observation| observation["text"].as_str().map(str::to_owned))
        .collect();
    let mut warnings: Vec<String> = plan
        .warnings
        .iter()
        .map(|warning| warning.code.to_owned())
        .collect();
    warnings.extend(
        observed
            .iter()
            .filter(|observation| observation["kind"] == "warning")
            .filter_map(|observation| observation["code"].as_str().map(str::to_owned)),
    );
    TurnOutcome {
        accepted: observed
            .iter()
            .any(|observation| observation["kind"] == "turn.accepted"),
        final_text: (!pieces.is_empty()).then_some(pieces),
        cleanup: Some("pending".to_owned()),
        warnings,
        observations: observed.to_vec(),
        ..TurnOutcome::default()
    }
}

/// C2 `RouteFailure`'s stop facts, on a turn with a stop order: Core's
/// acknowledgement (a qualified interrupted terminal, or the route's own),
/// Host's force evidence and the shared-server fact.
fn stop_facts(turn: &Value, end: &TurnEnd) -> Option<Value> {
    turn.get("stop").filter(|stop| !stop.is_null())?;
    let interrupted = end
        .terminal
        .as_ref()
        .is_some_and(|terminal| terminal.status == VendorTerminalStatus::Interrupted);
    Some(match &end.outcome {
        Err(AdapterError::Route(failure)) => json!({
            "acknowledged": failure.acknowledged,
            "forced": failure.forced,
            "shared": failure.shared,
        }),
        Ok(_) | Err(_) => json!({"acknowledged": interrupted, "forced": false, "shared": false}),
    })
}

/// The checker's shape of a vendor terminal.
fn terminal(terminal: &VendorTerminal) -> Value {
    let raw = |raw: Option<&serde_json::value::RawValue>| {
        raw.and_then(|raw| serde_json::from_str::<Value>(raw.get()).ok())
    };
    json!({
        "status": match terminal.status {
            VendorTerminalStatus::Completed => "completed",
            VendorTerminalStatus::Interrupted => "interrupted",
            VendorTerminalStatus::Failed => "failed",
        },
        "stop_reason": stop_reason(terminal.stop_reason),
        "vendor_stop_reason": terminal.vendor_stop_reason,
        "vendor_code": terminal.vendor_code,
        "class_hint": terminal.class_hint.map(class_hint),
        "detail": terminal.detail,
        "structured_output": raw(terminal.structured_output.as_deref()),
        "steps": terminal.steps,
        "cost": match &terminal.cost {
            Some(cost) => json!({"usd": number(cost.usd), "scope": cost.scope,
                "provenance": "reported"}),
            None => json!({"usd": null, "provenance": "unavailable"}),
        },
        "vendor": raw(terminal.vendor.as_deref()),
    })
}

/// A JSON number as an envelope writes it: a whole value as an integer.
fn number(value: f64) -> Value {
    #[expect(
        clippy::cast_possible_truncation,
        reason = "only a whole value inside i64's exact range is converted"
    )]
    let whole = value as i64;
    #[expect(clippy::cast_precision_loss, reason = "compared back to the original")]
    #[expect(clippy::float_cmp, reason = "an exact round trip is the test")]
    let exact = whole as f64 == value && value.abs() < 9.0e15;
    if exact { json!(whole) } else { json!(value) }
}

/// A server route's turn usage when its terminal carries none: the sum
/// of its keyless progress samples (C2 §5 AD6; Codex's samples are
/// keyless), as Core figures it (x.3.2 X3). `None` without a sample.
fn sampled_usage(observed: &[Value], plan: &RoutePlan) -> Option<Value> {
    const FIELDS: [&str; 5] = [
        "input_tokens",
        "cached_input_tokens",
        "output_tokens",
        "reasoning_output_tokens",
        "total_tokens",
    ];
    let samples: Vec<&Value> = observed
        .iter()
        .filter(|observation| observation["kind"] == "progress")
        .map(|observation| &observation["usage"])
        .filter(|usage| !usage.is_null())
        .collect();
    if samples.is_empty() {
        return None;
    }
    let mut usage = json!({"from": "samples", "scope": plan.capabilities.usage.tokens});
    for field in FIELDS {
        let sum = samples
            .iter()
            .map(|sample| sample[field].as_u64())
            .sum::<Option<u64>>();
        usage[field] = json!(sum);
    }
    Some(usage)
}

fn usage_sample(sample: &via_adapters::UsageSample) -> Value {
    json!({
        "input_tokens": sample.input,
        "cached_input_tokens": sample.cached_input,
        "output_tokens": sample.output,
        "reasoning_output_tokens": sample.reasoning_output,
        "total_tokens": sample.total,
    })
}

fn stop_reason(reason: StopReason) -> &'static str {
    match reason {
        StopReason::EndTurn => "end_turn",
        StopReason::MaxSteps => "max_steps",
        StopReason::Budget => "budget",
        StopReason::Refusal => "refusal",
        StopReason::Interrupted => "interrupted",
        StopReason::Error => "error",
        StopReason::Other => "other",
    }
}

fn class_hint(hint: ClassHint) -> &'static str {
    match hint {
        ClassHint::Auth => "auth",
        ClassHint::RateLimit => "rate_limit",
        ClassHint::ContextExceeded => "context_exceeded",
        ClassHint::BudgetExceeded => "budget_exceeded",
        ClassHint::VendorError => "vendor_error",
        ClassHint::Protocol => "protocol",
        ClassHint::ResumeMismatch => "resume_mismatch",
    }
}

fn denial_kind(kind: DenialKind) -> &'static str {
    match kind {
        DenialKind::FileWrite => "file_write",
        DenialKind::Command => "command",
        DenialKind::Network => "network",
        DenialKind::Other => "other",
    }
}

fn delivery_name(delivery: &SteerDelivery) -> String {
    match delivery {
        SteerDelivery::Injected => "injected".to_owned(),
        SteerDelivery::Partial(semantics) => semantics.to_string(),
    }
}

fn steer_error_name(error: &SteerError) -> &'static str {
    match error {
        SteerError::Unsupported => "unsupported_verb",
        SteerError::NoActiveTurn => "no_active_turn",
        SteerError::TurnMismatch => "turn_mismatch",
        SteerError::OverCapacity => "over_capacity",
        SteerError::NotSteerable => "not_steerable",
        SteerError::NotDelivered => "not_delivered",
        SteerError::NotRecorded { .. } => "not_recorded",
    }
}

fn cleanup_name(cleanup: Cleanup) -> &'static str {
    match cleanup {
        Cleanup::Quiescent => "quiescent",
        Cleanup::Uncertain => "uncertain",
        Cleanup::Pending => "pending",
    }
}

/// C2 `StartRejected` as `rejected` states it.
fn rejected_name(reason: &StartRejected) -> String {
    match reason {
        StartRejected::BoundUnsupported(_) => "bound_unsupported".to_owned(),
        StartRejected::InvalidParam { field } => format!("invalid_param:{field}"),
        StartRejected::VendorError(..) => "vendor_error".to_owned(),
        StartRejected::SessionGone => "session_gone".to_owned(),
        StartRejected::Protocol(_) => "protocol".to_owned(),
    }
}

/// C2 `AdapterError` as `error` states it; a stop order's honoured stop
/// is no error (its facts are `stop_facts`).
fn error_name(error: &AdapterError) -> Option<String> {
    let name = match error {
        AdapterError::Route(failure) => {
            if matches!(failure.cause, RouteError::Stopped { .. }) {
                return None;
            }
            route_cause(&failure.cause)
        }
        AdapterError::ResumeMismatch { .. } => "resume_mismatch",
        AdapterError::Rejected { .. } => return None,
        // Not C2 kinds a conforming case states: named so a check fails.
        AdapterError::Unavailable => "adapter_unavailable",
        AdapterError::TaskFailed => "adapter_task_failed",
        AdapterError::Open(_) => "adapter_open_failed",
    };
    Some(name.to_owned())
}

fn route_cause(cause: &RouteError) -> &'static str {
    match cause {
        RouteError::Protocol { .. } => "protocol",
        RouteError::HandshakeRefused { .. } => "handshake_refused",
        RouteError::TransportLost { .. } => "transport_lost",
        RouteError::ProcessExited { .. } => "process_exit",
        RouteError::Overflow { .. } => "overflow",
        RouteError::Store { .. } => "store",
        RouteError::Stopped { .. } => "stopped",
        RouteError::Deadline { .. } => "deadline",
        RouteError::ForceStopped { .. } => "force_stop",
        RouteError::ServerLost { .. } => "server_lost",
        RouteError::InvalidParam { .. } => "invalid_param",
        RouteError::ResumeMismatch { .. } => "resume_mismatch",
    }
}

/// C2 `DriverFailure` as `health.first_cause` states it.
fn failure_name(failure: &DriverFailure) -> &'static str {
    match failure {
        DriverFailure::Route(cause) => route_cause(cause),
        DriverFailure::ObservationOverflow => "overflow",
        DriverFailure::OwnedTask => "owned_task",
        DriverFailure::TurnAbandoned => "turn_abandoned",
        DriverFailure::ServerLost => "server_lost",
        DriverFailure::ResumeMismatch => "resume_mismatch",
        DriverFailure::RetirementUncertain => "retirement_uncertain",
    }
}

impl Pure {
    /// `<case dir>/<name>.<suffix>`: the fake's launch or progress log.
    pub(crate) fn case_file(&self, suffix: &str) -> PathBuf {
        self.case_dir.path().join(format!("{}.{suffix}", self.name))
    }

    /// The Store's evidence folders (runtime §4).
    fn evidence_dir(&self) -> PathBuf {
        self.state.path().join("state").join("evidence")
    }

    /// The Store the adapter set runs on.
    #[expect(
        clippy::used_underscore_binding,
        reason = "the pure half only holds the Store open; the run half commits to it"
    )]
    fn store(&self) -> &via_store::Store {
        &self._store
    }
}

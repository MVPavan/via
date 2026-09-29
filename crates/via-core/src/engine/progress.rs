//! The step tracker and the published progress (Task 4 design §2.4, §3.2).
//!
//! The tracker is per-turn state in the drive's `TurnRecord` and the only
//! place the step rule runs. Each `progress` item it folds yields a
//! [`ProgressDelta`], which the drive publishes to the slot's `Running`
//! entry under the slot state mutex, and at a step boundary the row of the
//! step that ended.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};
use via_adapters::{ProgressMarks, TurnActivity};
use via_store::StepRow;

use crate::api::rfc3339;

/// At most this many open tools are tracked and published per step.
const OPEN_MAX: usize = 64;

/// At most this many usage keys per step; a sample under a further key adds
/// to a keyless sum.
const USAGE_KEYS: usize = 16;

/// Core's wall clock for one drive: a monotonic base and its wall time, so
/// a message's arrival instant maps to Unix milliseconds.
#[derive(Clone, Copy, Debug)]
pub(super) struct Clock {
    base: tokio::time::Instant,
    wall: SystemTime,
}

impl Clock {
    /// A clock whose base is now.
    pub(super) fn now() -> Self {
        Self {
            base: tokio::time::Instant::now(),
            wall: SystemTime::now(),
        }
    }

    /// The monotonic base, shared with the turn's activity clock.
    pub(super) fn base(&self) -> tokio::time::Instant {
        self.base
    }

    /// Wall time at `at`.
    fn wall(&self, at: tokio::time::Instant) -> SystemTime {
        self.wall + at.saturating_duration_since(self.base)
    }

    /// Unix milliseconds at `at`.
    fn unix_ms(&self, at: tokio::time::Instant) -> i64 {
        unix_ms(self.wall(at))
    }
}

fn unix_ms(time: SystemTime) -> i64 {
    time.duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|since| i64::try_from(since.as_millis()).ok())
        .unwrap_or(0)
}

/// The change one folded item makes to the published progress, applied in
/// order: a boundary clears the open set, then names are appended, then
/// entries are removed by their index at removal time.
#[derive(Debug, Default, Eq, PartialEq)]
pub(super) struct ProgressDelta {
    current_step: u32,
    boundary: bool,
    started: Vec<String>,
    ended: Vec<usize>,
    overflow: bool,
    tokens: Option<u64>,
}

/// One folded item's result: the delta to publish and, at a step
/// boundary, the row of the step that ended.
pub(super) struct Folded {
    pub(super) delta: ProgressDelta,
    pub(super) row: Option<StepRow>,
}

/// The step tracker (design §2.4). `carried` holds rows the turn could not
/// commit before its terminal (design §3.2): they ride in it.
#[derive(Clone, Debug)]
pub(super) struct StepTracker {
    clock: Clock,
    current: u32,
    started_ms: i64,
    results_since_output: bool,
    /// Open tool ids, in start order; the published names follow them.
    open: Vec<String>,
    overflow: bool,
    /// This step's usage samples by key.
    usage: Vec<(Option<String>, u64)>,
    /// Samples under keys past the first 16.
    folded: u64,
    /// Tokens of completed steps, once any had a sample.
    tokens: Option<u64>,
    pub(super) carried: Vec<StepRow>,
}

impl Default for StepTracker {
    /// A tracker on a clock based now.
    fn default() -> Self {
        Self::new(Clock::now())
    }
}

impl StepTracker {
    pub(super) fn new(clock: Clock) -> Self {
        Self {
            clock,
            current: 0,
            started_ms: 0,
            results_since_output: false,
            open: Vec::new(),
            overflow: false,
            usage: Vec::new(),
            folded: 0,
            tokens: None,
            carried: Vec::new(),
        }
    }

    /// The drive's clock.
    pub(super) fn clock(&self) -> Clock {
        self.clock
    }

    /// Rule 1: at `turn.accepted` step 1 starts. `None` once accepted.
    pub(super) fn accept(&mut self) -> Option<ProgressDelta> {
        if self.current != 0 {
            return None;
        }
        self.current = 1;
        self.started_ms = self.clock.unix_ms(tokio::time::Instant::now());
        Some(ProgressDelta {
            current_step: 1,
            boundary: true,
            ..ProgressDelta::default()
        })
    }

    /// Rules 2–4 for one message's marks: its `model` mark applies before
    /// its tool starts, then ends; usage folds into the step.
    pub(super) fn fold(&mut self, marks: &ProgressMarks) -> Folded {
        let mut delta = ProgressDelta::default();
        let mut row = None;
        if marks.model && self.results_since_output && self.current >= 1 {
            // A step boundary: the step ends now and the next starts.
            let now = self.clock.unix_ms(marks.at);
            row = Some(StepRow {
                step: self.current,
                started_ms: self.started_ms,
                ended_ms: now,
                tokens: self.step_tokens(),
            });
            self.tokens = match (self.tokens, self.step_tokens()) {
                (total, None) => total,
                (total, Some(step)) => Some(total.unwrap_or(0).saturating_add(step)),
            };
            self.current = self.current.saturating_add(1);
            self.started_ms = now;
            self.results_since_output = false;
            self.open.clear();
            self.overflow = false;
            self.usage.clear();
            self.folded = 0;
            delta.boundary = true;
        }
        for (id, name) in &marks.tools_started {
            if self.open.contains(id) {
                continue;
            }
            if self.open.len() < OPEN_MAX {
                self.open.push(id.clone());
                delta.started.push(name.clone());
            } else {
                self.overflow = true;
            }
        }
        for id in &marks.tools_ended {
            self.results_since_output = true;
            if let Some(index) = self.open.iter().position(|open| open == id) {
                self.open.remove(index);
                delta.ended.push(index);
            }
        }
        if let Some((key, total)) = &marks.usage {
            if let Some(entry) = self.usage.iter_mut().find(|(known, _)| known == key) {
                entry.1 = *total;
            } else if self.usage.len() < USAGE_KEYS {
                self.usage.push((key.clone(), *total));
            } else {
                self.folded = self.folded.saturating_add(*total);
            }
        }
        delta.current_step = self.current;
        delta.overflow = self.overflow;
        delta.tokens = self.tokens;
        Folded { delta, row }
    }

    /// The step's tokens: the sum over its keys, `None` without a sample.
    fn step_tokens(&self) -> Option<u64> {
        if self.usage.is_empty() && self.folded == 0 {
            return None;
        }
        Some(
            self.usage
                .iter()
                .fold(self.folded, |sum, (_, total)| sum.saturating_add(*total)),
        )
    }

    /// Rule 5 and design §3.2: the rows the terminal carries, the carried
    /// ones and then the open step's, ended now.
    pub(super) fn terminal_rows(&self) -> Vec<StepRow> {
        let mut rows = self.carried.clone();
        if self.current >= 1 {
            rows.push(StepRow {
                step: self.current,
                started_ms: self.started_ms,
                ended_ms: unix_ms(SystemTime::now()),
                tokens: self.step_tokens(),
            });
        }
        rows
    }
}

/// The published progress of the running turn (design §2.4): a copy the
/// `status` read takes under the slot state mutex, at most 70 KiB.
#[derive(Clone, Debug)]
pub(super) struct Progress {
    turn: u32,
    current_step: u32,
    running_tools: Vec<String>,
    tools_overflow: bool,
    tokens: Option<u64>,
    clock: Clock,
    activity: TurnActivity,
}

impl Progress {
    /// Step 0 of `turn`, before its acceptance.
    pub(super) fn new(turn: u32, clock: Clock, activity: TurnActivity) -> Self {
        Self {
            turn,
            current_step: 0,
            running_tools: Vec::new(),
            tools_overflow: false,
            tokens: None,
            clock,
            activity,
        }
    }

    /// Step 0 of `turn` on a clock based now.
    #[cfg(any(test, feature = "test-failpoints"))]
    pub(super) fn starting(turn: u32) -> Self {
        let clock = Clock::now();
        Self::new(turn, clock, TurnActivity::new(clock.base()))
    }

    pub(super) fn turn(&self) -> u32 {
        self.turn
    }

    /// Applies one delta, in the tracker's order.
    pub(super) fn apply(&mut self, delta: &ProgressDelta) {
        self.current_step = delta.current_step;
        if delta.boundary {
            self.running_tools.clear();
        }
        self.running_tools.extend(delta.started.iter().cloned());
        for &index in &delta.ended {
            if index < self.running_tools.len() {
                self.running_tools.remove(index);
            }
        }
        self.tools_overflow = delta.overflow;
        self.tokens = delta.tokens;
    }

    /// The C1 §3.7 `progress` object; `tokens` is labelled `scope`.
    pub(super) fn to_value(&self, scope: &str) -> Value {
        let phase = if self.running_tools.is_empty() && !self.tools_overflow {
            "model"
        } else {
            "tools"
        };
        let last = self.clock.base() + Duration::from_millis(self.activity.last_ms());
        json!({
            "turn": self.turn,
            "current_step": self.current_step,
            "phase": phase,
            "running_tools": self.running_tools,
            "tools_overflow": self.tools_overflow,
            "last_activity_at": rfc3339(self.clock.wall(last)),
            "tokens": self.tokens.map(|total| json!({"total": total, "scope": scope})),
        })
    }
}

#[cfg(test)]
mod tests {
    use via_adapters::{ProgressMarks, TurnActivity};

    use super::{Clock, OPEN_MAX, Progress, StepTracker};

    fn marks(model: bool, started: &[&str], ended: &[&str]) -> ProgressMarks {
        ProgressMarks {
            at: tokio::time::Instant::now(),
            model,
            tools_started: started
                .iter()
                .map(|id| ((*id).to_owned(), format!("n{id}")))
                .collect(),
            tools_ended: ended.iter().map(|id| (*id).to_owned()).collect(),
            usage: None,
        }
    }

    fn usage(key: Option<&str>, total: u64) -> ProgressMarks {
        ProgressMarks {
            usage: Some((key.map(str::to_owned), total)),
            ..marks(false, &[], &[])
        }
    }

    /// Folds `items` into a tracker and its published copy; returns the
    /// rows written.
    fn run(
        tracker: &mut StepTracker,
        progress: &mut Progress,
        items: &[ProgressMarks],
    ) -> Vec<u32> {
        let mut rows = Vec::new();
        for item in items {
            let folded = tracker.fold(item);
            progress.apply(&folded.delta);
            rows.extend(folded.row.map(|row| row.step));
        }
        rows
    }

    fn fresh() -> (StepTracker, Progress) {
        let clock = Clock::now();
        let mut tracker = StepTracker::new(clock);
        let mut progress = Progress::new(1, clock, TurnActivity::new(clock.base()));
        progress.apply(&tracker.accept().unwrap());
        (tracker, progress)
    }

    /// Design §2.4 rules 1–4: the count rises exactly when model output
    /// follows tool results, a message's model mark applies before its
    /// tool starts, and the published copy follows the tracker.
    #[tokio::test]
    async fn the_step_rule_counts_output_after_tool_results() {
        let (mut tracker, mut progress) = fresh();
        assert!(tracker.accept().is_none());
        let rows = run(
            &mut tracker,
            &mut progress,
            &[
                marks(true, &[], &[]),
                marks(false, &["a"], &[]),
                marks(false, &[], &["a"]),
                marks(true, &[], &[]),
                marks(true, &[], &[]),
                // A tool request after results: the boundary comes first.
                marks(false, &[], &["untracked"]),
                marks(true, &["b"], &[]),
            ],
        );
        assert_eq!(rows, [1, 2]);
        assert_eq!(progress.current_step, 3);
        assert_eq!(progress.running_tools, ["nb"]);
        assert_eq!(tracker.terminal_rows().last().unwrap().step, 3);
    }

    /// Rule 3: a new id beyond 64 sets `tools_overflow`; a boundary clears
    /// both; ends remove by index in the published copy.
    #[tokio::test]
    async fn the_open_set_holds_64_and_overflows() {
        let (mut tracker, mut progress) = fresh();
        let ids: Vec<String> = (0..70).map(|index| index.to_string()).collect();
        let started: Vec<&str> = ids.iter().map(String::as_str).collect();
        run(&mut tracker, &mut progress, &[marks(true, &started, &[])]);
        assert_eq!(progress.running_tools.len(), OPEN_MAX);
        assert!(progress.tools_overflow);
        run(
            &mut tracker,
            &mut progress,
            &[marks(false, &[], &["3", "0"])],
        );
        assert_eq!(progress.running_tools[..3], ["n1", "n2", "n4"]);
        assert_eq!(tracker.open.len(), progress.running_tools.len());
        run(&mut tracker, &mut progress, &[marks(true, &[], &[])]);
        assert!(progress.running_tools.is_empty() && !progress.tools_overflow);
    }

    /// Design §2.4 tokens: one key supersedes, keys add, a 17th key adds to
    /// the keyless sum, and the total counts completed steps only.
    #[tokio::test]
    async fn usage_supersedes_within_a_key_and_adds_across_keys() {
        let (mut tracker, mut progress) = fresh();
        let mut items = vec![usage(None, 100), usage(None, 120), usage(Some("m1"), 5)];
        items.extend((0..16).map(|key| usage(Some(&format!("k{key}")), 1)));
        items.push(usage(Some("k0"), 2));
        run(&mut tracker, &mut progress, &items);
        assert_eq!(progress.tokens, None);
        // The 16 keys: none (120), m1 (5), k0 (2, superseding 1) and k1–k13
        // (1 each); k14 and k15 fold into the keyless sum (2).
        assert_eq!(tracker.step_tokens(), Some(120 + 5 + 2 + 13 + 2));
        let rows = run(
            &mut tracker,
            &mut progress,
            &[marks(false, &[], &["x"]), marks(true, &[], &[])],
        );
        assert_eq!(rows, [1]);
        assert_eq!(progress.tokens, Some(142));
        assert_eq!(tracker.terminal_rows().last().unwrap().tokens, None);
    }
}

//! The step tracker and the published progress (Task 4 design §2.4, §3.2).
//!
//! The tracker is per-turn state in the drive's `TurnRecord` and the only
//! place the step rule runs. Each `progress` item it folds yields a
//! [`ProgressDelta`], which the drive publishes to the slot's `Running`
//! entry under the slot state mutex, and at a step boundary the row of the
//! step that ended.

use std::collections::HashMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};
use via_adapters::{ProgressMarks, TurnActivity, UsageSample};
use via_store::StepRow;

use crate::api::{Tokens, rfc3339};

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

/// The largest token count Store keeps (a step row's `tokens` is an
/// SQLite integer): a sample or a step or turn sum past it is refused.
const TOKENS_MAX: u64 = i64::MAX.unsigned_abs();

/// A token count past [`TOKENS_MAX`]: the vendor's evidence cannot be
/// represented, and the turn fails `protocol` (review r1).
#[derive(Debug)]
pub(super) struct Unrepresentable;

/// A step's usage samples by key (the vendor's message ID, if any).
type Samples = Vec<(Option<String>, u64)>;

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
    usage: Samples,
    /// Samples under keys past the first 16.
    folded: u64,
    /// Tokens of completed steps, once any had a sample.
    tokens: Option<u64>,
    /// An item was refused as [`Unrepresentable`].
    unrepresentable: bool,
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
            unrepresentable: false,
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

    /// Rules 2–4 for one message's marks, decoded at `at`: its `model`
    /// mark applies before its tool starts, then ends; usage folds into the
    /// step by its sample's total (a sample without one adds nothing to the
    /// step). A sample that would take a count past [`TOKENS_MAX`] refuses
    /// the whole item before any change, so no row is built from it.
    pub(super) fn fold(
        &mut self,
        marks: &ProgressMarks,
        at: tokio::time::Instant,
    ) -> Result<Folded, Unrepresentable> {
        let boundary = marks.model && self.results_since_output && self.current >= 1;
        let sample = marks
            .usage
            .as_ref()
            .and_then(|usage| usage.total.map(|total| (usage.key.clone(), total)));
        let sampled = self.sample(boundary, sample.as_ref()).inspect_err(|_| {
            self.unrepresentable = true;
        })?;
        let mut delta = ProgressDelta::default();
        let mut row = None;
        if boundary {
            // A step boundary: the step ends now and the next starts.
            let now = self.clock.unix_ms(at);
            let step = self.step_tokens();
            row = Some(StepRow {
                step: self.current,
                started_ms: self.started_ms,
                ended_ms: now,
                tokens: step,
            });
            self.tokens = self.completed_after(boundary);
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
        if let Some((usage, folded)) = sampled {
            self.usage = usage;
            self.folded = folded;
        }
        delta.current_step = self.current;
        delta.overflow = self.overflow;
        delta.tokens = self.tokens;
        Ok(Folded { delta, row })
    }

    /// The step's usage once `sample` applies (after the boundary, if
    /// any), checked so the step's sum and the turn's stay within
    /// [`TOKENS_MAX`]; `None` without a sample.
    fn sample(
        &self,
        boundary: bool,
        sample: Option<&(Option<String>, u64)>,
    ) -> Result<Option<(Samples, u64)>, Unrepresentable> {
        let Some((key, total)) = sample else {
            return Ok(None);
        };
        let (mut usage, mut folded) = if boundary {
            (Vec::new(), 0)
        } else {
            (self.usage.clone(), self.folded)
        };
        if let Some(entry) = usage.iter_mut().find(|(known, _)| known == key) {
            entry.1 = *total;
        } else if usage.len() < USAGE_KEYS {
            usage.push((key.clone(), *total));
        } else {
            folded = folded.checked_add(*total).ok_or(Unrepresentable)?;
        }
        let step = usage
            .iter()
            .try_fold(folded, |sum, (_, total)| sum.checked_add(*total))
            .ok_or(Unrepresentable)?;
        let turn = self
            .completed_after(boundary)
            .unwrap_or(0)
            .checked_add(step)
            .ok_or(Unrepresentable)?;
        if turn > TOKENS_MAX {
            return Err(Unrepresentable);
        }
        Ok(Some((usage, folded)))
    }

    /// Tokens of completed steps once a boundary, if `boundary`, ends the
    /// open step. Every accepted fold kept the turn's sum within
    /// [`TOKENS_MAX`], so this cannot overflow.
    fn completed_after(&self, boundary: bool) -> Option<u64> {
        match (self.tokens, boundary.then(|| self.step_tokens()).flatten()) {
            (total, None) => total,
            (total, Some(step)) => Some(total.unwrap_or(0).saturating_add(step)),
        }
    }

    /// The turn's tokens by the step rule: completed steps and the open
    /// one, `None` without a sample.
    #[cfg(test)]
    pub(super) fn turn_tokens(&self) -> Option<u64> {
        self.completed_after(true)
    }

    /// Whether an item was refused as [`Unrepresentable`]: the turn fails
    /// `protocol`.
    pub(super) fn unrepresentable(&self) -> bool {
        self.unrepresentable
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

/// Most usage keys the turn-wide ledger holds (AD6); a sample under a
/// further new key adds as keyless.
const LEDGER_KEYS: usize = 1024;

/// One component's sum over the samples that contributed to it; `None`
/// once any contributing sample lacked it (AD6), or once the sum no
/// longer fits: an overflowing component is unavailable, never a
/// saturated count reported as exact (C1 §5 per-field usage).
#[derive(Clone, Copy, Debug, Default)]
struct Sum {
    value: u64,
    missing: bool,
}

impl Sum {
    fn add(&mut self, component: Option<u64>) {
        match component.and_then(|value| self.value.checked_add(value)) {
            Some(value) => self.value = value,
            None => self.missing = true,
        }
    }

    fn get(self) -> Option<u64> {
        (!self.missing).then_some(self.value)
    }
}

/// Component sums of a set of samples.
#[derive(Clone, Copy, Debug, Default)]
struct Sums([Sum; 5]);

impl Sums {
    fn add(&mut self, sample: &UsageSample) {
        let components = [
            sample.input,
            sample.cached_input,
            sample.output,
            sample.reasoning_output,
            sample.total,
        ];
        for (sum, component) in self.0.iter_mut().zip(components) {
            sum.add(component);
        }
    }

    fn tokens(self) -> Tokens {
        let [input, cached_input, output, reasoning_output, total] = self.0.map(Sum::get);
        Tokens {
            input,
            cached_input,
            output,
            reasoning_output,
            total,
        }
    }
}

/// The turn-wide usage ledger (AD6), apart from step accounting: a keyed
/// sample supersedes the key's earlier one across the whole turn, a
/// keyless one adds. Past 1,024 keys a new key adds as keyless, and the
/// ledger has overflowed: the envelope then reports `vendor_interval`.
#[derive(Clone, Debug, Default)]
pub(super) struct UsageLedger {
    keyed: HashMap<String, UsageSample>,
    keyless: Sums,
    sampled: bool,
    overflow: bool,
}

impl UsageLedger {
    /// Folds one per-call sample.
    pub(super) fn add(&mut self, sample: &UsageSample) {
        self.sampled = true;
        match &sample.key {
            Some(key) if self.keyed.contains_key(key) || self.keyed.len() < LEDGER_KEYS => {
                self.keyed.insert(key.clone(), sample.clone());
            }
            Some(_) => {
                self.overflow = true;
                self.keyless.add(sample);
            }
            None => self.keyless.add(sample),
        }
    }

    /// The turn's figure and whether its interval is unverified: a turn
    /// aggregate supersedes every call sample; `None` without a sample.
    pub(super) fn figure(&self, aggregate: Option<&UsageSample>) -> Option<(Tokens, bool)> {
        if let Some(aggregate) = aggregate {
            let mut sums = Sums::default();
            sums.add(aggregate);
            return Some((sums.tokens(), false));
        }
        if !self.sampled {
            return None;
        }
        let mut sums = self.keyless;
        for sample in self.keyed.values() {
            sums.add(sample);
        }
        Some((sums.tokens(), self.overflow))
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
    use via_adapters::{ProgressMarks, TurnActivity, UsageSample};

    use super::{Clock, OPEN_MAX, Progress, StepTracker, UsageLedger};

    fn marks(model: bool, started: &[&str], ended: &[&str]) -> ProgressMarks {
        ProgressMarks {
            model,
            tools_started: started
                .iter()
                .map(|id| ((*id).to_owned(), format!("n{id}")))
                .collect(),
            tools_ended: ended.iter().map(|id| (*id).to_owned()).collect(),
            usage: None,
        }
    }

    fn sample(key: Option<&str>, total: u64) -> UsageSample {
        UsageSample {
            key: key.map(str::to_owned),
            total: Some(total),
            ..UsageSample::default()
        }
    }

    fn usage(key: Option<&str>, total: u64) -> ProgressMarks {
        ProgressMarks {
            usage: Some(sample(key, total)),
            ..marks(false, &[], &[])
        }
    }

    fn fold(
        tracker: &mut StepTracker,
        item: &ProgressMarks,
    ) -> Result<super::Folded, super::Unrepresentable> {
        tracker.fold(item, tokio::time::Instant::now())
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
            let folded = fold(tracker, item).unwrap();
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
        run(&mut tracker, &mut progress, &[usage(None, 8)]);
        // The envelope's total: completed steps and the open one.
        assert_eq!(tracker.turn_tokens(), Some(150));
    }

    /// Review r1: a sample above `i64::MAX`, or a step or turn sum past it,
    /// is refused before any row is built and leaves the tracker as it was.
    #[tokio::test]
    async fn unrepresentable_tokens_are_refused_before_any_row() {
        let (mut tracker, _) = fresh();
        let max = u64::try_from(i64::MAX).unwrap();
        assert!(fold(&mut tracker, &usage(None, max + 1)).is_err());
        assert!(tracker.unrepresentable());
        assert_eq!(tracker.step_tokens(), None);
        assert!(fold(&mut tracker, &usage(None, max)).is_ok());
        // A second key would take the step's sum past `i64::MAX`.
        assert!(fold(&mut tracker, &usage(Some("m"), 1)).is_err());
        assert_eq!(tracker.step_tokens(), Some(max));
        assert!(fold(&mut tracker, &marks(false, &[], &["x"])).is_ok());
        // Output after results with a sample: step 2's sample would take
        // the turn past `i64::MAX`, so step 1's row is not built.
        let over = ProgressMarks {
            model: true,
            usage: Some(sample(None, 1)),
            ..marks(false, &[], &[])
        };
        assert!(fold(&mut tracker, &over).is_err());
        assert_eq!(tracker.current, 1);
        assert_eq!(tracker.turn_tokens(), Some(max));
        // Without the sample the boundary still folds.
        assert!(
            fold(&mut tracker, &marks(true, &[], &[]))
                .unwrap()
                .row
                .is_some()
        );
        assert_eq!(tracker.turn_tokens(), Some(max));
    }

    fn call(key: Option<&str>, input: u64, cached: Option<u64>, total: u64) -> UsageSample {
        UsageSample {
            key: key.map(str::to_owned),
            input: Some(input),
            cached_input: cached,
            output: Some(1),
            reasoning_output: Some(0),
            total: Some(total),
        }
    }

    /// AD6: key `a` repeated after a step boundary is counted once; a
    /// keyless sample adds.
    #[test]
    fn a_key_repeated_across_steps_counts_once_in_the_ledger() {
        let mut ledger = UsageLedger::default();
        ledger.add(&call(Some("a"), 10, Some(1), 11));
        ledger.add(&call(None, 5, Some(1), 6));
        // A later step reports `a` again: it supersedes, never adds.
        ledger.add(&call(Some("a"), 20, Some(2), 22));
        let (tokens, interval) = ledger.figure(None).unwrap();
        assert_eq!(tokens.input, Some(25));
        assert_eq!(tokens.cached_input, Some(3));
        assert_eq!(tokens.output, Some(2));
        assert_eq!(tokens.total, Some(28));
        assert!(!interval);
    }

    /// AD6: past 1,024 keys a new key adds as keyless, and the interval is
    /// unverified.
    #[test]
    fn key_1025_overflows_the_ledger() {
        let mut ledger = UsageLedger::default();
        for index in 0..1024 {
            ledger.add(&call(Some(&format!("k{index}")), 1, Some(0), 1));
        }
        assert!(!ledger.figure(None).unwrap().1, "1,024 keys fit");
        ledger.add(&call(Some("k1024"), 1, Some(0), 1));
        ledger.add(&call(Some("k1024"), 1, Some(0), 1));
        let (tokens, interval) = ledger.figure(None).unwrap();
        assert!(interval, "the 1,025th key overflowed");
        // The overflowing key's two samples both added.
        assert_eq!(tokens.total, Some(1026));
        // A known key still supersedes after the overflow.
        ledger.add(&call(Some("k0"), 1, Some(0), 5));
        assert_eq!(ledger.figure(None).unwrap().0.total, Some(1030));
    }

    /// AD6: a component is `null` if any contributing sample lacks it.
    #[test]
    fn a_missing_component_is_null() {
        let mut ledger = UsageLedger::default();
        ledger.add(&call(Some("a"), 10, Some(1), 11));
        ledger.add(&call(Some("b"), 10, None, 11));
        let (tokens, _) = ledger.figure(None).unwrap();
        assert_eq!(tokens.cached_input, None);
        assert_eq!(tokens.input, Some(20));
        // A superseded sample no longer contributes.
        ledger.add(&call(Some("b"), 10, Some(4), 11));
        assert_eq!(ledger.figure(None).unwrap().0.cached_input, Some(5));
    }

    /// Sol r1 F10: a component whose sum does not fit `u64` is reported
    /// unavailable (`null`), never saturated as an exact count; the other
    /// components keep their sums.
    #[test]
    fn an_overflowing_component_is_unavailable() {
        let mut ledger = UsageLedger::default();
        ledger.add(&call(Some("a"), u64::MAX - 1, Some(1), 11));
        ledger.add(&call(None, 5, Some(1), 6));
        let (tokens, _) = ledger.figure(None).unwrap();
        assert_eq!(tokens.input, None);
        assert_eq!(tokens.cached_input, Some(2));
        assert_eq!(tokens.total, Some(17));
    }

    /// AD6: a turn aggregate supersedes every call sample; no sample and no
    /// aggregate gives no figure.
    #[test]
    fn a_turn_aggregate_supersedes_the_samples() {
        let mut ledger = UsageLedger::default();
        assert!(ledger.figure(None).is_none());
        for index in 0..1100 {
            ledger.add(&call(Some(&format!("k{index}")), 1, Some(0), 1));
        }
        let aggregate = call(None, 156, None, 300);
        let (tokens, interval) = ledger.figure(Some(&aggregate)).unwrap();
        assert_eq!(tokens.input, Some(156));
        assert_eq!(tokens.cached_input, None);
        assert_eq!(tokens.total, Some(300));
        assert!(!interval, "the aggregate is the turn's own figure");
    }
}

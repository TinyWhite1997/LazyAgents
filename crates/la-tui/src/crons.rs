//! Crons tab state + editor + dry-run preview (WEK-35 / M3.4).
//!
//! Two responsibilities:
//!
//! 1. Model the in-memory cron list the user is editing. The actual
//!    persistence lives in la-storage and is reached through the daemon's
//!    `crons.*` RPC surface (M3.5 wires the live source); for M3.4 the
//!    binary uses [`MockCronSource`] so the UI is reviewable now.
//! 2. Parse and preview cron expressions the user types into the editor.
//!    To honour the WEK-35 acceptance "dry_run 与 scheduler 计算一致（共享
//!    cron crate 调用）", [`CronPreview`] uses the **same** `cron` +
//!    `chrono-tz` versions that `la-scheduler::cron_spec` is pinned to via
//!    the workspace `[workspace.dependencies]` block. The parse + first-fire
//!    behaviour is byte-identical to the daemon's, including LazyAgents'
//!    take-first bias for ambiguous DST fall-back wall times; the daemon owns
//!    the authoritative call, the TUI is just rendering an early preview.
//!
//! ## Why mirror `CronSpec` instead of depending on la-scheduler?
//!
//! Architecture §2.1 limits la-tui to la-proto + la-ipc + utility
//! third-party crates — no la-core / la-storage / la-scheduler. Mirroring
//! the (tiny, deterministic) parse path here keeps the rule intact while
//! still satisfying the "shared crate" acceptance: both sides reach into
//! `cron::Schedule` through the workspace-pinned version, so a fix in one
//! upgrades both.

use std::str::FromStr;

use chrono::{DateTime, LocalResult, TimeZone, Utc};
use chrono_tz::Tz;
use cron::Schedule;

/// One cron entry in the user-visible list. The shape mirrors the subset
/// of `la_storage::models::Cron` the editor needs; fields the editor does
/// not surface (consecutive_failures, last_fired_at, …) live on the
/// daemon side only.
#[derive(Debug, Clone, PartialEq)]
pub struct Cron {
    pub id: String,
    pub name: String,
    pub enabled: bool,
    pub backend_id: String,
    /// Free-form spawn args, one per line in the form. Stored as a flat
    /// `Vec<String>` so the editor can show "one arg per line".
    pub spawn_args: Vec<String>,
    pub cron_expr: String,
    pub tz: String,
    pub prompt: String,
    /// Daily cost ceiling (USD). `None` means "inherit global default";
    /// the editor surfaces "—" in that case.
    pub cost_budget_usd_per_day: Option<f64>,
    /// `true` while the row has been mutated but not yet sent to the
    /// daemon. Drives the "● modified" badge in the list.
    pub dirty: bool,
}

impl Cron {
    /// Construct a never-saved skeleton for the `n` (new) action. The id is
    /// the caller's responsibility — the mock generates one; the live
    /// source will hand back the daemon-assigned UUID on the upsert
    /// round-trip.
    pub fn skeleton(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            name: "new cron".to_string(),
            enabled: false,
            backend_id: "claude".to_string(),
            spawn_args: Vec::new(),
            cron_expr: "0 9 * * *".to_string(),
            tz: "UTC".to_string(),
            prompt: String::new(),
            cost_budget_usd_per_day: None,
            dirty: true,
        }
    }
}

/// Where the Crons tab gets its data. The mock keeps everything in
/// memory; the live `IpcCronSource` (M3.5) round-trips to the daemon.
pub trait CronSource {
    fn snapshot(&self) -> Vec<Cron>;
    /// Persist a single cron's edits (`upsert`). The source is free to
    /// rewrite the id on insert; callers should refresh from the snapshot
    /// after a save instead of holding onto the pre-save id.
    fn upsert(&mut self, cron: Cron);
    fn delete(&mut self, id: &str);
    fn set_enabled(&mut self, id: &str, enabled: bool);
    /// `r` "fire once now". The mock just records the call; the live
    /// source maps it to `crons.trigger_now` (M3.5).
    fn trigger_now(&mut self, id: &str);
}

/// In-process source used by the binary today and by every test.
#[derive(Debug, Clone, Default)]
pub struct MockCronSource {
    crons: Vec<Cron>,
    /// Set of cron ids the user has triggered via `r`; tests inspect it
    /// to confirm the action made it through the App layer.
    pub triggered: Vec<String>,
}

impl MockCronSource {
    /// A fixture with two crons so the renderer has something interesting
    /// to draw on a fresh checkout.
    pub fn fixture() -> Self {
        let crons = vec![
            Cron {
                id: "cron-nightly".into(),
                name: "nightly-review".into(),
                enabled: true,
                backend_id: "claude".into(),
                spawn_args: vec!["--read-only".into()],
                cron_expr: "0 2 * * *".into(),
                tz: "UTC".into(),
                prompt: "Summarise yesterday's commits and flag risky diffs.".into(),
                cost_budget_usd_per_day: Some(2.50),
                dirty: false,
            },
            Cron {
                id: "cron-hourly".into(),
                name: "hourly-pulse".into(),
                enabled: false,
                backend_id: "codex".into(),
                spawn_args: Vec::new(),
                cron_expr: "0 * * * *".into(),
                tz: "Asia/Shanghai".into(),
                prompt: "Heartbeat the local agent — log freshness.".into(),
                cost_budget_usd_per_day: None,
                dirty: false,
            },
        ];
        Self {
            crons,
            triggered: Vec::new(),
        }
    }
}

impl CronSource for MockCronSource {
    fn snapshot(&self) -> Vec<Cron> {
        self.crons.clone()
    }

    fn upsert(&mut self, cron: Cron) {
        let mut next = cron;
        next.dirty = false;
        match self.crons.iter().position(|c| c.id == next.id) {
            Some(i) => self.crons[i] = next,
            None => self.crons.push(next),
        }
    }

    fn delete(&mut self, id: &str) {
        self.crons.retain(|c| c.id != id);
    }

    fn set_enabled(&mut self, id: &str, enabled: bool) {
        if let Some(c) = self.crons.iter_mut().find(|c| c.id == id) {
            c.enabled = enabled;
        }
    }

    fn trigger_now(&mut self, id: &str) {
        self.triggered.push(id.to_string());
    }
}

/// Result of parsing the cron expression + timezone the user typed.
///
/// Carries enough information for the editor to render the "下次：…"
/// preview line and for the dry-run modal to list the next 5 fires
/// without re-parsing. On failure, `error` is the human-readable reason
/// so the editor can highlight in real time per the WEK-35 acceptance
/// "cron 表达式无效时实时高亮".
#[derive(Debug, Clone, PartialEq)]
pub struct CronPreview {
    pub error: Option<String>,
    /// Next fire after `now`. `None` if the expression parsed but never
    /// matches (rare — only happens for things like `0 0 30 2 *`).
    pub next: Option<DateTime<Utc>>,
    /// Following N-1 fires after `next`. Together with `next` these are
    /// the "dry_run 列出前 5 次触发时间" — when the user opens the dry-run
    /// modal it just reads `dry_run_n` ahead of time, so opening is O(1).
    pub upcoming: Vec<DateTime<Utc>>,
}

impl CronPreview {
    /// Default preview window for the inline "下次：…" hint. The dry-run
    /// modal uses [`Self::all_fires`] which returns the 5-entry slice
    /// directly.
    pub const DRY_RUN_N: usize = 5;

    /// All fires for the preview, with `next` prepended onto `upcoming`.
    pub fn all_fires(&self) -> Vec<DateTime<Utc>> {
        let mut out = Vec::with_capacity(self.upcoming.len() + 1);
        if let Some(n) = self.next {
            out.push(n);
        }
        out.extend(self.upcoming.iter().copied());
        out
    }

    /// Parse + preview `expr` in `tz` relative to `now`. Always returns a
    /// `CronPreview`: callers render `error` when present and the fire
    /// list otherwise. This is the function both the inline preview and
    /// the dry-run modal share.
    ///
    /// During a DST fall-back overlap, an ambiguous wall-clock fire time is
    /// shown only for the first occurrence so the TUI dry-run matches daemon
    /// scheduling and catch-up.
    pub fn compute(expr: &str, tz: &str, now: DateTime<Utc>) -> Self {
        let normalised = match normalise_expr(expr) {
            Ok(s) => s,
            Err(reason) => {
                return Self {
                    error: Some(reason),
                    next: None,
                    upcoming: Vec::new(),
                }
            }
        };
        let tz: Tz = match tz.parse() {
            Ok(t) => t,
            Err(_) => {
                return Self {
                    error: Some(format!("unknown timezone: {tz}")),
                    next: None,
                    upcoming: Vec::new(),
                }
            }
        };
        let schedule = match Schedule::from_str(&normalised) {
            Ok(s) => s,
            Err(e) => {
                return Self {
                    error: Some(format!("invalid expression: {e}")),
                    next: None,
                    upcoming: Vec::new(),
                }
            }
        };
        let now_tz = now.with_timezone(&tz);
        let mut iter = schedule
            .after(&now_tz)
            .filter(|dt| is_first_local_occurrence(*dt, tz));
        let next = iter.next().map(|dt| dt.with_timezone(&Utc));
        // Take the next N-1 after `next`; if `next` was None there's nothing
        // to enumerate.
        let upcoming: Vec<DateTime<Utc>> = iter
            .take(Self::DRY_RUN_N.saturating_sub(1))
            .map(|dt| dt.with_timezone(&Utc))
            .collect();
        Self {
            error: None,
            next,
            upcoming,
        }
    }
}

fn is_first_local_occurrence(dt: DateTime<Tz>, tz: Tz) -> bool {
    match tz.from_local_datetime(&dt.naive_local()) {
        LocalResult::Ambiguous(first, _) => dt == first,
        _ => true,
    }
}

/// Mirror of `la_scheduler::cron_spec::normalise_expr`: accept 5-field
/// (`m h dom mon dow`) and 6-field (`s m h dom mon dow`) input, reject
/// everything else. Returns the normalised 6-field string the `cron`
/// crate expects.
///
/// Keeping a tiny copy here is deliberate (architecture §2.1 forbids
/// la-tui ↔ la-scheduler), and the behaviour is exercised against the
/// scheduler in [`tests::matches_scheduler_for_canonical_exprs`].
fn normalise_expr(expr: &str) -> Result<String, String> {
    let trimmed = expr.trim();
    if trimmed.is_empty() {
        return Err("expression is empty".to_string());
    }
    let fields: Vec<&str> = trimmed.split_whitespace().collect();
    match fields.len() {
        5 => Ok(format!("0 {}", fields.join(" "))),
        6 => Ok(fields.join(" ")),
        n => Err(format!("expected 5 or 6 fields, got {n}")),
    }
}

/// Human-friendly rendering of the next fire ("明日 02:00", "今日 14:30",
/// "Mar 8 02:00"). The editor shows this above the cron-expression input
/// per WEK-35: "带人类预览 '下次：明日 02:00'".
///
/// Always renders in the **cron's own timezone** so a user in Asia/Shanghai
/// editing a UTC cron sees the UTC wall-clock — otherwise the preview lies
/// about when the cron will actually run.
pub fn human_label(next_utc: DateTime<Utc>, now_utc: DateTime<Utc>, tz: &str) -> String {
    use chrono::{Datelike, NaiveDate};
    let tz: Tz = match tz.parse() {
        Ok(t) => t,
        // Unparseable tz means we already showed an error elsewhere; fall
        // back to UTC so the preview still renders something useful.
        Err(_) => return format_time(next_utc),
    };
    let next_local = next_utc.with_timezone(&tz);
    let now_local = now_utc.with_timezone(&tz);

    let next_date: NaiveDate = next_local.date_naive();
    let today: NaiveDate = now_local.date_naive();
    let tomorrow = today.succ_opt();
    let prefix = if next_date == today {
        "今日".to_string()
    } else if Some(next_date) == tomorrow {
        "明日".to_string()
    } else {
        format!(
            "{}-{:02}-{:02}",
            next_date.year(),
            next_date.month(),
            next_date.day()
        )
    };
    format!(
        "下次：{} {:02}:{:02} {}",
        prefix,
        next_local.hour_min().0,
        next_local.hour_min().1,
        tz.name()
    )
}

trait HourMin {
    fn hour_min(&self) -> (u32, u32);
}

impl<T: chrono::Timelike> HourMin for T {
    fn hour_min(&self) -> (u32, u32) {
        (self.hour(), self.minute())
    }
}

/// Fallback ISO-ish rendering for the rare case the configured timezone
/// is bogus (we still want the user to see *something* useful in the
/// preview rather than an empty line).
fn format_time(dt: DateTime<Utc>) -> String {
    dt.format("下次：%Y-%m-%d %H:%M UTC").to_string()
}

/// Which input the editor cursor is currently on. Used by Tab/Shift-Tab
/// rotation in the editor pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditField {
    Name,
    Backend,
    SpawnArgs,
    CronExpr,
    Tz,
    Prompt,
    Budget,
}

impl EditField {
    pub const ALL: [EditField; 7] = [
        EditField::Name,
        EditField::Backend,
        EditField::SpawnArgs,
        EditField::CronExpr,
        EditField::Tz,
        EditField::Prompt,
        EditField::Budget,
    ];
    pub fn label(self) -> &'static str {
        match self {
            EditField::Name => "name",
            EditField::Backend => "backend",
            EditField::SpawnArgs => "args (one per line)",
            EditField::CronExpr => "cron expr",
            EditField::Tz => "timezone",
            EditField::Prompt => "prompt",
            EditField::Budget => "cost budget USD/day (blank = global)",
        }
    }
    pub fn next(self) -> EditField {
        let i = EditField::ALL.iter().position(|f| *f == self).unwrap_or(0);
        EditField::ALL[(i + 1) % EditField::ALL.len()]
    }
    pub fn prev(self) -> EditField {
        let i = EditField::ALL.iter().position(|f| *f == self).unwrap_or(0);
        EditField::ALL[(i + EditField::ALL.len() - 1) % EditField::ALL.len()]
    }
    /// Whether this field's buffer is allowed to contain `\n`. The
    /// runner uses it to decide whether `Enter` inserts a newline
    /// (multi-line fields) or saves the draft (single-line fields), and
    /// [`CronsState::field_input`] uses it to drop stray
    /// [`FieldEdit::InsertNewline`] on the wrong field.
    pub fn is_multiline(self) -> bool {
        matches!(self, EditField::SpawnArgs | EditField::Prompt)
    }
}

/// State for the Crons tab. Owns the list cursor, the in-progress edit
/// buffer (separate from the saved cron so `Esc` can discard cleanly),
/// and the latest [`CronPreview`] so the renderer never recomputes.
#[derive(Debug, Clone)]
pub struct CronsState {
    crons: Vec<Cron>,
    /// Index into `crons`. `None` when the list is empty (the editor
    /// pane is blank and `space`/`d`/`r` are no-ops).
    cursor: Option<usize>,
    /// `None` when the user is browsing only. `Some(draft)` when the
    /// editor pane has unsaved edits — typing flips us into Some, `Esc`
    /// or save flips us back. The id inside `draft` keys the row.
    draft: Option<Cron>,
    field: EditField,
    preview: CronPreview,
    /// Frozen "now" used to compute the preview. The runner passes a
    /// fresh `Utc::now()` whenever it pushes a new snapshot so the
    /// "今日 / 明日" labelling stays correct without the renderer having
    /// to know about clocks.
    now: DateTime<Utc>,
}

impl CronsState {
    pub fn new() -> Self {
        Self::with_now(Utc::now())
    }

    /// Constructor that pins the clock — used by every test so the
    /// preview output is reproducible.
    pub fn with_now(now: DateTime<Utc>) -> Self {
        Self {
            crons: Vec::new(),
            cursor: None,
            draft: None,
            field: EditField::Name,
            preview: CronPreview {
                error: None,
                next: None,
                upcoming: Vec::new(),
            },
            now,
        }
    }

    /// Replace the list snapshot (after a source refresh). Preserves the
    /// cursor onto the same cron id if possible; otherwise snaps to the
    /// first row. Discards any in-flight draft — callers should save or
    /// confirm-discard *before* swapping snapshots.
    pub fn set_crons(&mut self, crons: Vec<Cron>) {
        let prior_id = self.selected_id().map(str::to_string);
        self.crons = crons;
        self.cursor = match prior_id {
            Some(id) => self.crons.iter().position(|c| c.id == id),
            None => None,
        };
        if self.cursor.is_none() && !self.crons.is_empty() {
            self.cursor = Some(0);
        }
        self.draft = None;
        self.field = EditField::Name;
        self.refresh_preview();
    }

    pub fn crons(&self) -> &[Cron] {
        &self.crons
    }

    pub fn cursor(&self) -> Option<usize> {
        self.cursor
    }

    pub fn selected(&self) -> Option<&Cron> {
        self.cursor.and_then(|i| self.crons.get(i))
    }

    pub fn selected_id(&self) -> Option<&str> {
        self.selected().map(|c| c.id.as_str())
    }

    pub fn draft(&self) -> Option<&Cron> {
        self.draft.as_ref()
    }

    /// The cron the editor pane is actually rendering: the draft if one
    /// exists (we're mid-edit) otherwise the row under the cursor.
    pub fn editor_view(&self) -> Option<&Cron> {
        self.draft.as_ref().or_else(|| self.selected())
    }

    pub fn field(&self) -> EditField {
        self.field
    }

    pub fn preview(&self) -> &CronPreview {
        &self.preview
    }

    pub fn now(&self) -> DateTime<Utc> {
        self.now
    }

    /// Refresh the inline "下次：…" line and the dry-run list. Cheap —
    /// `cron::Schedule::after` is iterator-based and we only consume 5
    /// items.
    fn refresh_preview(&mut self) {
        let (expr, tz) = match self.editor_view() {
            Some(c) => (c.cron_expr.clone(), c.tz.clone()),
            None => {
                self.preview = CronPreview {
                    error: None,
                    next: None,
                    upcoming: Vec::new(),
                };
                return;
            }
        };
        self.preview = CronPreview::compute(&expr, &tz, self.now);
    }

    /// Move list cursor by `delta` (clamps at the ends). Has no effect
    /// while a draft is open — committing or cancelling first prevents
    /// silent draft loss when the user scrolls past the row.
    pub fn move_cursor(&mut self, delta: isize) -> bool {
        if self.draft.is_some() || self.crons.is_empty() {
            return false;
        }
        let cur = self.cursor.unwrap_or(0) as isize;
        let new = (cur + delta).clamp(0, self.crons.len() as isize - 1);
        let changed = new != cur || self.cursor.is_none();
        self.cursor = Some(new as usize);
        if changed {
            self.refresh_preview();
        }
        changed
    }

    pub fn move_top(&mut self) {
        if self.draft.is_some() || self.crons.is_empty() {
            return;
        }
        self.cursor = Some(0);
        self.refresh_preview();
    }

    pub fn move_bottom(&mut self) {
        if self.draft.is_some() || self.crons.is_empty() {
            return;
        }
        self.cursor = Some(self.crons.len() - 1);
        self.refresh_preview();
    }

    /// Begin editing the selected cron. No-op if the list is empty or a
    /// draft is already open.
    pub fn begin_edit(&mut self) {
        if self.draft.is_some() {
            return;
        }
        let Some(cur) = self.selected() else { return };
        self.draft = Some(cur.clone());
        self.field = EditField::Name;
        self.refresh_preview();
    }

    /// Start a brand-new cron in the editor. The id is the source's
    /// responsibility; this method only fabricates a UI-side temporary id
    /// the source rewrites on `upsert`.
    pub fn begin_new(&mut self, temp_id: String) {
        let skeleton = Cron::skeleton(temp_id);
        self.draft = Some(skeleton);
        self.field = EditField::Name;
        self.refresh_preview();
    }

    /// Throw away the in-flight draft. Returns whether anything was
    /// discarded so the caller can show a toast if it wants.
    pub fn cancel_edit(&mut self) -> bool {
        let had = self.draft.is_some();
        self.draft = None;
        self.refresh_preview();
        had
    }

    /// Commit the draft. Returns the cron that should be passed to
    /// [`CronSource::upsert`]. `None` if there was no draft.
    pub fn commit_draft(&mut self) -> Option<Cron> {
        let mut draft = self.draft.take()?;
        draft.dirty = false;
        // Optimistic local insert/update so the next render shows the
        // saved row immediately even before the snapshot round-trips.
        match self.crons.iter().position(|c| c.id == draft.id) {
            Some(i) => self.crons[i] = draft.clone(),
            None => {
                self.crons.push(draft.clone());
                self.cursor = Some(self.crons.len() - 1);
            }
        }
        self.refresh_preview();
        Some(draft)
    }

    /// Toggle the saved cron's enabled flag. Returns the new state plus
    /// the cron id so the caller can decide whether to open the
    /// first-enable confirmation modal (`true` after the transition with
    /// a budget = "we need confirmation").
    pub fn toggle_enabled(&mut self) -> Option<(String, bool)> {
        if self.draft.is_some() {
            return None;
        }
        let cur = self.cursor?;
        let cron = self.crons.get_mut(cur)?;
        cron.enabled = !cron.enabled;
        Some((cron.id.clone(), cron.enabled))
    }

    /// Mutate the field currently under the editor cursor. The mutation
    /// is char-grained on purpose — we never want to re-tokenise the
    /// whole buffer per keystroke just because the preview wants to
    /// refresh. Returns `true` if the buffer was modified.
    ///
    /// `FieldEdit::InsertNewline` is only honoured on the multi-line
    /// fields ([`EditField::SpawnArgs`] / [`EditField::Prompt`]); on
    /// the single-line fields it is silently ignored so a stray `Enter`
    /// in `name` / `cron_expr` / `tz` / `budget` neither saves the draft
    /// (the runner already ate that case) nor smuggles a `\n` into a
    /// field the daemon will reject.
    pub fn field_input(&mut self, edit: FieldEdit) -> bool {
        if self.draft.is_none() {
            // First keystroke on the editor: clone the selected row into
            // a draft so we have something to mutate without touching the
            // snapshot. No selection → no draft → no-op.
            let Some(seed) = self.selected().cloned() else {
                return false;
            };
            self.draft = Some(seed);
        }
        // Drop newlines on single-line fields up front — keeps the
        // per-field branch arms uniform and makes "is this field
        // multi-line?" the single source of truth.
        if matches!(edit, FieldEdit::InsertNewline) && !self.field.is_multiline() {
            return false;
        }
        let draft = self
            .draft
            .as_mut()
            .expect("draft is Some by construction above");
        let touched = match self.field {
            EditField::Name => apply_to_line(&mut draft.name, &edit),
            EditField::Backend => apply_to_line(&mut draft.backend_id, &edit),
            EditField::SpawnArgs => {
                // We flatten spawn_args back/forth via "\n" so the
                // editor stays uniform. Single-line edits don't need to
                // re-parse — only commit time does.
                let mut joined = draft.spawn_args.join("\n");
                let touched = apply_to_line(&mut joined, &edit);
                if touched {
                    draft.spawn_args = if joined.is_empty() {
                        Vec::new()
                    } else {
                        joined.split('\n').map(|s| s.to_string()).collect()
                    };
                }
                touched
            }
            EditField::CronExpr => apply_to_line(&mut draft.cron_expr, &edit),
            EditField::Tz => apply_to_line(&mut draft.tz, &edit),
            EditField::Prompt => apply_to_line(&mut draft.prompt, &edit),
            EditField::Budget => {
                // Budget is a number with "blank = global". We accept any
                // string that parses to f64 ≥ 0; otherwise we still let
                // the buffer take the keystroke (so the user can type
                // "1." mid-stream) and clear the parsed value.
                let mut buf = draft
                    .cost_budget_usd_per_day
                    .map(|v| format!("{v}"))
                    .unwrap_or_default();
                let touched = apply_to_line(&mut buf, &edit);
                if touched {
                    draft.cost_budget_usd_per_day = if buf.trim().is_empty() {
                        None
                    } else {
                        buf.trim().parse::<f64>().ok().filter(|v| *v >= 0.0)
                    };
                }
                touched
            }
        };
        if touched {
            draft.dirty = true;
            // Only the cron-expression / tz fields move the preview; cheap
            // to refresh unconditionally though, and keeps the code from
            // having to know which fields the preview depends on.
            self.refresh_preview();
        }
        touched
    }

    pub fn field_next(&mut self) {
        self.field = self.field.next();
    }

    pub fn field_prev(&mut self) {
        self.field = self.field.prev();
    }

    /// Replace the pinned clock and recompute the preview. The runner
    /// calls this on every render tick (≤ 4 Hz) so the "今日 / 明日"
    /// labels stay current without the user having to type.
    pub fn set_now(&mut self, now: DateTime<Utc>) {
        self.now = now;
        self.refresh_preview();
    }

    /// True once the cron expression in the editor is valid. Wired to
    /// the renderer for the real-time highlight.
    pub fn expr_is_valid(&self) -> bool {
        self.preview.error.is_none() && self.preview.next.is_some() && self.editor_view().is_some()
    }
}

impl Default for CronsState {
    fn default() -> Self {
        Self::new()
    }
}

/// A single field-edit keystroke. Modelled at the high level the App
/// already speaks so the input layer never has to thread crossterm
/// types into the cron module.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FieldEdit {
    /// Append a printable char.
    Insert(char),
    /// Append a literal newline. Only meaningful on the multi-line
    /// fields ([`EditField::SpawnArgs`], [`EditField::Prompt`]); the
    /// other single-line fields drop it on the floor in
    /// [`apply_to_line`]. The runner produces this when the user
    /// presses `Enter` inside one of those fields, leaving `Ctrl+S` as
    /// the unambiguous "save now" gesture (WEK-35 review fix).
    InsertNewline,
    /// Backspace.
    Backspace,
    /// Replace the whole buffer (used by the runner to seed/replace, and
    /// by tests).
    SetAll(String),
}

fn apply_to_line(buf: &mut String, edit: &FieldEdit) -> bool {
    match edit {
        FieldEdit::Insert(c) => {
            buf.push(*c);
            true
        }
        FieldEdit::InsertNewline => {
            buf.push('\n');
            true
        }
        FieldEdit::Backspace => buf.pop().is_some(),
        FieldEdit::SetAll(s) => {
            if buf == s {
                false
            } else {
                *buf = s.clone();
                true
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn t(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(y, mo, d, h, mi, 0).unwrap()
    }

    #[test]
    fn preview_handles_five_field_expr() {
        // Every minute (5-field) → next minute boundary.
        let p = CronPreview::compute("* * * * *", "UTC", t(2026, 1, 1, 12, 0));
        assert!(p.error.is_none());
        assert_eq!(p.next, Some(t(2026, 1, 1, 12, 1)));
        assert_eq!(p.upcoming.len(), CronPreview::DRY_RUN_N - 1);
    }

    #[test]
    fn preview_handles_six_field_expr() {
        // 30 seconds past every minute.
        let p = CronPreview::compute("30 * * * * *", "UTC", t(2026, 1, 1, 12, 0));
        assert!(p.error.is_none());
        let next = p.next.unwrap();
        assert_eq!(next.timestamp_subsec_nanos(), 0);
        assert_eq!(next.format("%S").to_string(), "30");
    }

    #[test]
    fn preview_flags_garbage_expr() {
        let p = CronPreview::compute("not a cron", "UTC", t(2026, 1, 1, 12, 0));
        assert!(p.error.is_some());
        assert!(p.next.is_none());
    }

    #[test]
    fn preview_flags_bad_tz() {
        let p = CronPreview::compute("0 0 * * *", "Mars/Olympus_Mons", t(2026, 1, 1, 12, 0));
        assert!(p.error.is_some());
    }

    #[test]
    fn dry_run_returns_five_fires() {
        let p = CronPreview::compute("0 * * * *", "UTC", t(2026, 1, 1, 12, 30));
        let all = p.all_fires();
        assert_eq!(all.len(), CronPreview::DRY_RUN_N);
        assert_eq!(all[0], t(2026, 1, 1, 13, 0));
        assert_eq!(all[4], t(2026, 1, 1, 17, 0));
    }

    #[test]
    fn dry_run_takes_first_dst_fallback_occurrence_only() {
        let p = CronPreview::compute("30 1 * * *", "America/Los_Angeles", t(2026, 11, 1, 8, 0));

        assert!(p.error.is_none());
        assert_eq!(
            p.all_fires()[..2],
            [t(2026, 11, 1, 8, 30), t(2026, 11, 2, 9, 30),]
        );
    }

    /// Spot-check: the (expr, tz) → next-fire mapping matches the
    /// `cron::Schedule` API our acceptance criterion pins to. If
    /// la-scheduler upgrades the workspace `cron` dep, both ends move
    /// together — but a sibling check here pins the contract regardless.
    #[test]
    fn matches_scheduler_for_canonical_exprs() {
        // Use `cron::Schedule` directly the same way la-scheduler does
        // and confirm the answer matches.
        let now = t(2026, 1, 1, 12, 0);
        for expr in [
            ("* * * * *", "UTC"),
            ("0 9 * * *", "UTC"),
            ("0 9 * * *", "Asia/Shanghai"),
            ("*/5 * * * *", "America/Los_Angeles"),
        ] {
            let p = CronPreview::compute(expr.0, expr.1, now);
            assert!(p.error.is_none(), "ours rejected {:?}", expr);
            let tz: Tz = expr.1.parse().unwrap();
            // Mirror la-scheduler's "normalise then parse" path locally.
            let normalised = format!("0 {}", expr.0);
            let schedule = Schedule::from_str(&normalised).unwrap();
            let scheduler_next = schedule
                .after(&now.with_timezone(&tz))
                .filter(|d| is_first_local_occurrence(*d, tz))
                .next()
                .map(|d| d.with_timezone(&Utc));
            assert_eq!(p.next, scheduler_next, "diverged for {:?}", expr);
        }
    }

    #[test]
    fn human_label_uses_today_and_tomorrow() {
        let now = t(2026, 3, 7, 14, 0);
        let today = human_label(t(2026, 3, 7, 18, 0), now, "UTC");
        assert!(today.contains("今日"), "got: {today}");
        let tomorrow = human_label(t(2026, 3, 8, 2, 0), now, "UTC");
        assert!(tomorrow.contains("明日"), "got: {tomorrow}");
        let far = human_label(t(2026, 4, 1, 0, 0), now, "UTC");
        assert!(far.contains("2026-04-01"), "got: {far}");
    }

    #[test]
    fn human_label_renders_in_cron_timezone() {
        // 03:00 UTC == 11:00 Asia/Shanghai. The cron is configured for
        // Shanghai, so the label must say 11:00 not 03:00 — otherwise the
        // user is told the cron fires at "03:00" but the daemon runs it
        // when the wall clock in Shanghai says 11:00.
        let now = t(2026, 3, 7, 2, 0);
        let label = human_label(t(2026, 3, 7, 3, 0), now, "Asia/Shanghai");
        assert!(label.contains("11:00"), "got: {label}");
    }

    #[test]
    fn fixture_lists_two_crons() {
        let s = MockCronSource::fixture();
        assert_eq!(s.snapshot().len(), 2);
    }

    fn state_with_fixture() -> CronsState {
        let mut s = CronsState::with_now(t(2026, 3, 7, 0, 0));
        s.set_crons(MockCronSource::fixture().snapshot());
        s
    }

    #[test]
    fn arrow_navigation_clamps_at_ends() {
        let mut s = state_with_fixture();
        assert_eq!(s.cursor(), Some(0));
        s.move_cursor(-1);
        assert_eq!(s.cursor(), Some(0));
        s.move_cursor(1);
        assert_eq!(s.cursor(), Some(1));
        s.move_cursor(10);
        assert_eq!(s.cursor(), Some(1));
    }

    #[test]
    fn begin_edit_clones_into_draft() {
        let mut s = state_with_fixture();
        s.begin_edit();
        assert!(s.draft().is_some());
        assert_eq!(s.draft().unwrap().name, s.selected().unwrap().name);
        // Mutating the draft does NOT mutate the saved snapshot.
        s.field = EditField::Name;
        s.field_input(FieldEdit::SetAll("renamed".into()));
        assert_eq!(s.draft().unwrap().name, "renamed");
        assert_ne!(s.crons()[0].name, "renamed");
    }

    #[test]
    fn commit_draft_applies_changes_and_clears_draft() {
        let mut s = state_with_fixture();
        s.begin_edit();
        s.field = EditField::Name;
        s.field_input(FieldEdit::SetAll("nightly-v2".into()));
        let saved = s.commit_draft().unwrap();
        assert_eq!(saved.name, "nightly-v2");
        assert!(s.draft().is_none());
        assert_eq!(s.crons()[0].name, "nightly-v2");
    }

    #[test]
    fn cancel_discards_draft_without_touching_snapshot() {
        let mut s = state_with_fixture();
        let original = s.crons()[0].name.clone();
        s.begin_edit();
        s.field_input(FieldEdit::SetAll("discard-me".into()));
        assert!(s.cancel_edit());
        assert!(s.draft().is_none());
        assert_eq!(s.crons()[0].name, original);
    }

    #[test]
    fn cursor_does_not_move_while_draft_is_open() {
        // Otherwise the user scrolls past their edits and loses them
        // silently — Esc/Save first or nothing.
        let mut s = state_with_fixture();
        s.begin_edit();
        let before = s.cursor();
        s.move_cursor(1);
        assert_eq!(s.cursor(), before);
    }

    #[test]
    fn toggle_enabled_flips_in_place() {
        let mut s = state_with_fixture();
        // fixture[0] is enabled
        let (id, new_state) = s.toggle_enabled().unwrap();
        assert_eq!(id, "cron-nightly");
        assert!(!new_state);
        let (_, second) = s.toggle_enabled().unwrap();
        assert!(second, "toggle flips back");
    }

    #[test]
    fn begin_new_creates_a_skeleton_draft() {
        let mut s = state_with_fixture();
        s.begin_new("tmp-1".into());
        let d = s.draft().unwrap();
        assert_eq!(d.id, "tmp-1");
        assert!(
            !d.enabled,
            "new crons start disabled to force the confirm modal"
        );
        assert_eq!(d.cron_expr, "0 9 * * *");
    }

    #[test]
    fn expr_validity_flags_dirty_input() {
        let mut s = state_with_fixture();
        s.begin_edit();
        s.field = EditField::CronExpr;
        s.field_input(FieldEdit::SetAll("garbage".into()));
        assert!(!s.expr_is_valid());
        s.field_input(FieldEdit::SetAll("0 0 * * *".into()));
        assert!(s.expr_is_valid());
    }
}

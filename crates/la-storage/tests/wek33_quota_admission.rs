//! Integration test for WEK-33 / M3.2 acceptance:
//!
//! > 集成：把 `max_runs_per_day=2` 配出来，第三次触发被拒并写 audit
//!
//! Exercises the full admission pipeline end-to-end against a real SQLite
//! database:
//!   1. Seed a cron with `max_runs_per_day=2` and the rest at default.
//!   2. Drive three fires through `evaluate_admission` + the new
//!      `RunsRepo::create` / `RunsRepo::create_rejected` paths.
//!   3. Assert the first two created normal `running` rows, the third was
//!      refused with `runs.status='cancelled'`,
//!      `error_kind='quota_max_runs_per_day'`, and the in-memory decision
//!      matches.
//!
//! Also covers the auto-pause path (`pause_on_consecutive_failures`):
//! bump the counter to threshold, then `pause_for_failures` flips
//! `enabled=0` atomically, and the subsequent admission returns
//! `RefusePaused` with `runs.status='cancelled'`,
//! `error_kind='quota_paused'`.

use la_scheduler::quota::{
    evaluate_admission, AdmissionDecision, CronQuota, GlobalQuota, QuotaSnapshot,
};
use la_storage::{
    BackendUpsert, CronUpsert, NewProject, NewRejectedRun, NewRun, RunFinish, RunsListFilter,
    Storage, StorageConfig,
};
use tempfile::TempDir;

async fn open_storage() -> (TempDir, Storage) {
    let dir = TempDir::new().expect("tempdir");
    let storage = Storage::open(StorageConfig::for_test(dir.path()))
        .await
        .expect("open storage");
    (dir, storage)
}

async fn seed_project_and_cron(
    storage: &Storage,
    cron_id: &str,
    max_runs_per_day: i64,
    pause_on_consecutive_failures: i64,
) -> String {
    storage
        .backends()
        .upsert(BackendUpsert {
            id: "claude",
            display_name: "Claude Code",
            version: None,
            available: true,
        })
        .await
        .unwrap();
    let project_id = la_storage::new_id();
    storage
        .projects()
        .create(NewProject {
            id: project_id.clone(),
            root_path: "/tmp/lazyagents/wek33".into(),
            display_name: "wek33".into(),
            vcs: Some("git".into()),
        })
        .await
        .unwrap();
    storage
        .crons()
        .upsert(CronUpsert {
            id: cron_id.to_string(),
            name: "wek33-cron".into(),
            enabled: true,
            project_id: project_id.clone(),
            backend_id: "claude".into(),
            spawn_args: serde_json::json!({"cwd":"/tmp/lazyagents/wek33"}),
            prompt: "wek33".into(),
            cron_expr: "* * * * *".into(),
            tz: "UTC".into(),
            catchup_mode: "coalesce".into(),
            max_concurrent_runs: 8, // out of scope for this test
            max_runs_per_day,
            max_runtime_s: 1800,
            cost_budget_usd_per_day: None,
            failure_backoff: "expo(1m,2,1h)".into(),
            pause_on_consecutive_failures,
            consecutive_failures: 0,
            last_fired_at: None,
            next_fire_at: None,
        })
        .await
        .unwrap();
    project_id
}

fn quota_from(cron: &la_storage::Cron) -> CronQuota {
    CronQuota {
        max_concurrent_runs: u32::try_from(cron.max_concurrent_runs).unwrap(),
        max_runs_per_day: u32::try_from(cron.max_runs_per_day).unwrap(),
        max_runtime_s: u32::try_from(cron.max_runtime_s).unwrap(),
        cost_budget_usd_per_day: cron.cost_budget_usd_per_day,
        pause_on_consecutive_failures: u32::try_from(cron.pause_on_consecutive_failures).unwrap(),
        consecutive_failures: u32::try_from(cron.consecutive_failures).unwrap(),
        enabled: cron.enabled != 0,
    }
}

async fn snapshot_for(storage: &Storage, cron_id: &str, window_start: &str) -> QuotaSnapshot {
    let running_for_cron = storage
        .runs()
        .count_running_for_cron(cron_id)
        .await
        .unwrap();
    let running_global = storage.runs().count_running_global().await.unwrap();
    let window_runs_today = storage
        .runs()
        .count_since_for_cron(cron_id, window_start)
        .await
        .unwrap();
    let window_cost_today = storage
        .runs()
        .sum_cost_since_for_cron(cron_id, window_start)
        .await
        .unwrap();
    QuotaSnapshot {
        running_for_cron: u32::try_from(running_for_cron).unwrap(),
        running_global: u32::try_from(running_global).unwrap(),
        window_runs_today: u32::try_from(window_runs_today).unwrap(),
        window_cost_today,
        current_loadavg_1m: None,
    }
}

#[tokio::test]
async fn third_trigger_is_refused_and_writes_audit_when_max_runs_per_day_is_two() {
    let (_dir, storage) = open_storage().await;
    let cron_id = "cron-wek33";
    seed_project_and_cron(&storage, cron_id, 2, 5).await;
    let global = GlobalQuota {
        // Disable both global rails — we are only testing per-cron caps here.
        global_max_concurrent_runs: 0,
        cpu_load_throttle: None,
    };
    // Use a fixed window-start so the test is deterministic; every run we
    // create below has scheduled_at >= "2000-01-01 00:00:00".
    let window_start = "2000-01-01 00:00:00";

    // ---- Fire 1: admitted, recorded as running, then finished completed. ----
    let cron = storage.crons().get(cron_id).await.unwrap().unwrap();
    let quota = quota_from(&cron);
    let snap = snapshot_for(&storage, cron_id, window_start).await;
    let decision = evaluate_admission(&quota, &global, &snap);
    assert_eq!(
        decision,
        AdmissionDecision::Admit,
        "fire 1 should be admitted"
    );
    storage
        .runs()
        .create(NewRun {
            id: "run-1".into(),
            cron_id: Some(cron_id.into()),
            session_id: None,
            scheduled_at: "2000-01-01 03:17:00".into(),
            started_at: Some("2000-01-01 03:17:00".into()),
            status: "running".into(),
            coalesced_count: 1,
        })
        .await
        .unwrap();
    storage
        .runs()
        .finish(
            "run-1",
            RunFinish {
                finished_at: "2000-01-01 03:18:00".into(),
                status: "completed".into(),
                exit_code: Some(0),
                cost_usd_est: None,
                error_kind: None,
                error_detail: None,
                tail_log: None,
            },
        )
        .await
        .unwrap();

    // ---- Fire 2: admitted (still 1 row in window), finished completed. ----
    let snap = snapshot_for(&storage, cron_id, window_start).await;
    assert_eq!(snap.window_runs_today, 1, "after fire 1, 1 row in window");
    let decision = evaluate_admission(&quota, &global, &snap);
    assert_eq!(
        decision,
        AdmissionDecision::Admit,
        "fire 2 should be admitted"
    );
    storage
        .runs()
        .create(NewRun {
            id: "run-2".into(),
            cron_id: Some(cron_id.into()),
            session_id: None,
            scheduled_at: "2000-01-01 04:17:00".into(),
            started_at: Some("2000-01-01 04:17:00".into()),
            status: "running".into(),
            coalesced_count: 1,
        })
        .await
        .unwrap();
    storage
        .runs()
        .finish(
            "run-2",
            RunFinish {
                finished_at: "2000-01-01 04:18:00".into(),
                status: "completed".into(),
                exit_code: Some(0),
                cost_usd_est: None,
                error_kind: None,
                error_detail: None,
                tail_log: None,
            },
        )
        .await
        .unwrap();

    // ---- Fire 3: REFUSED — 2 rows in window, cap is 2. ----
    let snap = snapshot_for(&storage, cron_id, window_start).await;
    assert_eq!(snap.window_runs_today, 2, "after fire 2, 2 rows in window");
    let decision = evaluate_admission(&quota, &global, &snap);
    assert!(
        matches!(
            decision,
            AdmissionDecision::RefuseRunsPerDay { limit: 2, used: 2 }
        ),
        "fire 3 should be refused, got {decision:?}"
    );
    let audit_id = "run-3-rejected";
    storage
        .runs()
        .create_rejected(NewRejectedRun {
            id: audit_id,
            cron_id,
            scheduled_at: "2000-01-01 05:17:00",
            status: decision.rejected_status().unwrap(),
            coalesced_count: 1,
            error_kind: decision.error_kind().unwrap(),
            error_detail: &decision.error_detail(),
        })
        .await
        .unwrap();

    // ---- Audit row sanity. ----
    let audit = storage.runs().get(audit_id).await.unwrap().unwrap();
    assert_eq!(audit.status, "cancelled");
    assert_eq!(audit.error_kind.as_deref(), Some("quota_max_runs_per_day"));
    assert!(audit
        .error_detail
        .as_deref()
        .unwrap()
        .contains("max_runs_per_day=2"));
    assert!(audit.started_at.is_none(), "rejected row has no started_at");
    assert!(audit.exit_code.is_none(), "rejected row has no exit_code");
    assert!(audit.session_id.is_none(), "rejected row has no session");
    assert_eq!(audit.finished_at.as_deref(), Some("2000-01-01 05:17:00"));

    // List visible from `runs.list` so the TUI's runs panel will surface it.
    let runs = storage
        .runs()
        .list(RunsListFilter {
            cron_id: Some(cron_id),
            since: None,
            limit: 100,
        })
        .await
        .unwrap();
    assert_eq!(runs.len(), 3, "3 total rows: 2 completed + 1 rejected");
    assert!(runs
        .iter()
        .any(|r| r.status == "cancelled"
            && r.error_kind.as_deref() == Some("quota_max_runs_per_day")));

    // After the refusal, snapshot now shows 3 rows in window — proving the
    // audit row counts against the cap on subsequent fires too (so we don't
    // sneak in a 4th attempt by exploiting the rejection bookkeeping).
    let snap = snapshot_for(&storage, cron_id, window_start).await;
    assert_eq!(snap.window_runs_today, 3);
}

#[tokio::test]
async fn auto_pause_writes_audit_with_quota_paused_after_threshold_reached() {
    let (_dir, storage) = open_storage().await;
    let cron_id = "cron-wek33-pause";
    seed_project_and_cron(
        &storage, cron_id, /*max_runs_per_day*/ 24, /*pause_on*/ 3,
    )
    .await;
    let global = GlobalQuota {
        global_max_concurrent_runs: 0,
        cpu_load_throttle: None,
    };

    // Three terminal-failure bumps — first two below threshold (no pause),
    // third hits threshold (pause flips).
    for _ in 0..2 {
        let new_count = storage
            .crons()
            .bump_consecutive_failures(cron_id)
            .await
            .unwrap()
            .unwrap();
        assert!(new_count < 3);
        assert!(!storage.crons().pause_for_failures(cron_id).await.unwrap());
        assert_eq!(
            storage.crons().get(cron_id).await.unwrap().unwrap().enabled,
            1
        );
    }
    let third = storage
        .crons()
        .bump_consecutive_failures(cron_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(third, 3);
    let paused = storage.crons().pause_for_failures(cron_id).await.unwrap();
    assert!(paused, "third failure should auto-pause");
    let cron = storage.crons().get(cron_id).await.unwrap().unwrap();
    assert_eq!(cron.enabled, 0, "enabled flipped to 0 by auto-pause");

    // Subsequent admission now refuses with RefusePaused regardless of
    // anything else.
    let quota = quota_from(&cron);
    let snap = QuotaSnapshot {
        running_for_cron: 0,
        running_global: 0,
        window_runs_today: 0,
        window_cost_today: 0.0,
        current_loadavg_1m: None,
    };
    let decision = evaluate_admission(&quota, &global, &snap);
    assert_eq!(decision, AdmissionDecision::RefusePaused);

    let audit_id = "run-pause-audit";
    storage
        .runs()
        .create_rejected(NewRejectedRun {
            id: audit_id,
            cron_id,
            scheduled_at: "2000-01-01 06:17:00",
            status: decision.rejected_status().unwrap(),
            coalesced_count: 1,
            error_kind: decision.error_kind().unwrap(),
            error_detail: &decision.error_detail(),
        })
        .await
        .unwrap();
    let audit = storage.runs().get(audit_id).await.unwrap().unwrap();
    assert_eq!(audit.status, "cancelled");
    assert_eq!(audit.error_kind.as_deref(), Some("quota_paused"));

    // Calling pause_for_failures again is idempotent — already disabled.
    assert!(!storage.crons().pause_for_failures(cron_id).await.unwrap());

    // A successful run resets the counter, but does NOT re-enable
    // (re-enable is a separate user action by design).
    assert!(storage
        .crons()
        .reset_consecutive_failures(cron_id)
        .await
        .unwrap());
    let cron = storage.crons().get(cron_id).await.unwrap().unwrap();
    assert_eq!(cron.consecutive_failures, 0);
    assert_eq!(cron.enabled, 0, "reset does not re-enable");
}

#[tokio::test]
async fn cost_budget_refusal_writes_budget_exceeded_status() {
    let (_dir, storage) = open_storage().await;
    let cron_id = "cron-wek33-budget";
    seed_project_and_cron(&storage, cron_id, 100, 5).await;
    storage
        .crons()
        .upsert(CronUpsert {
            id: cron_id.into(),
            name: "wek33-budget".into(),
            enabled: true,
            project_id: storage
                .crons()
                .get(cron_id)
                .await
                .unwrap()
                .unwrap()
                .project_id
                .clone(),
            backend_id: "claude".into(),
            spawn_args: serde_json::json!({}),
            prompt: "wek33".into(),
            cron_expr: "* * * * *".into(),
            tz: "UTC".into(),
            catchup_mode: "coalesce".into(),
            max_concurrent_runs: 8,
            max_runs_per_day: 100,
            max_runtime_s: 1800,
            cost_budget_usd_per_day: Some(0.5),
            failure_backoff: "expo(1m,2,1h)".into(),
            pause_on_consecutive_failures: 5,
            consecutive_failures: 0,
            last_fired_at: None,
            next_fire_at: None,
        })
        .await
        .unwrap();

    // Seed one completed run that ate the whole budget.
    storage
        .runs()
        .create(NewRun {
            id: "run-spent".into(),
            cron_id: Some(cron_id.into()),
            session_id: None,
            scheduled_at: "2000-01-01 03:00:00".into(),
            started_at: Some("2000-01-01 03:00:00".into()),
            status: "running".into(),
            coalesced_count: 1,
        })
        .await
        .unwrap();
    storage
        .runs()
        .finish(
            "run-spent",
            RunFinish {
                finished_at: "2000-01-01 03:00:30".into(),
                status: "completed".into(),
                exit_code: Some(0),
                cost_usd_est: Some(0.5),
                error_kind: None,
                error_detail: None,
                tail_log: None,
            },
        )
        .await
        .unwrap();

    let cron = storage.crons().get(cron_id).await.unwrap().unwrap();
    let quota = quota_from(&cron);
    let snap = snapshot_for(&storage, cron_id, "2000-01-01 00:00:00").await;
    assert!(
        (snap.window_cost_today - 0.5).abs() < 1e-9,
        "window cost should reflect the seeded run"
    );
    let decision = evaluate_admission(
        &quota,
        &GlobalQuota {
            global_max_concurrent_runs: 0,
            cpu_load_throttle: None,
        },
        &snap,
    );
    assert!(matches!(
        decision,
        AdmissionDecision::RefuseBudgetExceeded { .. }
    ));
    storage
        .runs()
        .create_rejected(NewRejectedRun {
            id: "run-overbudget",
            cron_id,
            scheduled_at: "2000-01-01 03:05:00",
            status: decision.rejected_status().unwrap(),
            coalesced_count: 1,
            error_kind: decision.error_kind().unwrap(),
            error_detail: &decision.error_detail(),
        })
        .await
        .unwrap();
    let audit = storage.runs().get("run-overbudget").await.unwrap().unwrap();
    assert_eq!(audit.status, "budget_exceeded");
    assert_eq!(
        audit.error_kind.as_deref(),
        Some("quota_cost_budget_exceeded")
    );
}

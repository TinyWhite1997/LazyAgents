//! WEK-35 / M3.4 Crons tab acceptance — exercises Story 3 end-to-end
//! against the App + MockCronSource so the contract the daemon will
//! implement in M3.5 is pinned before the wire surface arrives.
//!
//! These tests are intentionally outside the unit tests in `crons.rs` /
//! `app.rs`: they walk the user-visible key sequence (just like the
//! runner would) and assert on the resulting state + rendered buffer.
//! If a future input-layer change re-routes a key, the unit tests stay
//! green and these break — exactly the signal we want for an
//! acceptance suite.

use la_tui::app::{Focus, Modal, Tab};
use la_tui::runner::draw;
use la_tui::{App, AppMsg, CronPreview, EditField, FieldEdit, MockCronSource, MockSessionSource};
use ratatui::backend::TestBackend;
use ratatui::Terminal;

fn app() -> App<MockSessionSource, MockCronSource> {
    App::with_sources(MockSessionSource::fixture(), MockCronSource::fixture())
}

fn render(app: &App<MockSessionSource, MockCronSource>, w: u16, h: u16) -> String {
    let backend = TestBackend::new(w, h);
    let mut terminal = Terminal::new(backend).expect("terminal");
    terminal
        .draw(|f| {
            let _ = draw(f, app);
        })
        .expect("draw");
    let buf = terminal.backend().buffer().clone();
    let area = buf.area();
    let mut out = String::new();
    for y in 0..area.height {
        for x in 0..area.width {
            out.push_str(buf[(x, y)].symbol());
        }
        out.push('\n');
    }
    out
}

#[test]
fn story3_create_edit_save_enable_and_trigger_a_cron() {
    let mut a = app();

    // Switch to the Crons tab via the digit shortcut. The hint layer
    // already advertises `2` so users coming from the Sessions tab can
    // type it without leaving the keyboard.
    a.handle(AppMsg::SetTab(Tab::Crons));
    assert_eq!(a.tab, Tab::Crons);
    assert_eq!(a.focus, Focus::Sidebar, "tab switch lands on the list pane");

    // Start a new cron (`n` from the list). Focus moves to the editor
    // and a draft becomes visible in the right pane.
    a.handle(AppMsg::CronNew);
    assert_eq!(a.focus, Focus::Main);
    let draft = a.crons.draft().expect("draft after CronNew");
    assert!(
        !draft.enabled,
        "new crons start disabled — confirm gate fires on first enable"
    );

    // Type a name into the Name field, then move to the cron expression
    // field via Tab and type a valid expression.
    a.handle(AppMsg::CronFieldEdit(FieldEdit::SetAll("dep-pulse".into())));
    // Navigate to CronExpr by pressing Tab the right number of times.
    while a.crons.field() != EditField::CronExpr {
        a.handle(AppMsg::CronFieldNext);
    }
    a.handle(AppMsg::CronFieldEdit(FieldEdit::SetAll("0 2 * * *".into())));
    assert!(
        a.crons.expr_is_valid(),
        "valid 5-field expr highlights green"
    );
    assert!(
        a.crons.preview().next.is_some(),
        "preview computes the next fire time"
    );

    // Save the draft (Enter from the editor; the input layer treats
    // Enter as Save inside the editor pane).
    a.handle(AppMsg::CronSaveDraft);
    assert!(a.crons.draft().is_none(), "draft cleared after save");
    let saved_id = a
        .crons
        .crons()
        .iter()
        .find(|c| c.name == "dep-pulse")
        .expect("the saved cron landed in the list")
        .id
        .clone();
    let saved_expr = a
        .crons
        .crons()
        .iter()
        .find(|c| c.id == saved_id)
        .unwrap()
        .cron_expr
        .clone();
    assert_eq!(saved_expr, "0 2 * * *");

    // Move cursor onto the newly saved row (it was pushed to the end)
    // and toggle enabled. Because it's the disabled→enabled transition,
    // the App opens the first-enable confirmation modal that the brief
    // calls out ("首次启用强制确认（弹预算与下次触发时间）").
    while a.crons.selected_id() != Some(&saved_id) {
        a.handle(AppMsg::CronListDown);
    }
    a.handle(AppMsg::CronToggleEnabled);
    let (cron_id, budget_label, next_label) = match a.modal.clone() {
        Some(Modal::ConfirmEnableCron {
            cron_id,
            budget_label,
            next_label,
            ..
        }) => (cron_id, budget_label, next_label),
        other => panic!("expected ConfirmEnableCron modal, got {:?}", other),
    };
    assert_eq!(cron_id, saved_id);
    // Budget defaults to "inherits global default" because we didn't
    // type a number into the Budget field.
    assert!(
        budget_label.contains("inherits"),
        "budget label = {budget_label}"
    );
    assert!(
        next_label.starts_with("下次："),
        "next-fire label is the same string the editor pane already showed: {next_label}"
    );

    // Confirm enable.
    a.handle(AppMsg::Confirm);
    assert!(a.modal.is_none());
    let after_enabled = a
        .crons
        .crons()
        .iter()
        .find(|c| c.id == saved_id)
        .unwrap()
        .enabled;
    assert!(after_enabled, "cron persisted as enabled after confirm");

    // `r` fires the cron once via the mock — the source records the id.
    a.handle(AppMsg::CronTriggerNow);
    assert_eq!(a.cron_source().triggered.last().unwrap(), &saved_id);
}

#[test]
fn invalid_cron_expression_blocks_save_and_highlights_red() {
    let mut a = app();
    a.handle(AppMsg::SetTab(Tab::Crons));
    a.handle(AppMsg::CronNew);
    while a.crons.field() != EditField::CronExpr {
        a.handle(AppMsg::CronFieldNext);
    }
    a.handle(AppMsg::CronFieldEdit(FieldEdit::SetAll(
        "not a cron".into(),
    )));
    assert!(
        !a.crons.expr_is_valid(),
        "expr_is_valid is false on garbage"
    );
    assert!(
        a.crons.preview().error.is_some(),
        "preview surfaces a human-readable error"
    );
    a.handle(AppMsg::CronSaveDraft);
    // The mock list should not have a "not a cron" row.
    let count = a
        .crons
        .crons()
        .iter()
        .filter(|c| c.cron_expr == "not a cron")
        .count();
    assert_eq!(count, 0, "garbage cron did NOT persist");
    assert!(
        a.last_toast
            .as_deref()
            .unwrap_or("")
            .contains("save aborted"),
        "save toast names the failure: {:?}",
        a.last_toast
    );
}

#[test]
fn dry_run_returns_five_fires_via_same_cron_crate_as_scheduler() {
    let mut a = app();
    a.handle(AppMsg::SetTab(Tab::Crons));
    // Default fixture's first row is "0 2 * * *" UTC — known schedule.
    a.handle(AppMsg::CronDryRun);
    let Some(Modal::DryRunCron { ref fires, .. }) = a.modal else {
        panic!("expected dry-run modal");
    };
    assert_eq!(
        fires.len(),
        CronPreview::DRY_RUN_N,
        "dry-run lists exactly N upcoming fires"
    );
}

#[test]
fn space_on_an_enabled_cron_disables_without_confirmation_modal() {
    // Disabling is one-step; only the disabled→enabled transition needs
    // the user to acknowledge the budget. Pin that here so a future
    // refactor doesn't accidentally show the confirm modal on disable.
    let mut a = app();
    a.handle(AppMsg::SetTab(Tab::Crons));
    // Fixture[0] is enabled.
    a.handle(AppMsg::CronToggleEnabled);
    assert!(a.modal.is_none(), "no confirm modal on disable");
    let row = a
        .crons
        .crons()
        .iter()
        .find(|c| c.id == "cron-nightly")
        .unwrap();
    assert!(!row.enabled);
}

#[test]
fn first_enable_cancel_reverts_to_disabled_so_state_stays_consistent() {
    // The brief says first-enable forces confirmation; if the user
    // bails, the row must NOT be left half-enabled.
    let mut a = app();
    a.handle(AppMsg::SetTab(Tab::Crons));
    // Move to the disabled fixture (index 1).
    a.handle(AppMsg::CronListDown);
    let id = a.crons.selected_id().unwrap().to_string();
    a.handle(AppMsg::CronToggleEnabled);
    assert!(matches!(a.modal, Some(Modal::ConfirmEnableCron { .. })));
    a.handle(AppMsg::Cancel);
    let row = a.crons.crons().iter().find(|c| c.id == id).unwrap();
    assert!(!row.enabled, "cancel restored the disabled state");
}

#[test]
fn delete_requires_confirmation_then_removes_row() {
    let mut a = app();
    a.handle(AppMsg::SetTab(Tab::Crons));
    let before = a.crons.crons().len();
    a.handle(AppMsg::CronDelete);
    assert!(matches!(a.modal, Some(Modal::ConfirmDeleteCron { .. })));
    a.handle(AppMsg::Confirm);
    assert_eq!(a.crons.crons().len(), before - 1);
}

#[test]
fn crons_tab_renders_list_and_editor_panes() {
    let a = {
        let mut a = app();
        a.handle(AppMsg::SetTab(Tab::Crons));
        a
    };
    let text = render(&a, 120, 30);
    assert!(text.contains("Crons"), "list title visible:\n{text}");
    assert!(text.contains("Editor"), "editor pane visible");
    assert!(text.contains("nightly-review"), "fixture cron listed");
    assert!(text.contains("0 2 * * *"), "cron expr visible in list");
    // The editor pane previews the next fire. ratatui's TestBackend
    // separates double-width CJK chars with a space, so check the
    // characters individually rather than the literal "下次：" substring.
    assert!(
        text.contains('下') && text.contains('次'),
        "human preview slot visible:\n{text}"
    );
}

#[test]
fn editor_pane_invalid_expr_renders_error_marker() {
    let mut a = app();
    a.handle(AppMsg::SetTab(Tab::Crons));
    a.handle(AppMsg::CronNew);
    while a.crons.field() != EditField::CronExpr {
        a.handle(AppMsg::CronFieldNext);
    }
    a.handle(AppMsg::CronFieldEdit(FieldEdit::SetAll("garbage".into())));
    let text = render(&a, 120, 30);
    assert!(
        text.contains("✗"),
        "invalid-expression marker rendered:\n{text}"
    );
}

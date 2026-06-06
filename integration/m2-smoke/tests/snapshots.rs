mod common;

use std::path::PathBuf;

use common::{
    bootstrap_daemon, call, client, make_bare_project_repo, standard_backends, write_agent_change,
};
use la_proto::methods::{
    SessionsCreateParams, SessionsCreateResult, WorktreeDiffParams, WorktreeDiffResult,
    WorktreeStatusParams, WorktreeStatusResult,
};
use la_tui::{DiffPayload, DiffView, DiffViewWidget};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::widgets::Widget;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn diff_panel_widget_snapshot_is_written_as_artifact_input() {
    let daemon = bootstrap_daemon(standard_backends()).await;
    let (_project, repo) = make_bare_project_repo().await;
    let mut conn = client(&daemon.socket).await;
    let created: SessionsCreateResult = call(
        &mut conn,
        1,
        "sessions.create",
        SessionsCreateParams {
            project_dir: repo.to_string_lossy().into_owned(),
            backend: "claude".into(),
            args: vec![],
            prompt: None,
            worktree: true,
        },
    )
    .await;
    let wt = PathBuf::from(&created.cwd);
    let path = write_agent_change(&wt, "claude", "snapshot").await;
    let status: WorktreeStatusResult = call(
        &mut conn,
        2,
        "worktree.status",
        WorktreeStatusParams {
            session_id: created.session_id.clone(),
        },
    )
    .await;
    let diff: WorktreeDiffResult = call(
        &mut conn,
        3,
        "worktree.diff",
        WorktreeDiffParams {
            session_id: created.session_id,
            path,
            staged: false,
            context_lines: None,
        },
    )
    .await;

    let snapshot = render_diff_panel_widget_snapshot_text(status, diff);
    assert!(snapshot.contains("claude"));
    assert!(snapshot.contains("unstaged"));

    let out_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/m2-smoke/snapshots");
    tokio::fs::create_dir_all(&out_dir).await.unwrap();
    tokio::fs::write(out_dir.join("diff-panel.txt"), snapshot)
        .await
        .unwrap();
}

fn render_diff_panel_widget_snapshot_text(
    status: WorktreeStatusResult,
    diff: WorktreeDiffResult,
) -> String {
    let mut view = DiffView::new();
    view.apply_status(status.files);
    let _ = view.toggle_expand();
    view.apply_diff(DiffPayload {
        file: diff.file,
        hunks: diff.hunks,
        truncated: diff.truncated,
    });

    let area = Rect::new(0, 0, 100, 12);
    let mut buf = Buffer::empty(area);
    DiffViewWidget::new(&view).render(area, &mut buf);
    buffer_text(&buf)
}

fn buffer_text(buf: &Buffer) -> String {
    let mut out = String::new();
    let area = buf.area();
    for y in 0..area.height {
        for x in 0..area.width {
            out.push_str(buf[(x, y)].symbol());
        }
        out.push('\n');
    }
    out
}

#![cfg(unix)]

mod common;

use std::path::PathBuf;

use common::{
    bootstrap_daemon, call, client, make_bare_project_repo, standard_backends, write_agent_change,
};
use la_proto::methods::{
    SessionsCreateParams, SessionsCreateResult, WorktreeDiffParams, WorktreeDiffResult,
    WorktreeStatusParams, WorktreeStatusResult,
};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn diff_payload_buffer_snapshot_is_written_as_artifact_input() {
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

    let snapshot = render_diff_payload_snapshot_text(&status, &diff);
    assert!(snapshot.contains("claude"));
    assert!(snapshot.contains("unstaged"));

    let out_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/m2-smoke/snapshots");
    tokio::fs::create_dir_all(&out_dir).await.unwrap();
    tokio::fs::write(out_dir.join("diff-payload.txt"), snapshot)
        .await
        .unwrap();
}

fn render_diff_payload_snapshot_text(
    status: &WorktreeStatusResult,
    diff: &WorktreeDiffResult,
) -> String {
    let area = Rect::new(0, 0, 100, 12);
    let mut buf = Buffer::empty(area);
    put(&mut buf, 0, 0, &format!("branch: {}", status.branch));
    put(&mut buf, 0, 1, &format!("file: {}", diff.file.path));
    put(
        &mut buf,
        0,
        2,
        &format!(
            "staged: {} unstaged: {}",
            diff.file.staged_hunks, diff.file.unstaged_hunks
        ),
    );
    for (idx, hunk) in diff.hunks.iter().take(3).enumerate() {
        put(&mut buf, 0, 4 + idx as u16 * 2, &hunk.header);
        let preview = hunk
            .lines
            .iter()
            .find(|line| line.content.contains("claude"))
            .map(|line| line.content.as_str())
            .unwrap_or("");
        put(&mut buf, 2, 5 + idx as u16 * 2, preview);
    }
    buffer_text(&buf)
}

fn put(buf: &mut Buffer, x: u16, y: u16, text: &str) {
    for (offset, ch) in text.chars().enumerate() {
        let x = x + offset as u16;
        if x < buf.area().width && y < buf.area().height {
            buf[(x, y)].set_symbol(&ch.to_string());
        }
    }
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

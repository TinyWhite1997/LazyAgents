//! Exports the JSON Schemas for the full M1 method/notification surface
//! into `docs/schema/`.
//!
//! Run with: `cargo run -p la-proto --bin la-proto-gen-schema -- <out-dir>`.
//!
//! If `<out-dir>` is omitted, defaults to `docs/schema` relative to the
//! current working directory. We deliberately fail loudly on IO errors so
//! the CI step that calls this catches missing-directory drift early.
//!
//! The same set of files is also asserted byte-for-byte by the `schema_*`
//! golden tests in `tests/round_trip.rs`, so editing a typed struct without
//! re-running this binary turns CI red.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use la_proto::methods::{
    AdaptersDiscover, EventsSubscribe, Initialize, Method, MetricsScrape, ProjectsCreate,
    ProjectsList, SessionsArchive, SessionsAttach, SessionsCreate, SessionsDelete, SessionsDetach,
    SessionsImport, SessionsList, SessionsReplay, SessionsResize, SessionsSignal, SessionsWrite,
    Shutdown, WorktreeCommit, WorktreeDiff, WorktreeDiscard, WorktreeOpenInEditor, WorktreeStage,
    WorktreeStatus, WorktreeUnstage,
};
use la_proto::notifications::{
    CronFired, DaemonHealth, NotificationMethod, SchedulerHealth, SessionGap, SessionOutput,
    SessionStateNotice, WorktreeChanged, WorktreeCommitCreated,
};
use schemars::schema::RootSchema;
use schemars::schema_for;

fn main() {
    let out_dir = env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("docs/schema"));
    fs::create_dir_all(&out_dir).expect("create docs/schema");

    // Methods: emit `<method>.params.schema.json` and `<method>.result.schema.json`.
    write_method::<Initialize>(&out_dir);
    write_method::<Shutdown>(&out_dir);
    write_method::<SessionsList>(&out_dir);
    write_method::<SessionsCreate>(&out_dir);
    write_method::<SessionsAttach>(&out_dir);
    write_method::<SessionsDetach>(&out_dir);
    write_method::<SessionsWrite>(&out_dir);
    write_method::<SessionsResize>(&out_dir);
    write_method::<SessionsSignal>(&out_dir);
    write_method::<SessionsArchive>(&out_dir);
    write_method::<SessionsDelete>(&out_dir);
    write_method::<ProjectsList>(&out_dir);
    write_method::<ProjectsCreate>(&out_dir);
    write_method::<AdaptersDiscover>(&out_dir);
    write_method::<SessionsImport>(&out_dir);
    write_method::<SessionsReplay>(&out_dir);
    write_method::<EventsSubscribe>(&out_dir);
    write_method::<WorktreeStatus>(&out_dir);
    write_method::<WorktreeDiff>(&out_dir);
    write_method::<WorktreeStage>(&out_dir);
    write_method::<WorktreeUnstage>(&out_dir);
    write_method::<WorktreeDiscard>(&out_dir);
    write_method::<WorktreeCommit>(&out_dir);
    write_method::<WorktreeOpenInEditor>(&out_dir);
    // M4.5 / WEK-75: observability scrape RPC. Schema is part of the same
    // golden set so a metric-naming refactor that drifts the param/result
    // shape (e.g. adding `format: "openmetrics" | "text"`) trips CI.
    write_method::<MetricsScrape>(&out_dir);

    // Notifications: only params have a schema.
    write_notification::<SessionOutput>(&out_dir);
    write_notification::<SessionStateNotice>(&out_dir);
    write_notification::<SessionGap>(&out_dir);
    write_notification::<CronFired>(&out_dir);
    write_notification::<DaemonHealth>(&out_dir);
    write_notification::<SchedulerHealth>(&out_dir);
    write_notification::<WorktreeChanged>(&out_dir);
    write_notification::<WorktreeCommitCreated>(&out_dir);

    println!("wrote schemas to {}", out_dir.display());
}

fn write_method<M: Method>(out_dir: &Path) {
    let params_schema = schema_for!(M::Params);
    let result_schema = schema_for!(M::Result);
    let safe = method_to_filename(M::NAME);
    write_schema(
        out_dir,
        &format!("{safe}.params.schema.json"),
        &params_schema,
    );
    write_schema(
        out_dir,
        &format!("{safe}.result.schema.json"),
        &result_schema,
    );
}

fn write_notification<N: NotificationMethod>(out_dir: &Path) {
    let schema = schema_for!(N::Params);
    let safe = method_to_filename(N::NAME);
    write_schema(out_dir, &format!("{safe}.params.schema.json"), &schema);
}

fn write_schema(out_dir: &Path, file: &str, schema: &RootSchema) {
    let path = out_dir.join(file);
    let json = serde_json::to_string_pretty(schema).expect("serialize schema");
    fs::write(&path, json + "\n").unwrap_or_else(|e| panic!("write {}: {e}", path.display()));
    println!("  {}", path.display());
}

fn method_to_filename(name: &str) -> String {
    // `sessions.create` → `sessions__create`. Dots aren't unsafe on disk but
    // double-dotting on Windows extension handlers can confuse tooling; the
    // double underscore convention is also what `tower-lsp` uses.
    name.replace('.', "__")
}

//! Exports the JSON Schemas for all M0.2 method params/results and
//! notifications into `docs/schema/`.
//!
//! Run with: `cargo run -p la-proto --bin la-proto-gen-schema -- <out-dir>`.
//!
//! If `<out-dir>` is omitted, defaults to `docs/schema` relative to the
//! current working directory. We deliberately fail loudly on IO errors so
//! the CI step that calls this catches missing-directory drift early.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use la_proto::methods::{Initialize, Method, SessionsAttach, SessionsCreate, SessionsWrite};
use la_proto::notifications::{NotificationMethod, SessionOutput};
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
    write_method::<SessionsCreate>(&out_dir);
    write_method::<SessionsAttach>(&out_dir);
    write_method::<SessionsWrite>(&out_dir);

    // Notifications: only params have a schema.
    write_notification::<SessionOutput>(&out_dir);

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

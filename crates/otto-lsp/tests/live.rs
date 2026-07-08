//! Best-effort live smoke test against a real `rust-analyzer` on `PATH`.
//!
//! `#[ignore]`d — does NOT run under plain `cargo test` (only compiled, never
//! executed, so the workspace test gate stays hermetic/offline). Run explicitly:
//!
//! ```text
//! cargo test -p otto-lsp -- --ignored
//! ```
//!
//! Skips cleanly (early `return`) if `rust-analyzer` isn't resolvable on `PATH`.

use otto_lsp::registry::resolve_command;
use otto_lsp::service::{Lsp, LspConfigResolved};
use std::time::Duration;

#[tokio::test]
#[ignore]
async fn rust_analyzer_reports_type_error_on_touch_file() {
    if resolve_command(&["rust-analyzer"]).is_none() {
        eprintln!("skipping: rust-analyzer not found on PATH");
        return;
    }

    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();

    std::fs::write(
        root.join("Cargo.toml"),
        r#"[package]
name = "live-smoke"
version = "0.1.0"
edition = "2021"
"#,
    )
    .expect("write Cargo.toml");

    let src_dir = root.join("src");
    std::fs::create_dir_all(&src_dir).expect("mkdir src");
    let main_rs = src_dir.join("main.rs");
    std::fs::write(
        &main_rs,
        r#"fn main() {
    let _x: u32 = "not a number";
}
"#,
    )
    .expect("write main.rs");

    let lsp = Lsp::new(root.to_path_buf(), LspConfigResolved::enabled_default());

    // rust-analyzer publishes an initial (often empty) diagnostics push right
    // after `didOpen`, well before it has finished indexing/type-checking —
    // so `touch_file`'s internal wait-for-a-fresh-push (bounded at 5s) reliably
    // returns fast without the real type error yet. The actual error-bearing
    // push can take much longer (cold index, crate metadata, std sources), so
    // after the initial touch we poll `report_for` (a cheap sync read over
    // whatever the background reader task has already collected) for up to a
    // couple minutes rather than repeatedly re-touching the file.
    lsp.touch_file(&main_rs, true).await;

    let mut report = String::new();
    for _ in 0..30 {
        report = lsp.report_for(&main_rs);
        if report.contains("ERROR") {
            break;
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }

    assert!(
        report.contains("ERROR"),
        "expected an ERROR diagnostic in report, got: {report:?}"
    );
}

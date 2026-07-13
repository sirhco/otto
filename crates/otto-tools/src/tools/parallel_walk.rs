//! Shared multi-threaded directory walk for `grep`, `glob`, and `skill`
//! discovery, off the async runtime.
//!
//! `ignore::WalkBuilder::build_parallel()` already spawns its own worker
//! threads (one per available core) that traverse the tree concurrently and
//! respect `.gitignore` — no extra dependency needed on top of `ignore`
//! (which every caller here already pulls in). Running the whole walk inside
//! `spawn_blocking` keeps `std::fs`/regex work off async runtime workers.

use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use ignore::{WalkBuilder, WalkState};

/// Walk `root` in parallel, calling `classify` for every non-directory entry
/// and collecting the (possibly empty) items it returns. `classify` runs
/// concurrently across worker threads, so it must be `Send + Sync`.
///
/// Once the collected total reaches `limit`, remaining threads stop at their
/// next entry (cooperative, not instant — a thread mid-file finishes that
/// file first).
pub async fn parallel_collect<T, F>(root: PathBuf, limit: Option<usize>, classify: F) -> Vec<T>
where
    T: Send + 'static,
    F: Fn(&ignore::DirEntry) -> Vec<T> + Send + Sync + 'static,
{
    tokio::task::spawn_blocking(move || {
        let results: Mutex<Vec<T>> = Mutex::new(Vec::new());
        let stop = AtomicBool::new(false);
        WalkBuilder::new(&root)
            .hidden(false)
            .require_git(false)
            .build_parallel()
            .run(|| {
                Box::new(|entry| {
                    if stop.load(Ordering::Relaxed) {
                        return WalkState::Quit;
                    }
                    let Ok(entry) = entry else {
                        return WalkState::Continue;
                    };
                    if entry.file_type().map(|t| t.is_dir()).unwrap_or(true) {
                        return WalkState::Continue;
                    }
                    let items = classify(&entry);
                    if items.is_empty() {
                        return WalkState::Continue;
                    }
                    let mut guard = results.lock().unwrap();
                    guard.extend(items);
                    if let Some(limit) = limit
                        && guard.len() >= limit
                    {
                        stop.store(true, Ordering::Relaxed);
                        return WalkState::Quit;
                    }
                    WalkState::Continue
                })
            });
        results.into_inner().unwrap()
    })
    .await
    .expect("parallel_collect worker panicked")
}

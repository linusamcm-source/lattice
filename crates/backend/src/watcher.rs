//! Debounced filesystem watcher (`docs/orignal_specs/SPEC.md` §5.2 "Watcher").
//!
//! Wraps `notify` to observe a repository tree and forward the paths of changed
//! **source** files (Rust, Python, or TypeScript) to an async consumer. A burst of
//! rapid events for the same path is coalesced into a single emission via a
//! [`DEBOUNCE`] quiet-period window, so a save that the editor reports as several
//! events re-parses the file only once (`SPEC.md` §11.2). The watcher never panics
//! on a `notify` error — it logs to stderr and keeps running (`SPEC.md` §11.1).
//!
//! The OS-callback glue is deliberately thin; the testable logic lives in
//! [`is_source_file`] (the extension filter) and [`debounce_loop`] (the coalescer),
//! both exercised directly so coverage does not depend on filesystem-event timing.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use notify::{Event, RecommendedWatcher, RecursiveMode, Watcher};
use tokio::sync::mpsc::{unbounded_channel, Sender, UnboundedReceiver};
use tokio::time::timeout;

/// Quiet period used to coalesce a burst of change events for the same path.
///
/// After the last raw event for a path, the watcher waits this long with no
/// further event before emitting once. 150 ms absorbs an editor's multi-event
/// save without adding perceptible latency to the live graph.
pub const DEBOUNCE: Duration = Duration::from_millis(150);

/// Returns `true` when `path` names a source file Lattice parses (`.rs`, `.py`,
/// or `.ts` extension).
///
/// This is the watcher's sole content filter: changes to any other file type are
/// dropped before they ever reach the debounce stage. The accepted extension set
/// mirrors the language paths behind [`crate::parser::parse_source`].
pub fn is_source_file(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|ext| ext.to_str()),
        Some("rs") | Some("py") | Some("ts")
    )
}

/// Coalesces a stream of raw changed paths into debounced emissions.
///
/// Reads raw paths from `rx` (already source-file-filtered), groups every path seen
/// within a [`DEBOUNCE`] quiet window into a deduplicated set, and forwards each
/// distinct path once on `tx` after the window settles. Returns when `rx` closes
/// (all senders dropped) or `tx`'s receiver is gone.
///
/// Kept separate from the `notify` wiring so it can be unit-tested against an
/// in-memory channel, with no filesystem dependency.
async fn debounce_loop(mut rx: UnboundedReceiver<PathBuf>, tx: Sender<PathBuf>) {
    while let Some(first) = rx.recv().await {
        let mut pending: HashSet<PathBuf> = HashSet::new();
        pending.insert(first);
        // Extend the window on each subsequent event; flush once it goes quiet.
        loop {
            match timeout(DEBOUNCE, rx.recv()).await {
                Ok(Some(path)) => {
                    pending.insert(path);
                }
                // Channel closed: flush what we have, then the outer loop ends.
                Ok(None) => break,
                // Quiet for DEBOUNCE: the burst is over.
                Err(_) => break,
            }
        }
        for path in pending {
            if tx.send(path).await.is_err() {
                return; // consumer dropped; nothing left to do.
            }
        }
    }
}

/// Watches `root` recursively and forwards debounced source-file change paths on `tx`.
///
/// Runs until the task is cancelled or `tx`'s receiver is dropped. Any `notify`
/// setup or runtime error is logged to stderr and — for runtime errors — skipped;
/// a failure to initialise the watcher logs and returns without panicking.
pub async fn watch(root: PathBuf, tx: Sender<PathBuf>) {
    // notify's callback runs on its own thread; bridge into async via an
    // unbounded channel whose non-async `send` is safe to call from there.
    let (raw_tx, raw_rx) = unbounded_channel::<PathBuf>();

    let mut watcher: RecommendedWatcher =
        match notify::recommended_watcher(move |res: notify::Result<Event>| match res {
            Ok(event) => {
                for path in event.paths {
                    if is_source_file(&path) {
                        // Receiver lives as long as `watch`; ignore send errors
                        // that occur only during shutdown.
                        let _ = raw_tx.send(path);
                    }
                }
            }
            Err(error) => eprintln!("lattice watcher event error: {error}"),
        }) {
            Ok(watcher) => watcher,
            Err(error) => {
                eprintln!("lattice watcher init error: {error}");
                return;
            }
        };

    if let Err(error) = watcher.watch(&root, RecursiveMode::Recursive) {
        eprintln!("lattice watch error for {}: {error}", root.display());
        return;
    }

    // `watcher` must stay alive for the whole loop, or watching stops.
    debounce_loop(raw_rx, tx).await;
    drop(watcher);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tempfile::tempdir;
    use tokio::sync::mpsc::{channel, unbounded_channel};
    use tokio::time::{sleep, timeout};

    #[test]
    fn is_source_file_accepts_source_exts_and_rejects_others() {
        assert!(is_source_file(Path::new("/x/a.rs")));
        assert!(is_source_file(Path::new("a.rs")));
        assert!(is_source_file(Path::new("/x/a.py")));
        assert!(is_source_file(Path::new("a.py")));
        assert!(is_source_file(Path::new("/x/a.ts")));
        assert!(is_source_file(Path::new("a.ts")));
        assert!(!is_source_file(Path::new("/x/notes.txt")));
        assert!(!is_source_file(Path::new("/x/Makefile")));
        assert!(!is_source_file(Path::new("/x/a.rs.bak")));
    }

    #[tokio::test]
    async fn debounce_loop_coalesces_repeats_of_same_path_to_one() {
        let (raw_tx, raw_rx) = unbounded_channel::<PathBuf>();
        let (out_tx, mut out_rx) = channel::<PathBuf>(8);
        let handle = tokio::spawn(debounce_loop(raw_rx, out_tx));

        let p = PathBuf::from("/x/a.rs");
        for _ in 0..3 {
            raw_tx.send(p.clone()).expect("send");
        }

        let first = timeout(DEBOUNCE * 4, out_rx.recv())
            .await
            .expect("emit within window")
            .expect("some path");
        assert_eq!(first, p);
        // No second emission for the coalesced burst.
        assert!(timeout(DEBOUNCE * 2, out_rx.recv()).await.is_err());

        drop(raw_tx);
        let _ = handle.await;
    }

    #[tokio::test]
    async fn debounce_loop_forwards_a_single_path() {
        let (raw_tx, raw_rx) = unbounded_channel::<PathBuf>();
        let (out_tx, mut out_rx) = channel::<PathBuf>(8);
        let handle = tokio::spawn(debounce_loop(raw_rx, out_tx));

        let p = PathBuf::from("/x/only.rs");
        raw_tx.send(p.clone()).expect("send");
        let got = timeout(DEBOUNCE * 4, out_rx.recv())
            .await
            .expect("emit")
            .expect("some");
        assert_eq!(got, p);

        drop(raw_tx);
        let _ = handle.await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn writing_rs_file_emits_exactly_one_path() {
        let dir = tempdir().expect("tempdir");
        let (tx, mut rx) = channel::<PathBuf>(16);
        let root = dir.path().to_path_buf();
        let h = tokio::spawn(watch(root, tx));
        sleep(Duration::from_millis(300)).await; // let the watcher register

        let file = dir.path().join("a.rs");
        std::fs::write(&file, "fn x() {}").expect("write");

        let got = timeout(DEBOUNCE + Duration::from_secs(1), rx.recv())
            .await
            .expect("a path within budget")
            .expect("some path");
        assert!(got.ends_with("a.rs"), "got: {}", got.display());
        h.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn writing_non_rust_file_emits_nothing() {
        let dir = tempdir().expect("tempdir");
        let (tx, mut rx) = channel::<PathBuf>(16);
        let root = dir.path().to_path_buf();
        let h = tokio::spawn(watch(root, tx));
        sleep(Duration::from_millis(300)).await;

        std::fs::write(dir.path().join("notes.txt"), "hello").expect("write");

        assert!(
            timeout(DEBOUNCE + Duration::from_secs(1), rx.recv())
                .await
                .is_err(),
            "non-rust change must not emit"
        );
        h.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn rapid_writes_to_same_file_coalesce_to_at_most_one() {
        let dir = tempdir().expect("tempdir");
        let (tx, mut rx) = channel::<PathBuf>(16);
        let root = dir.path().to_path_buf();
        let h = tokio::spawn(watch(root, tx));
        sleep(Duration::from_millis(300)).await;

        let file = dir.path().join("a.rs");
        for i in 0..3 {
            std::fs::write(&file, format!("fn x() {{ /* {i} */ }}")).expect("write");
        }

        // Drain for several debounce windows, then assert we saw at most one.
        let mut received = 0_usize;
        while let Ok(Some(_)) = timeout(DEBOUNCE * 3, rx.recv()).await {
            received += 1;
        }
        assert!(
            received <= 1,
            "expected <= 1 coalesced emit, got {received}"
        );
        h.abort();
    }
}

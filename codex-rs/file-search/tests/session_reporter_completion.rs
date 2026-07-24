#![allow(clippy::unwrap_used)]

use codex_file_search::FileSearchOptions;
use codex_file_search::FileSearchSnapshot;
use codex_file_search::SessionReporter;
use codex_file_search::create_session;
use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Condvar;
use std::sync::Mutex;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::thread;
use std::time::Duration;
use std::time::Instant;

const UPDATE_COUNT: usize = 64;
static NEXT_FIXTURE_ID: AtomicU64 = AtomicU64::new(0);

#[derive(Default)]
struct RecordingReporter {
    updates: Mutex<Vec<(String, Instant)>>,
    completions: Mutex<Vec<Instant>>,
    changed: Condvar,
}

impl RecordingReporter {
    fn wait_for_initial_completion(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let mut completions = self.completions.lock().unwrap();
        loop {
            if !completions.is_empty() {
                return true;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return false;
            }
            let (next_completions, _) = self.changed.wait_timeout(completions, remaining).unwrap();
            completions = next_completions;
        }
    }

    fn wait_for_update_after(
        &self,
        query: &str,
        sent_at: Instant,
        timeout: Duration,
    ) -> Option<Instant> {
        let deadline = Instant::now() + timeout;
        let mut updates = self.updates.lock().unwrap();
        loop {
            if let Some((_, observed_at)) =
                updates.iter().rev().find(|(observed_query, observed_at)| {
                    observed_query == query && *observed_at >= sent_at
                })
            {
                return Some(*observed_at);
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return None;
            }
            let (next_updates, _) = self.changed.wait_timeout(updates, remaining).unwrap();
            updates = next_updates;
        }
    }

    fn wait_for_completion_after(&self, update_at: Instant, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let mut completions = self.completions.lock().unwrap();
        loop {
            if completions
                .iter()
                .any(|completion_at| *completion_at >= update_at)
            {
                return true;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return false;
            }
            let (next_completions, _) = self.changed.wait_timeout(completions, remaining).unwrap();
            completions = next_completions;
        }
    }

    fn completion_count(&self) -> usize {
        self.completions.lock().unwrap().len()
    }
}

impl SessionReporter for RecordingReporter {
    fn on_update(&self, snapshot: &FileSearchSnapshot) {
        self.updates
            .lock()
            .unwrap()
            .push((snapshot.query.clone(), Instant::now()));
        self.changed.notify_all();
    }

    fn on_complete(&self) {
        self.completions.lock().unwrap().push(Instant::now());
        self.changed.notify_all();
    }
}

struct FixtureDir {
    path: PathBuf,
}

impl FixtureDir {
    fn new() -> Self {
        let id = NEXT_FIXTURE_ID.fetch_add(1, Ordering::Relaxed);
        let path = std::env::current_dir().unwrap().join(format!(
            ".file-search-completion-fixture-{}-{id}",
            std::process::id()
        ));
        fs::create_dir(&path).unwrap();
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for FixtureDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

#[test]
fn session_reports_completion_for_each_sequential_query_update() {
    let fixture = FixtureDir::new();
    for update in 0..UPDATE_COUNT {
        fs::write(
            fixture
                .path()
                .join(format!("completion-marker-{update:02}.txt")),
            b"",
        )
        .unwrap();
    }

    let reporter = Arc::new(RecordingReporter::default());
    let session = create_session(
        vec![fixture.path().to_path_buf()],
        FileSearchOptions {
            compute_indices: false,
            respect_gitignore: false,
            ..Default::default()
        },
        reporter.clone(),
        None,
    )
    .unwrap();

    assert!(
        reporter.wait_for_initial_completion(Duration::from_secs(5)),
        "initial walk did not complete"
    );

    for update in 0..UPDATE_COUNT {
        let query = format!("completion-marker-{update:02}");
        let sent_at = Instant::now();
        session.update_query(&query);
        let update_at = reporter.wait_for_update_after(&query, sent_at, Duration::from_secs(5));
        assert!(
            update_at.is_some(),
            "query update {update} did not produce its own callback"
        );
        assert!(
            reporter.wait_for_completion_after(update_at.unwrap(), Duration::from_secs(5)),
            "query update {update} did not produce a completion after its callback"
        );
    }

    assert!(
        reporter.completion_count() >= UPDATE_COUNT + 1,
        "expected an initial completion and one completion for each query update"
    );
}

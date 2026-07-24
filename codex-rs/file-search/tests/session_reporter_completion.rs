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

const UPDATE_COUNT: usize = 64;
static NEXT_FIXTURE_ID: AtomicU64 = AtomicU64::new(0);

#[derive(Default)]
struct ReporterState {
    updates: Vec<String>,
    completion_count: usize,
}

#[derive(Default)]
struct RecordingReporter {
    state: Mutex<ReporterState>,
    changed: Condvar,
}

impl RecordingReporter {
    // Completion is the public synchronization signal. Deliberately use the
    // same unbounded condition-variable wait as RunReporter rather than a
    // scheduler-dependent deadline: failure to signal is the contract failure.
    fn wait_for_initial_completion(&self) {
        let mut state = self.lock_state();
        while state.completion_count == 0 {
            state = self
                .changed
                .wait(state)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
        }
    }

    fn wait_for_update(&self, query: &str) {
        let mut state = self.lock_state();
        while !state
            .updates
            .iter()
            .any(|observed_query| observed_query == query)
        {
            state = self
                .changed
                .wait(state)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
        }
    }

    fn wait_for_completion_after(&self, prior_count: usize) {
        let mut state = self.lock_state();
        while state.completion_count <= prior_count {
            state = self
                .changed
                .wait(state)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
        }
    }

    fn completion_count(&self) -> usize {
        self.lock_state().completion_count
    }

    fn lock_state(&self) -> std::sync::MutexGuard<'_, ReporterState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl SessionReporter for RecordingReporter {
    fn on_update(&self, snapshot: &FileSearchSnapshot) {
        self.lock_state().updates.push(snapshot.query.clone());
        self.changed.notify_all();
    }

    fn on_complete(&self) {
        self.lock_state().completion_count += 1;
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

    reporter.wait_for_initial_completion();

    for update in 0..UPDATE_COUNT {
        let query = format!("completion-marker-{update:02}");
        let completions_before = reporter.completion_count();
        session.update_query(&query);
        reporter.wait_for_update(&query);
        reporter.wait_for_completion_after(completions_before);
    }

    assert!(
        reporter.completion_count() >= UPDATE_COUNT + 1,
        "expected an initial completion and one completion for each query update"
    );
}

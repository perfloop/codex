#![allow(clippy::unwrap_used)]

use codex_file_search::FileSearchOptions;
use codex_file_search::FileSearchSnapshot;
use codex_file_search::SessionReporter;
use codex_file_search::create_session;
use std::fs;
use std::sync::Arc;
use std::sync::Condvar;
use std::sync::Mutex;

const UPDATE_COUNT: usize = 64;

#[derive(Default)]
struct ReporterState {
    query: String,
    sequence: usize,
    query_sequence: usize,
    completion_sequence: usize,
    completions: usize,
}

#[derive(Default)]
struct RecordingReporter {
    state: Mutex<ReporterState>,
    changed: Condvar,
}

impl RecordingReporter {
    fn wait_for_completion(&self) {
        let mut state = self.lock();
        while state.completion_sequence == 0 {
            state = self
                .changed
                .wait(state)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
    }

    fn wait_for_query_and_completion(&self, query: &str) {
        let mut state = self.lock();
        while state.query != query || state.completion_sequence <= state.query_sequence {
            state = self
                .changed
                .wait(state)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
    }

    fn completions(&self) -> usize {
        self.lock().completions
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, ReporterState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl SessionReporter for RecordingReporter {
    fn on_update(&self, snapshot: &FileSearchSnapshot) {
        let mut state = self.lock();
        state.sequence += 1;
        state.query.clone_from(&snapshot.query);
        state.query_sequence = state.sequence;
        self.changed.notify_all();
    }

    fn on_complete(&self) {
        let mut state = self.lock();
        state.sequence += 1;
        state.completion_sequence = state.sequence;
        state.completions += 1;
        self.changed.notify_all();
    }
}

#[test]
fn session_reports_completion_for_each_sequential_query_update() {
    let fixture = tempfile::tempdir().unwrap();
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

    reporter.wait_for_completion();
    for update in 0..UPDATE_COUNT {
        let query = format!("completion-marker-{update:02}");
        session.update_query(&query);
        reporter.wait_for_query_and_completion(&query);
    }
    assert!(reporter.completions() > UPDATE_COUNT);
}

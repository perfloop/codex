#![allow(clippy::unwrap_used)]

use codex_file_search::FileSearchOptions;
use codex_file_search::FileSearchSnapshot;
use codex_file_search::SessionReporter;
use codex_file_search::create_session;
use pretty_assertions::assert_eq;
use std::fs;
use std::sync::Arc;
use std::sync::Condvar;
use std::sync::Mutex;
use std::time::Duration;
use std::time::Instant;

#[derive(Default)]
struct RecordingReporter {
    state: Mutex<ReporterState>,
    cv: Condvar,
}

#[derive(Default)]
struct ReporterState {
    updates: Vec<FileSearchSnapshot>,
    complete_count: usize,
}

impl RecordingReporter {
    fn wait_for_query(&self, query: &str, timeout: Duration) -> Option<FileSearchSnapshot> {
        let deadline = Instant::now() + timeout;
        let mut state = self.state.lock().unwrap();
        loop {
            if let Some(snapshot) = state.updates.iter().rev().find(|snapshot| {
                snapshot.query == query
                    && snapshot
                        .matches
                        .iter()
                        .any(|file_match| file_match.path.to_string_lossy().contains(query))
            }) {
                return Some(snapshot.clone());
            }

            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return None;
            }
            let (next_state, wait_result) = self.cv.wait_timeout(state, remaining).unwrap();
            state = next_state;
            if wait_result.timed_out() {
                return state
                    .updates
                    .iter()
                    .rev()
                    .find(|snapshot| {
                        snapshot.query == query
                            && snapshot
                                .matches
                                .iter()
                                .any(|file_match| file_match.path.to_string_lossy().contains(query))
                    })
                    .cloned();
            }
        }
    }

    fn wait_for_completion(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let mut state = self.state.lock().unwrap();
        loop {
            if state.complete_count > 0 {
                return true;
            }

            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return false;
            }
            let (next_state, wait_result) = self.cv.wait_timeout(state, remaining).unwrap();
            state = next_state;
            if wait_result.timed_out() {
                return state.complete_count > 0;
            }
        }
    }
}

impl SessionReporter for RecordingReporter {
    fn on_update(&self, snapshot: &FileSearchSnapshot) {
        let mut state = self.state.lock().unwrap();
        state.updates.push(snapshot.clone());
        self.cv.notify_all();
    }

    fn on_complete(&self) {
        let mut state = self.state.lock().unwrap();
        state.complete_count += 1;
        self.cv.notify_all();
    }
}

#[test]
fn rapid_query_updates_produce_the_final_result_and_completion() {
    const FILE_COUNT: usize = 256;
    const UPDATE_COUNT: usize = 32;

    let root = tempfile::tempdir().unwrap();
    for index in 0..FILE_COUNT {
        let path = root.path().join(format!("file_{index:05}_component.rs"));
        fs::write(path, []).unwrap();
    }

    let reporter = Arc::new(RecordingReporter::default());
    let session = create_session(
        vec![root.path().to_path_buf()],
        FileSearchOptions {
            compute_indices: true,
            ..Default::default()
        },
        reporter.clone(),
        /*cancel_flag*/ None,
    )
    .unwrap();

    let final_query = format!("file_{:05}", (UPDATE_COUNT - 1) * 7);
    for update in 0..UPDATE_COUNT {
        let query = format!("file_{:05}", update * 7);
        session.update_query(&query);
    }

    let snapshot = reporter
        .wait_for_query(&final_query, Duration::from_secs(5))
        .expect("final query should produce a snapshot");
    assert_eq!(snapshot.query, final_query);
    assert!(
        snapshot
            .matches
            .iter()
            .any(|file_match| file_match.path.to_string_lossy().contains(&final_query))
    );
    assert!(reporter.wait_for_completion(Duration::from_secs(5)));
}

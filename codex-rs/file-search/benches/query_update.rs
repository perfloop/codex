use codex_file_search::FileSearchOptions;
use codex_file_search::FileSearchSession;
use codex_file_search::FileSearchSnapshot;
use codex_file_search::MatchType;
use codex_file_search::SessionReporter;
use codex_file_search::create_session;
use divan::Bencher;
use std::fs;
use std::sync::Arc;
use std::sync::Condvar;
use std::sync::Mutex;
use std::time::Duration;
use std::time::Instant;
use tempfile::TempDir;

const DENSE_MATCH_COUNT: usize = 64;
const MATCH_LIMIT: usize = 20;
const FIRST_QUERY: &str = "dense-file";
const SECOND_QUERY: &str = "dense-file-";

fn main() {
    if std::env::args().nth(1).as_deref() == Some("--metadata-probe") {
        let update_count = std::env::args()
            .nth(2)
            .unwrap_or_else(|| panic!("--metadata-probe requires an update count"))
            .parse::<usize>()
            .unwrap_or_else(|error| panic!("invalid metadata probe update count: {error}"));
        if std::env::args().nth(3).is_some() {
            panic!("--metadata-probe accepts exactly one update count");
        }
        run_metadata_probe(update_count);
        return;
    }

    divan::main();
}

#[divan::bench]
fn query_update_snapshot_delivery(bencher: Bencher) {
    let mut state = QueryUpdateBench::new();
    bencher.bench_local(|| state.update_and_validate());
}

fn run_metadata_probe(update_count: usize) {
    let mut state = QueryUpdateBench::new();
    for _ in 0..update_count {
        divan::black_box(state.update_and_validate());
    }
}

struct QueryUpdateBench {
    session: FileSearchSession,
    reporter: Arc<RecordingReporter>,
    _tree: TempDir,
    next_is_first_query: bool,
}

impl QueryUpdateBench {
    fn new() -> Self {
        let tree =
            tempfile::tempdir().unwrap_or_else(|error| panic!("create fixture tree: {error}"));
        for index in 0..DENSE_MATCH_COUNT {
            let path = tree.path().join(format!("dense-file-{index:03}.txt"));
            fs::write(path, format!("fixture {index}"))
                .unwrap_or_else(|error| panic!("write fixture: {error}"));
        }

        let reporter = Arc::new(RecordingReporter::default());
        let options = FileSearchOptions {
            respect_gitignore: false,
            ..FileSearchOptions::default()
        };
        assert_eq!(options.limit.get(), MATCH_LIMIT);
        let session = create_session(
            vec![tree.path().to_path_buf()],
            options,
            reporter.clone(),
            /*cancel_flag*/ None,
        )
        .unwrap_or_else(|error| panic!("create search session: {error}"));

        let update_count = reporter.update_count();
        session.update_query(FIRST_QUERY);
        let snapshot = reporter.wait_for_dense_snapshot(update_count, FIRST_QUERY);
        validate_dense_snapshot(&snapshot, FIRST_QUERY);

        Self {
            session,
            reporter,
            _tree: tree,
            next_is_first_query: false,
        }
    }

    fn update_and_validate(&mut self) -> usize {
        let query = if self.next_is_first_query {
            FIRST_QUERY
        } else {
            SECOND_QUERY
        };
        self.next_is_first_query = !self.next_is_first_query;

        let update_count = self.reporter.update_count();
        self.session.update_query(query);
        let snapshot = self.reporter.wait_for_dense_snapshot(update_count, query);
        validate_dense_snapshot(&snapshot, query);

        snapshot.matches.len() + snapshot.total_match_count + snapshot.scanned_file_count
    }
}

fn validate_dense_snapshot(snapshot: &FileSearchSnapshot, query: &str) {
    assert_eq!(snapshot.query, query);
    assert!(snapshot.walk_complete);
    assert_eq!(snapshot.matches.len(), MATCH_LIMIT);
    assert!(snapshot.total_match_count >= DENSE_MATCH_COUNT);
    assert!(
        snapshot
            .matches
            .iter()
            .all(|file_match| file_match.match_type == MatchType::File)
    );
}

#[derive(Default)]
struct ReporterState {
    updates: Vec<FileSearchSnapshot>,
}

#[derive(Default)]
struct RecordingReporter {
    state: Mutex<ReporterState>,
    updates_ready: Condvar,
}

impl RecordingReporter {
    fn update_count(&self) -> usize {
        self.state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .updates
            .len()
    }

    fn wait_for_dense_snapshot(&self, after: usize, query: &str) -> FileSearchSnapshot {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());

        loop {
            if let Some(snapshot) = state.updates.iter().skip(after).find(|snapshot| {
                snapshot.query == query
                    && snapshot.walk_complete
                    && snapshot.matches.len() == MATCH_LIMIT
                    && snapshot.total_match_count >= DENSE_MATCH_COUNT
            }) {
                return snapshot.clone();
            }

            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                panic!("timed out waiting for dense snapshot for query {query}");
            }
            let (next_state, timeout) = self
                .updates_ready
                .wait_timeout(state, remaining)
                .unwrap_or_else(|error| error.into_inner());
            state = next_state;
            if timeout.timed_out() {
                panic!("timed out waiting for dense snapshot for query {query}");
            }
        }
    }
}

impl SessionReporter for RecordingReporter {
    fn on_update(&self, snapshot: &FileSearchSnapshot) {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        state.updates.push(snapshot.clone());
        self.updates_ready.notify_all();
    }

    fn on_complete(&self) {}
}

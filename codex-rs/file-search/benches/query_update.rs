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
use tempfile::TempDir;

const DENSE_MATCH_COUNT: usize = 64;
const MATCH_LIMIT: usize = 20;
const QUERY_MARKER_LENGTH: usize = 200;

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
    query_generation: usize,
}

impl QueryUpdateBench {
    fn new() -> Self {
        let tree =
            tempfile::tempdir().unwrap_or_else(|error| panic!("create fixture tree: {error}"));
        let marker = "a".repeat(QUERY_MARKER_LENGTH);
        for index in 0..DENSE_MATCH_COUNT {
            let path = tree
                .path()
                .join(format!("dense-{marker}-file-{index:03}.txt"));
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

        let mut state = Self {
            session,
            reporter,
            _tree: tree,
            query_generation: 0,
        };
        divan::black_box(state.update_and_validate());
        state
    }

    fn update_and_validate(&mut self) -> usize {
        self.query_generation += 1;
        assert!(self.query_generation <= QUERY_MARKER_LENGTH);
        let query = format!("dense-{}", "a".repeat(self.query_generation));
        let event_count = self.reporter.event_count();
        self.session.update_query(&query);
        let snapshot = self
            .reporter
            .wait_for_completed_dense_snapshot(event_count, &query);
        validate_dense_snapshot(&snapshot, &query);

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
    events: Vec<ReporterEvent>,
}

enum ReporterEvent {
    Update(FileSearchSnapshot),
    Complete,
}

#[derive(Default)]
struct RecordingReporter {
    state: Mutex<ReporterState>,
    events_ready: Condvar,
}

impl RecordingReporter {
    fn event_count(&self) -> usize {
        self.state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .events
            .len()
    }

    fn wait_for_completed_dense_snapshot(&self, after: usize, query: &str) -> FileSearchSnapshot {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());

        loop {
            let mut matching_snapshot = None;
            for event in state.events.iter().skip(after) {
                match event {
                    ReporterEvent::Update(snapshot)
                        if snapshot.query == query
                            && snapshot.walk_complete
                            && snapshot.matches.len() == MATCH_LIMIT
                            && snapshot.total_match_count >= DENSE_MATCH_COUNT =>
                    {
                        matching_snapshot = Some(snapshot.clone());
                    }
                    ReporterEvent::Complete => {
                        if let Some(snapshot) = matching_snapshot {
                            return snapshot;
                        }
                    }
                    ReporterEvent::Update(_) => {}
                }
            }

            state = self
                .events_ready
                .wait(state)
                .unwrap_or_else(|error| error.into_inner());
        }
    }
}

impl SessionReporter for RecordingReporter {
    fn on_update(&self, snapshot: &FileSearchSnapshot) {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        state.events.push(ReporterEvent::Update(snapshot.clone()));
        self.events_ready.notify_all();
    }

    fn on_complete(&self) {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        state.events.push(ReporterEvent::Complete);
        self.events_ready.notify_all();
    }
}

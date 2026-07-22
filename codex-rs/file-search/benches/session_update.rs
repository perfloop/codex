#![allow(clippy::expect_used)]

use codex_file_search::FileSearchOptions;
use codex_file_search::FileSearchSession;
use codex_file_search::FileSearchSnapshot;
use codex_file_search::MatchType;
use codex_file_search::SessionReporter;
use codex_file_search::create_session;
use divan::Bencher;
use std::env;
use std::fs;
use std::io::Read;
use std::io::Write;
use std::num::NonZero;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Condvar;
use std::sync::Mutex;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;
use tempfile::TempDir;

const MATCH_LIMIT: usize = 20;
const WALK_THREADS: usize = 2;
const WAIT_TIMEOUT: Duration = Duration::from_secs(5);

fn main() {
    let mode = env::args().skip(1).find(|arg| {
        matches!(
            arg.as_str(),
            "--perfloop-probes-json" | "--perfloop-latency-json" | "--perfloop-verify-probe-shape"
        )
    });

    match mode.as_deref() {
        Some("--perfloop-probes-json") => emit_metadata_probe_sample(),
        Some("--perfloop-latency-json") => emit_latency_sample(),
        Some("--perfloop-verify-probe-shape") => verify_metadata_probe_shape(),
        _ => divan::main(),
    }
}

#[divan::bench(sample_count = 20, sample_size = 1)]
fn query_update_after_walk(bencher: Bencher) {
    bencher
        // The walk is setup, while each measured iteration updates a completed session.
        .with_inputs(|| PreparedSession::new(MATCH_LIMIT))
        .bench_local_values(|prepared| {
            let query = next_query();
            let snapshot = prepared.update(query);
            verify_query_snapshot(&snapshot, prepared.limit, query);
            snapshot
        });
}

fn emit_metadata_probe_sample() {
    let measurement = measure_sample(MATCH_LIMIT);
    let metadata_probes = measurement
        .metadata_probes
        .expect("metadata probe counter must be configured");

    println!("{{\"metric\":\"matcher_metadata_probes_per_snapshot\",\"value\":{metadata_probes}}}");
    println!(
        "{{\"metric\":\"snapshot_match_count\",\"value\":{}}}",
        measurement.snapshot_match_count
    );
}

fn emit_latency_sample() {
    let measurement = measure_sample(MATCH_LIMIT);

    println!(
        "{{\"metric\":\"snapshot_delivery_ns\",\"value\":{}}}",
        measurement.snapshot_delivery_ns
    );
    println!(
        "{{\"metric\":\"snapshot_match_count\",\"value\":{}}}",
        measurement.snapshot_match_count
    );
}

fn verify_metadata_probe_shape() {
    let narrow = measure_sample(/*limit*/ 1);
    let dense = measure_sample(MATCH_LIMIT);

    let narrow_probes = narrow
        .metadata_probes
        .expect("metadata probe counter must be configured");
    let dense_probes = dense
        .metadata_probes
        .expect("metadata probe counter must be configured");
    let baseline_shape = narrow_probes >= narrow.snapshot_match_count
        && dense_probes >= dense.snapshot_match_count
        && dense_probes > narrow_probes;
    let carried_type_shape = narrow_probes == 0 && dense_probes == 0;
    assert!(
        baseline_shape || carried_type_shape,
        "metadata probes must either scale with the completed top-N snapshot or be eliminated: limit1={narrow_probes}, limit20={dense_probes}",
    );

    println!("metadata probe shape verified: limit1={narrow_probes}, limit20={dense_probes}");
}

struct Measurement {
    metadata_probes: Option<usize>,
    snapshot_delivery_ns: u128,
    snapshot_match_count: usize,
}

fn measure_sample(limit: usize) -> Measurement {
    let prepared = PreparedSession::new(limit);
    let metadata_probe_counter = metadata_probe_counter_file();
    if let Some(counter_file) = &metadata_probe_counter {
        reset_metadata_probe_counter(counter_file);
    }

    let query = next_query();
    let started = Instant::now();
    let snapshot = prepared.update(query);
    let snapshot_delivery_ns = started.elapsed().as_nanos();
    let metadata_probes = metadata_probe_counter
        .as_deref()
        .map(take_metadata_probe_count);

    verify_query_snapshot(&snapshot, limit, query);
    Measurement {
        metadata_probes,
        snapshot_delivery_ns,
        snapshot_match_count: snapshot.matches.len(),
    }
}

fn metadata_probe_counter_file() -> Option<PathBuf> {
    env::var_os("PERFLOOP_METADATA_COUNTER_FILE").map(PathBuf::from)
}

fn reset_metadata_probe_counter(counter_file: &Path) {
    let mut counter = fs::OpenOptions::new()
        .write(true)
        .open(counter_file)
        .expect("metadata probe counter file");
    counter
        .write_all(&0_u64.to_ne_bytes())
        .expect("reset metadata probe counter");
    counter.flush().expect("flush metadata probe counter");
}

fn take_metadata_probe_count(counter_file: &Path) -> usize {
    let mut counter = fs::File::open(counter_file).expect("open metadata probe counter");
    let mut bytes = [0; std::mem::size_of::<u64>()];
    counter
        .read_exact(&mut bytes)
        .expect("read metadata probe counter");
    usize::try_from(u64::from_ne_bytes(bytes)).expect("metadata probe count fits usize")
}

#[derive(Clone, Copy)]
enum Query {
    Files,
    Directories,
}

impl Query {
    fn text(self) -> &'static str {
        match self {
            Self::Files => "dense-file",
            Self::Directories => "dense-directory",
        }
    }

    fn expected_match_type(self) -> MatchType {
        match self {
            Self::Files => MatchType::File,
            Self::Directories => MatchType::Directory,
        }
    }
}

fn next_query() -> Query {
    static NEXT_QUERY: AtomicUsize = AtomicUsize::new(0);

    match NEXT_QUERY.fetch_add(1, Ordering::Relaxed) % 2 {
        0 => Query::Files,
        _ => Query::Directories,
    }
}

struct PreparedSession {
    session: FileSearchSession,
    reporter: Arc<RecordingReporter>,
    _tree: TempDir,
    limit: usize,
}

impl PreparedSession {
    fn new(limit: usize) -> Self {
        let tree = tempfile::tempdir().expect("benchmark tree");
        create_dense_tree(tree.path());

        let reporter = Arc::new(RecordingReporter::default());
        let session = create_session(
            vec![tree.path().to_path_buf()],
            FileSearchOptions {
                limit: NonZero::new(limit).expect("positive benchmark limit"),
                exclude: Vec::new(),
                threads: NonZero::new(WALK_THREADS).expect("positive walker thread count"),
                compute_indices: true,
                respect_gitignore: true,
            },
            reporter.clone(),
            /*cancel_flag*/ None,
        )
        .expect("benchmark session");

        session.update_query("dense");
        let initial_snapshot = reporter.wait_for_initial_complete();
        verify_initial_snapshot(&initial_snapshot, limit);

        Self {
            session,
            reporter,
            _tree: tree,
            limit,
        }
    }

    fn update(&self, query: Query) -> FileSearchSnapshot {
        let update_count = self.reporter.update_count();
        let completion_count = self.reporter.completion_count();
        self.session.update_query(query.text());

        let updates = self
            .reporter
            .wait_for_completed_updates(update_count, completion_count);
        assert_eq!(updates.len(), 1, "one completed snapshot per query update");

        let snapshot = updates.into_iter().next().expect("completed update");
        assert_eq!(snapshot.query, query.text());
        snapshot
    }
}

#[derive(Default)]
struct RecordingReporter {
    state: Mutex<ReporterState>,
    changed: Condvar,
}

#[derive(Default)]
struct ReporterState {
    updates: Vec<FileSearchSnapshot>,
    completions: usize,
}

impl RecordingReporter {
    fn update_count(&self) -> usize {
        self.state.lock().expect("reporter state").updates.len()
    }

    fn completion_count(&self) -> usize {
        self.state.lock().expect("reporter state").completions
    }

    fn wait_for_initial_complete(&self) -> FileSearchSnapshot {
        let deadline = Instant::now() + WAIT_TIMEOUT;
        let mut state = self.state.lock().expect("reporter state");
        loop {
            if state.completions > 0 {
                return state.updates.last().cloned().expect("initial snapshot");
            }
            state = wait_for_reporter_change(&self.changed, state, deadline);
        }
    }

    fn wait_for_completed_updates(
        &self,
        update_count: usize,
        completion_count: usize,
    ) -> Vec<FileSearchSnapshot> {
        let deadline = Instant::now() + WAIT_TIMEOUT;
        let mut state = self.state.lock().expect("reporter state");
        loop {
            if state.completions > completion_count {
                return state.updates[update_count..].to_vec();
            }
            state = wait_for_reporter_change(&self.changed, state, deadline);
        }
    }
}

fn wait_for_reporter_change<'a>(
    changed: &Condvar,
    state: std::sync::MutexGuard<'a, ReporterState>,
    deadline: Instant,
) -> std::sync::MutexGuard<'a, ReporterState> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    assert!(
        !remaining.is_zero(),
        "timed out waiting for file-search update"
    );

    let (state, timeout) = changed
        .wait_timeout(state, remaining)
        .expect("reporter state");
    assert!(
        !timeout.timed_out(),
        "timed out waiting for file-search update"
    );
    state
}

impl SessionReporter for RecordingReporter {
    fn on_update(&self, snapshot: &FileSearchSnapshot) {
        let mut state = self.state.lock().expect("reporter state");
        state.updates.push(snapshot.clone());
        self.changed.notify_all();
    }

    fn on_complete(&self) {
        let mut state = self.state.lock().expect("reporter state");
        state.completions += 1;
        self.changed.notify_all();
    }
}

fn create_dense_tree(root: &Path) {
    for index in 0..MATCH_LIMIT {
        fs::write(root.join(format!("dense-file-{index:02}.txt")), "fixture")
            .expect("benchmark file");
        fs::create_dir(root.join(format!("dense-directory-{index:02}")))
            .expect("benchmark directory");
    }
}

fn verify_initial_snapshot(snapshot: &FileSearchSnapshot, limit: usize) {
    assert_eq!(snapshot.matches.len(), limit);
    assert!(snapshot.total_match_count >= limit);

    for file_match in &snapshot.matches {
        let name = file_match
            .path
            .file_name()
            .and_then(|name| name.to_str())
            .expect("ASCII benchmark path");
        let expected_match_type = if name.starts_with("dense-file-") {
            MatchType::File
        } else if name.starts_with("dense-directory-") {
            MatchType::Directory
        } else {
            panic!("unexpected benchmark path: {name}");
        };
        assert_eq!(file_match.match_type, expected_match_type);
    }
}

fn verify_query_snapshot(snapshot: &FileSearchSnapshot, limit: usize, query: Query) {
    assert_eq!(snapshot.matches.len(), limit);
    assert_eq!(snapshot.total_match_count, MATCH_LIMIT);
    assert!(snapshot.matches.iter().all(|file_match| {
        file_match.path.to_string_lossy().starts_with(query.text())
            && file_match.match_type == query.expected_match_type()
    }));
}

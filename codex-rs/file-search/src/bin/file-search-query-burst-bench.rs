#![allow(clippy::unwrap_used)]

use codex_file_search::BenchQueueMetrics;
use codex_file_search::FileSearchOptions;
use codex_file_search::FileSearchSnapshot;
use codex_file_search::SessionReporter;
use codex_file_search::create_session_with_bench_metrics;
use std::fs;
use std::num::NonZero;
use std::sync::Arc;
use std::sync::Condvar;
use std::sync::Mutex;
use std::thread;
use std::time::Duration;
use std::time::Instant;

const FILE_COUNT: usize = 8_192;
const UPDATE_COUNT: usize = 64;
const BURSTS_PER_SAMPLE: usize = 48;
const TYPED_CHAR_CADENCE: Duration = Duration::from_millis(9);
const WARMUP_QUERY: &str = "zz";

fn main() {
    if let Err(error) = run() {
        eprintln!("file-search typed-query benchmark failed: {error:#}");
        std::process::exit(1);
    }
}

fn run() -> anyhow::Result<()> {
    let queries = typed_query_tokens();
    let fixture = tempfile::Builder::new()
        .prefix(".file-search-typed-")
        .tempdir_in(std::env::current_dir()?)?;
    let final_query = queries
        .last()
        .ok_or_else(|| anyhow::anyhow!("fixture requires a final query"))?;
    for (index, query) in queries.iter().enumerate() {
        fs::write(
            fixture
                .path()
                .join(format!("{query}_priority_{index:04}.rs")),
            b"",
        )?;
    }
    for index in queries.len()..FILE_COUNT {
        fs::write(
            fixture
                .path()
                .join(format!("fixture_{final_query}_{index:05}.rs")),
            b"",
        )?;
    }
    let final_file = format!("{final_query}_priority_{:04}.rs", queries.len() - 1);

    let metrics = Arc::new(BenchQueueMetrics::default());
    let reporter = Arc::new(BurstReporter::new(metrics.clone()));
    let session = create_session_with_bench_metrics(
        vec![fixture.path().to_path_buf()],
        FileSearchOptions {
            limit: NonZero::new(20).unwrap(),
            exclude: Vec::new(),
            threads: NonZero::new(2).unwrap(),
            compute_indices: true,
            respect_gitignore: false,
        },
        reporter.clone(),
        metrics.clone(),
        None,
    )?;

    reporter.wait_for_initial_completion();
    let update_id = session.update_query_with_id(WARMUP_QUERY);
    let warmup = reporter.wait_for_update(update_id, WARMUP_QUERY);
    anyhow::ensure!(
        warmup.snapshot.scanned_file_count >= FILE_COUNT,
        "warmup scanned only {} of {FILE_COUNT} fixture files",
        warmup.snapshot.scanned_file_count
    );
    reporter.wait_for_completion_after(warmup.sequence);

    metrics.reset();
    reporter.reset();
    let mut callback_latency_ns = 0_u128;
    for _ in 0..BURSTS_PER_SAMPLE {
        callback_latency_ns += u128::from(run_typed_query_burst(
            &session,
            &queries,
            &reporter,
            &final_file,
        )?);
    }
    let (peak_depth, mean_depth, p99_age, resolved, logical) = metrics.stats();
    anyhow::ensure!(
        logical == (UPDATE_COUNT * BURSTS_PER_SAMPLE) as u64,
        "recorded {logical} logical updates"
    );
    let (completions, stale, same_text_stale) = reporter.outcomes();
    for (metric, value) in [
        (
            "newest_query_callback_latency_ns",
            (callback_latency_ns / BURSTS_PER_SAMPLE as u128) as u64,
        ),
        ("query_signal_peak_depth", peak_depth),
        ("query_signal_enqueue_mean_depth_milli", mean_depth),
        ("query_signal_queue_age_p99_ns", p99_age),
        ("query_signal_resolved_count", resolved),
        ("logical_query_update_count", logical),
        ("session_completion_count", completions),
        ("stale_snapshot_count", stale),
        ("same_text_stale_snapshot_count", same_text_stale),
        ("final_callback_verified", 1),
    ] {
        emit(metric, value);
    }
    Ok(())
}

fn run_typed_query_burst(
    session: &codex_file_search::FileSearchSession,
    queries: &[String],
    reporter: &BurstReporter,
    expected_file: &str,
) -> anyhow::Result<u64> {
    let mut due = Instant::now();
    let mut final_update = None;
    for (index, query) in queries.iter().enumerate() {
        if index != 0 {
            due += TYPED_CHAR_CADENCE;
            thread::sleep(due.saturating_duration_since(Instant::now()));
        }
        let sent_at = Instant::now();
        let update_id = session.update_query_with_id(query);
        if index + 1 == queries.len() {
            final_update = Some((update_id, query.as_str(), sent_at));
        }
    }
    let (update_id, query, sent_at) =
        final_update.ok_or_else(|| anyhow::anyhow!("burst had no final query"))?;
    let callback = reporter.wait_for_final_update(update_id, query, expected_file);
    reporter.wait_for_completion_after(callback.sequence);
    Ok(callback
        .observed_at
        .saturating_duration_since(sent_at)
        .as_nanos()
        .min(u128::from(u64::MAX)) as u64)
}

fn typed_query_tokens() -> Vec<String> {
    let mut prefix = String::new();
    (0..UPDATE_COUNT)
        .map(|index| {
            prefix.push(char::from(b'a' + (index % 26) as u8));
            format!("{prefix}{WARMUP_QUERY}")
        })
        .collect()
}

fn emit(metric: &str, value: u64) {
    println!(r#"{{"metric":"{metric}","value":{value}}}"#);
}

struct BurstReporter {
    metrics: Arc<BenchQueueMetrics>,
    state: Mutex<ReporterState>,
    changed: Condvar,
}

#[derive(Default)]
struct ReporterState {
    updates: Vec<ObservedUpdate>,
    completions: Vec<u64>,
    sequence: u64,
    stale: u64,
    same_text_stale: u64,
}

#[derive(Clone)]
struct ObservedUpdate {
    snapshot: FileSearchSnapshot,
    observed_at: Instant,
    sequence: u64,
}

impl BurstReporter {
    fn new(metrics: Arc<BenchQueueMetrics>) -> Self {
        Self {
            metrics,
            state: Mutex::new(ReporterState::default()),
            changed: Condvar::new(),
        }
    }

    fn reset(&self) {
        *self.lock() = ReporterState::default();
    }

    fn wait_for_initial_completion(&self) {
        self.wait(|state| (!state.completions.is_empty()).then_some(()));
    }

    fn wait_for_update(&self, update_id: u64, query: &str) -> ObservedUpdate {
        self.wait(|state| {
            state
                .updates
                .iter()
                .find(|update| {
                    update.snapshot.update_id == update_id && update.snapshot.query == query
                })
                .cloned()
        })
    }

    fn wait_for_final_update(
        &self,
        update_id: u64,
        query: &str,
        expected_file: &str,
    ) -> ObservedUpdate {
        self.wait(|state| {
            state
                .updates
                .iter()
                .find(|update| {
                    update.snapshot.update_id == update_id
                        && update.snapshot.query == query
                        && snapshot_contains_file(&update.snapshot, expected_file)
                })
                .cloned()
        })
    }

    fn wait_for_completion_after(&self, update_sequence: u64) {
        self.wait(|state| {
            state
                .completions
                .iter()
                .any(|completion| *completion > update_sequence)
                .then_some(())
        });
    }

    fn outcomes(&self) -> (u64, u64, u64) {
        let state = self.lock();
        (
            state.completions.len() as u64,
            state.stale,
            state.same_text_stale,
        )
    }

    fn wait<T>(&self, predicate: impl Fn(&ReporterState) -> Option<T>) -> T {
        let mut state = self.lock();
        loop {
            if let Some(value) = predicate(&state) {
                return value;
            }
            state = self
                .changed
                .wait(state)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, ReporterState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl SessionReporter for BurstReporter {
    fn on_update(&self, snapshot: &FileSearchSnapshot) {
        let observed_at = Instant::now();
        let (latest_id, latest_query) = self.metrics.latest();
        let stale = snapshot.update_id != latest_id;
        let mut state = self.lock();
        state.sequence += 1;
        let sequence = state.sequence;
        state.stale += stale as u64;
        state.same_text_stale += (stale && snapshot.query == latest_query) as u64;
        state.updates.push(ObservedUpdate {
            snapshot: snapshot.clone(),
            observed_at,
            sequence,
        });
        self.changed.notify_all();
    }

    fn on_complete(&self) {
        let mut state = self.lock();
        state.sequence += 1;
        let sequence = state.sequence;
        state.completions.push(sequence);
        self.changed.notify_all();
    }
}

fn snapshot_contains_file(snapshot: &FileSearchSnapshot, expected_file: &str) -> bool {
    snapshot.matches.iter().any(|file_match| {
        file_match
            .path
            .file_name()
            .is_some_and(|name| name == expected_file)
    })
}

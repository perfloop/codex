#![allow(clippy::unwrap_used)]

use anyhow::Context;
use codex_file_search::BenchQueueMetrics;
use codex_file_search::FileSearchOptions;
use codex_file_search::FileSearchSnapshot;
use codex_file_search::SessionReporter;
use codex_file_search::create_session_with_bench_metrics;
use std::fs;
use std::num::NonZero;
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

const FILE_COUNT: usize = 8_192;
const UPDATE_COUNT: usize = 64;
// One sample averages repeated identity-checked typed bursts before emitting
// the supporting newest-query callback-latency observation.
const BURSTS_PER_SAMPLE: usize = 48;
// PasteBurst treats an 8-ms-or-shorter plain-character interval as a paste. The
// TUI's own human-input helper sleeps its recommended 8-ms-plus-1-ms delay and
// flushes after every character, so this is the source-backed non-burst cadence.
const TYPED_CHAR_CADENCE: Duration = Duration::from_millis(9);
const CALLBACK_TIMEOUT: Duration = Duration::from_secs(30);
const WARMUP_QUERY: &str = "zz";
static NEXT_FIXTURE_ID: AtomicU64 = AtomicU64::new(0);

fn main() {
    if let Err(error) = run() {
        eprintln!("file-search typed-query benchmark failed: {error:#}");
        std::process::exit(1);
    }
}

fn run() -> anyhow::Result<()> {
    let queries = typed_query_tokens();
    let fixture = Fixture::create(&queries)?;
    let queue_metrics = Arc::new(BenchQueueMetrics::default());
    let reporter = Arc::new(BurstReporter::new(queue_metrics.clone()));
    let limit = NonZero::new(20).ok_or_else(|| anyhow::anyhow!("nonzero result limit"))?;
    let threads = NonZero::new(2).ok_or_else(|| anyhow::anyhow!("nonzero matcher threads"))?;
    let session = create_session_with_bench_metrics(
        vec![fixture.directory.path().to_path_buf()],
        FileSearchOptions {
            limit,
            exclude: Vec::new(),
            threads,
            compute_indices: true,
            respect_gitignore: false,
        },
        reporter.clone(),
        queue_metrics.clone(),
        None,
    )?;

    // Let the walker and matcher reach an idle, fully populated state before the
    // typed-query control begins. This keeps the measured burst focused on
    // superseded query work rather than initial discovery.
    reporter.wait_for_initial_completion(CALLBACK_TIMEOUT)?;
    let warmup_sent_at = Instant::now();
    let warmup_id = session.update_query_with_id(WARMUP_QUERY);
    let warmup =
        reporter.wait_for_update(warmup_id, WARMUP_QUERY, warmup_sent_at, CALLBACK_TIMEOUT)?;
    if warmup.snapshot.scanned_file_count < FILE_COUNT {
        anyhow::bail!(
            "warmup scanned only {} of {FILE_COUNT} fixture files",
            warmup.snapshot.scanned_file_count
        );
    }
    reporter.wait_for_completion_after(warmup.observed_at, CALLBACK_TIMEOUT)?;

    queue_metrics.reset();
    reporter.reset();

    let mut total_newest_query_callback_latency_ns = 0_u128;
    for _ in 0..BURSTS_PER_SAMPLE {
        total_newest_query_callback_latency_ns += u128::from(run_typed_query_burst(
            &session,
            &queries,
            &reporter,
            &fixture.final_result_file,
        )?);
    }
    let newest_query_callback_latency_ns = (total_newest_query_callback_latency_ns
        / BURSTS_PER_SAMPLE as u128)
        .min(u128::from(u64::MAX)) as u64;
    let queue_stats = queue_metrics.queue_stats();
    let expected_logical_update_count = UPDATE_COUNT * BURSTS_PER_SAMPLE;
    if queue_stats.logical_update_count != expected_logical_update_count {
        anyhow::bail!(
            "recorded {} logical updates; expected {expected_logical_update_count}",
            queue_stats.logical_update_count
        );
    }
    let outcomes = reporter.outcomes();

    emit(
        "newest_query_callback_latency_ns",
        newest_query_callback_latency_ns,
    );
    emit(
        "query_signal_peak_depth",
        usize_to_u64(queue_stats.peak_depth),
    );
    emit(
        "query_signal_enqueue_mean_depth_milli",
        queue_stats.enqueued_mean_depth_milli,
    );
    emit("query_signal_queue_age_p99_ns", queue_stats.p99_age_ns);
    emit(
        "query_signal_resolved_count",
        usize_to_u64(queue_stats.resolved_query_count),
    );
    emit(
        "logical_query_update_count",
        usize_to_u64(queue_stats.logical_update_count),
    );
    emit(
        "session_completion_count",
        usize_to_u64(outcomes.completion_count),
    );
    emit(
        "worker_completion_count",
        usize_to_u64(queue_stats.completion_count),
    );
    emit(
        "stale_snapshot_count",
        usize_to_u64(outcomes.stale_snapshot_count),
    );
    emit(
        "same_text_stale_snapshot_count",
        usize_to_u64(outcomes.same_text_stale_snapshot_count),
    );
    emit("final_callback_verified", 1);

    Ok(())
}

fn run_typed_query_burst(
    session: &codex_file_search::FileSearchSession,
    queries: &[String],
    reporter: &BurstReporter,
    expected_file: &str,
) -> anyhow::Result<u64> {
    let mut next_send_at = Instant::now();
    let mut final_update = None;
    for (index, query) in queries.iter().enumerate() {
        if index != 0 {
            next_send_at += TYPED_CHAR_CADENCE;
            let remaining = next_send_at.saturating_duration_since(Instant::now());
            if !remaining.is_zero() {
                thread::sleep(remaining);
            }
        }

        let sent_at = Instant::now();
        let update_id = session.update_query_with_id(query);
        if index + 1 == queries.len() {
            final_update = Some((update_id, query.as_str(), sent_at));
        }
    }

    let (final_update_id, final_query, final_sent_at) =
        final_update.ok_or_else(|| anyhow::anyhow!("burst had no final query"))?;
    let final_callback = reporter.wait_for_final_update(
        final_update_id,
        final_query,
        final_sent_at,
        expected_file,
        CALLBACK_TIMEOUT,
    )?;
    reporter.wait_for_completion_after(final_callback.observed_at, CALLBACK_TIMEOUT)?;

    Ok(final_callback
        .observed_at
        .saturating_duration_since(final_sent_at)
        .as_nanos()
        .min(u128::from(u64::MAX)) as u64)
}

// These are exactly the distinct `@`-token values produced by ordinary
// typing at a cursor immediately before an existing `zz` suffix: `azz`,
// `abzz`, ... . They deliberately are not append-only matcher inputs, which
// exercises the edit path rather than an incremental append fast path. Every
// generic fixture path contains the final value, so each query still exercises
// the broad 8,192-file matcher workload.
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
    println!(
        "{}",
        serde_json::json!({ "metric": metric, "value": value })
    );
}

fn usize_to_u64(value: usize) -> u64 {
    value.try_into().unwrap_or(u64::MAX)
}

struct Fixture {
    directory: FixtureDir,
    final_result_file: String,
}

impl Fixture {
    fn create(queries: &[String]) -> anyhow::Result<Self> {
        let directory = FixtureDir::new()?;
        let final_query = queries
            .last()
            .ok_or_else(|| anyhow::anyhow!("fixture requires a final query"))?;

        for (index, query) in queries.iter().enumerate() {
            fs::write(
                directory
                    .path()
                    .join(format!("{query}_priority_{index:04}.rs")),
                b"",
            )
            .with_context(|| format!("create priority fixture for query {query}"))?;
        }
        for index in queries.len()..FILE_COUNT {
            fs::write(
                directory
                    .path()
                    .join(format!("fixture_{final_query}_{index:05}.rs")),
                b"",
            )
            .with_context(|| format!("create generic fixture {index}"))?;
        }

        Ok(Self {
            directory,
            final_result_file: format!("{final_query}_priority_{:04}.rs", queries.len() - 1),
        })
    }
}

struct FixtureDir {
    path: PathBuf,
}

impl FixtureDir {
    fn new() -> anyhow::Result<Self> {
        let root = std::env::current_dir().context("resolve benchmark working directory")?;
        let id = NEXT_FIXTURE_ID.fetch_add(1, Ordering::Relaxed);
        let path = root.join(format!(
            ".file-search-query-burst-fixture-{}-{id}",
            std::process::id()
        ));
        fs::create_dir(&path)
            .with_context(|| format!("create fixture directory {}", path.display()))?;
        Ok(Self { path })
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

struct BurstReporter {
    queue_metrics: Arc<BenchQueueMetrics>,
    state: Mutex<ReporterState>,
    changed: Condvar,
}

#[derive(Default)]
struct ReporterState {
    updates: Vec<ObservedUpdate>,
    completions: Vec<Instant>,
    stale_snapshot_count: usize,
    same_text_stale_snapshot_count: usize,
}

#[derive(Clone)]
struct ObservedUpdate {
    snapshot: FileSearchSnapshot,
    observed_at: Instant,
}

struct ReporterOutcomes {
    completion_count: usize,
    stale_snapshot_count: usize,
    same_text_stale_snapshot_count: usize,
}

impl BurstReporter {
    fn new(queue_metrics: Arc<BenchQueueMetrics>) -> Self {
        Self {
            queue_metrics,
            state: Mutex::new(ReporterState::default()),
            changed: Condvar::new(),
        }
    }

    fn reset(&self) {
        *self.lock_state() = ReporterState::default();
    }

    fn wait_for_initial_completion(&self, timeout: Duration) -> anyhow::Result<()> {
        let deadline = Instant::now() + timeout;
        let mut state = self.lock_state();
        loop {
            if !state.completions.is_empty() {
                return Ok(());
            }
            state = self.wait_for_change(state, deadline)?;
        }
    }

    fn wait_for_update(
        &self,
        update_id: u64,
        query: &str,
        sent_at: Instant,
        timeout: Duration,
    ) -> anyhow::Result<ObservedUpdate> {
        let deadline = Instant::now() + timeout;
        let mut state = self.lock_state();
        loop {
            if let Some(update) = state.updates.iter().find(|update| {
                update.snapshot.update_id == update_id
                    && update.snapshot.query == query
                    && update.observed_at >= sent_at
            }) {
                return Ok(update.clone());
            }
            state = self.wait_for_change(state, deadline)?;
        }
    }

    fn wait_for_final_update(
        &self,
        update_id: u64,
        query: &str,
        sent_at: Instant,
        expected_file: &str,
        timeout: Duration,
    ) -> anyhow::Result<ObservedUpdate> {
        let deadline = Instant::now() + timeout;
        let mut state = self.lock_state();
        loop {
            if let Some(update) = state.updates.iter().find(|update| {
                update.snapshot.update_id == update_id
                    && update.snapshot.query == query
                    && update.observed_at >= sent_at
                    && snapshot_contains_file(&update.snapshot, expected_file)
            }) {
                return Ok(update.clone());
            }
            state = self.wait_for_change(state, deadline)?;
        }
    }

    fn wait_for_completion_after(
        &self,
        observed_at: Instant,
        timeout: Duration,
    ) -> anyhow::Result<()> {
        let deadline = Instant::now() + timeout;
        let mut state = self.lock_state();
        loop {
            if state
                .completions
                .iter()
                .any(|completion_at| *completion_at >= observed_at)
            {
                return Ok(());
            }
            state = self.wait_for_change(state, deadline)?;
        }
    }

    fn outcomes(&self) -> ReporterOutcomes {
        let state = self.lock_state();
        ReporterOutcomes {
            completion_count: state.completions.len(),
            stale_snapshot_count: state.stale_snapshot_count,
            same_text_stale_snapshot_count: state.same_text_stale_snapshot_count,
        }
    }

    fn wait_for_change<'a>(
        &self,
        state: std::sync::MutexGuard<'a, ReporterState>,
        deadline: Instant,
    ) -> anyhow::Result<std::sync::MutexGuard<'a, ReporterState>> {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            anyhow::bail!("timed out waiting for the file-search session");
        }
        let (state, _) = self
            .changed
            .wait_timeout(state, remaining)
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        Ok(state)
    }

    fn lock_state(&self) -> std::sync::MutexGuard<'_, ReporterState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl SessionReporter for BurstReporter {
    fn on_update(&self, snapshot: &FileSearchSnapshot) {
        let observed_at = Instant::now();
        let (latest_update_id, latest_query) = self.queue_metrics.latest_query_identity();
        let stale = snapshot.update_id != latest_update_id;
        let same_text_stale = stale && snapshot.query == latest_query;

        let mut state = self.lock_state();
        if stale {
            state.stale_snapshot_count += 1;
        }
        if same_text_stale {
            state.same_text_stale_snapshot_count += 1;
        }
        state.updates.push(ObservedUpdate {
            snapshot: snapshot.clone(),
            observed_at,
        });
        self.changed.notify_all();
    }

    fn on_complete(&self) {
        self.lock_state().completions.push(Instant::now());
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

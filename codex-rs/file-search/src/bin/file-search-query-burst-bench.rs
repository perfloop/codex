//! Measures the callback latency for the newest query during rapid file-search edits.
//!
//! The fixture is intentionally larger than the result limit and each burst changes
//! a non-prefix portion of the query. That keeps the measured path on the matcher
//! work rather than treating an appended character as an already-computed result.

use codex_file_search::FileSearchOptions;
use codex_file_search::FileSearchSnapshot;
use codex_file_search::SessionReporter;
use codex_file_search::create_session;
use std::error::Error;
use std::fs;
use std::io;
use std::num::NonZero;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Condvar;
use std::sync::Mutex;
use std::thread;
use std::time::Duration;
use std::time::Instant;
use std::time::SystemTime;

const FILE_COUNT: usize = 8_192;
const QUERY_VARIANT_COUNT: usize = 16;
const BURST_COUNT: usize = 100;
const UPDATES_PER_BURST: usize = 64;
const UPDATE_CADENCE: Duration = Duration::from_millis(2);
const TIMEOUT: Duration = Duration::from_secs(20);

struct Fixture {
    path: PathBuf,
}

impl Fixture {
    fn create() -> io::Result<Self> {
        let nonce = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_err(io::Error::other)?
            .as_nanos();
        let path = std::env::current_dir()?.join("target").join(format!(
            "file-search-query-burst-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir_all(&path)?;

        for index in 0..FILE_COUNT {
            let bucket = path.join(format!("bucket-{}", index % 64));
            fs::create_dir_all(&bucket)?;
            fs::write(
                bucket.join(format!(
                    "file_{index:05}_needle_00_needle_01_needle_02_needle_03_needle_04_needle_05_needle_06_needle_07_needle_08_needle_09_needle_10_needle_11_needle_12_needle_13_needle_14_needle_15_component.rs"
                )),
                [],
            )?;
        }

        Ok(Self { path })
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

#[derive(Default)]
struct BurstReporter {
    state: Mutex<ReporterState>,
    cv: Condvar,
}

#[derive(Default)]
struct ReporterState {
    updates: Vec<TimedSnapshot>,
    completions: Vec<Instant>,
}

#[derive(Clone)]
struct TimedSnapshot {
    snapshot: FileSearchSnapshot,
    received_at: Instant,
}

impl BurstReporter {
    fn wait_for_query_after(
        &self,
        query: &str,
        after: Instant,
        timeout: Duration,
    ) -> Option<TimedSnapshot> {
        let deadline = Instant::now() + timeout;
        let mut state = self.state.lock().expect("reporter state should not poison");
        loop {
            if let Some(snapshot) =
                state.updates.iter().rev().find(|snapshot| {
                    snapshot.received_at >= after && snapshot.snapshot.query == query
                })
            {
                return Some(snapshot.clone());
            }

            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return None;
            }
            let (next_state, wait_result) = self
                .cv
                .wait_timeout(state, remaining)
                .expect("reporter state should not poison");
            state = next_state;
            if wait_result.timed_out() {
                return state
                    .updates
                    .iter()
                    .rev()
                    .find(|snapshot| {
                        snapshot.received_at >= after && snapshot.snapshot.query == query
                    })
                    .cloned();
            }
        }
    }

    fn wait_for_completion_after(&self, after: Instant, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let mut state = self.state.lock().expect("reporter state should not poison");
        loop {
            if state
                .completions
                .iter()
                .any(|completed_at| *completed_at >= after)
            {
                return true;
            }

            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return false;
            }
            let (next_state, wait_result) = self
                .cv
                .wait_timeout(state, remaining)
                .expect("reporter state should not poison");
            state = next_state;
            if wait_result.timed_out() {
                return state
                    .completions
                    .iter()
                    .any(|completed_at| *completed_at >= after);
            }
        }
    }

    fn stale_snapshot_count_since(&self, start: Instant, current_query: &str) -> usize {
        self.state
            .lock()
            .expect("reporter state should not poison")
            .updates
            .iter()
            .filter(|snapshot| {
                snapshot.received_at >= start && snapshot.snapshot.query != current_query
            })
            .count()
    }
}

impl SessionReporter for BurstReporter {
    fn on_update(&self, snapshot: &FileSearchSnapshot) {
        let mut state = self.state.lock().expect("reporter state should not poison");
        state.updates.push(TimedSnapshot {
            snapshot: snapshot.clone(),
            received_at: Instant::now(),
        });
        self.cv.notify_all();
    }

    fn on_complete(&self) {
        let mut state = self.state.lock().expect("reporter state should not poison");
        state.completions.push(Instant::now());
        self.cv.notify_all();
    }
}

fn query_for(round: usize, update: usize) -> String {
    let variant = (round * UPDATES_PER_BURST + update) % QUERY_VARIANT_COUNT;
    format!("needle_{variant:02}")
}

fn p99(mut latencies: Vec<u128>) -> u128 {
    latencies.sort_unstable();
    let percentile_index = (latencies.len() * 99).div_ceil(100) - 1;
    latencies[percentile_index]
}

fn run() -> Result<(), Box<dyn Error>> {
    let fixture = Fixture::create()?;
    let reporter = Arc::new(BurstReporter::default());
    let session = create_session(
        vec![fixture.path.clone()],
        FileSearchOptions {
            limit: NonZero::try_from(20)?,
            exclude: Vec::new(),
            threads: NonZero::try_from(2)?,
            compute_indices: true,
            respect_gitignore: true,
        },
        reporter.clone(),
        /*cancel_flag*/ None,
    )?;

    let warm_query = query_for(BURST_COUNT + 1, 0);
    let warm_started_at = Instant::now();
    session.update_query(&warm_query);
    reporter
        .wait_for_query_after(&warm_query, warm_started_at, TIMEOUT)
        .ok_or_else(|| io::Error::new(io::ErrorKind::TimedOut, "warm query did not finish"))?;
    if !reporter.wait_for_completion_after(warm_started_at, TIMEOUT) {
        return Err(io::Error::new(io::ErrorKind::TimedOut, "warm query did not complete").into());
    }

    let mut latest_query_latencies = Vec::with_capacity(BURST_COUNT);
    let mut stale_snapshot_count = 0;
    for round in 0..BURST_COUNT {
        let burst_started_at = Instant::now();
        let final_query = query_for(round, UPDATES_PER_BURST - 1);
        let mut final_query_sent_at = burst_started_at;

        for update in 0..UPDATES_PER_BURST {
            let query = query_for(round, update);
            if update + 1 == UPDATES_PER_BURST {
                final_query_sent_at = Instant::now();
            }
            session.update_query(&query);
            if update + 1 != UPDATES_PER_BURST {
                thread::sleep(UPDATE_CADENCE);
            }
        }

        let final_snapshot = reporter
            .wait_for_query_after(&final_query, final_query_sent_at, TIMEOUT)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::TimedOut,
                    "latest query did not produce a snapshot",
                )
            })?;
        if !final_snapshot
            .snapshot
            .matches
            .iter()
            .any(|file_match| file_match.path.to_string_lossy().contains(&final_query))
        {
            return Err(io::Error::other("latest query did not return its matching file").into());
        }
        std::hint::black_box(&final_snapshot.snapshot.matches);
        latest_query_latencies.push(
            final_snapshot
                .received_at
                .duration_since(final_query_sent_at)
                .as_nanos(),
        );
        if !reporter.wait_for_completion_after(final_snapshot.received_at, TIMEOUT) {
            return Err(
                io::Error::new(io::ErrorKind::TimedOut, "latest query did not complete").into(),
            );
        }
        stale_snapshot_count += reporter.stale_snapshot_count_since(burst_started_at, &final_query);
    }
    drop(session);

    println!(
        "{{\"metric\":\"p99_latest_query_result_latency_ns\",\"value\":{}}}",
        p99(latest_query_latencies)
    );
    println!("{{\"metric\":\"stale_snapshot_count\",\"value\":{stale_snapshot_count}}}");
    Ok(())
}

fn main() {
    if let Err(error) = run() {
        eprintln!("file-search query burst benchmark failed: {error}");
        std::process::exit(1);
    }
}

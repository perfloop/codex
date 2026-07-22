//! Focused streaming-render proof workload.
//!
//! The workload feeds newline-bearing chunks into the same `StreamController`
//! used by agent-message streaming. By default every eight chunks it advances
//! the existing commit-tick boundary, matching the TUI's bounded presentation cadence while
//! retaining a single mutable paragraph, unclosed fence, or table tail.

use super::controller::StreamController;
use super::render::reset_streaming_render_stats;
use super::render::streaming_render_stats;
use crate::history_cell::HistoryCell;
use crate::history_cell::HistoryRenderMode;
use crate::markdown::append_markdown_agent;
use crate::terminal_hyperlinks::visible_lines;
use ratatui::text::Line;
use std::hint::black_box;
use std::path::Path;
use std::path::PathBuf;
use std::time::Instant;

const DEFAULT_PRESENTATION_CHUNKS: usize = 8;
const CHUNK_COUNTS: [usize; 3] = [10, 100, 1_000];
const RENDER_WIDTH: usize = 80;

fn presentation_chunks() -> usize {
    std::env::var("PERFLOOP_PRESENTATION_CHUNKS")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|&value| value > 0)
        .unwrap_or(DEFAULT_PRESENTATION_CHUNKS)
}

struct Fixture {
    kind: &'static str,
    chunk_count: usize,
    chunks: Vec<String>,
}

fn test_cwd() -> PathBuf {
    std::env::temp_dir()
}

fn fixtures() -> Vec<Fixture> {
    let mut fixtures = Vec::new();
    for chunk_count in CHUNK_COUNTS {
        fixtures.push(Fixture {
            kind: "paragraph",
            chunk_count,
            chunks: growing_paragraph_chunks(chunk_count),
        });
        fixtures.push(Fixture {
            kind: "unclosed_fence",
            chunk_count,
            chunks: unclosed_fence_chunks(chunk_count),
        });
        fixtures.push(Fixture {
            kind: "table",
            chunk_count,
            chunks: growing_table_chunks(chunk_count),
        });
    }
    fixtures
}

fn growing_paragraph_chunks(chunk_count: usize) -> Vec<String> {
    (0..chunk_count)
        .map(|index| format!("p{index:04}\n"))
        .collect()
}

fn unclosed_fence_chunks(chunk_count: usize) -> Vec<String> {
    let mut chunks = vec!["```rust\n".to_string()];
    chunks.extend((1..chunk_count).map(|index| format!("x{index:04}\n")));
    chunks
}

fn growing_table_chunks(chunk_count: usize) -> Vec<String> {
    let mut chunks = vec!["| A | B |\n".to_string(), "|---|---|\n".to_string()];
    chunks.extend((2..chunk_count).map(|index| format!("| {index:04} | x |\n")));
    chunks
}

fn line_text(line: &Line<'_>) -> String {
    line.spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect()
}

fn plain_lines(lines: &[Line<'_>]) -> Vec<String> {
    lines.iter().map(line_text).collect()
}

fn strip_agent_prefix(lines: Vec<Line<'static>>) -> Vec<Line<'static>> {
    lines
        .into_iter()
        .map(|line| Line::from(line_text(&line).chars().skip(2).collect::<String>()))
        .collect()
}

fn expected_lines(source: &str) -> Vec<String> {
    let mut expected = Vec::new();
    append_markdown_agent(source, Some(RENDER_WIDTH), &mut expected);
    plain_lines(&expected)
}

fn verify_fixture_at_presentation_boundaries(fixture: &Fixture, cwd: &Path) {
    let presentation_chunks = presentation_chunks();
    let mut controller = StreamController::new(Some(RENDER_WIDTH), cwd, HistoryRenderMode::Rich);
    let mut source = String::new();
    let mut emitted_lines = Vec::new();

    for (index, chunk) in fixture.chunks.iter().enumerate() {
        source.push_str(chunk);
        controller.push(chunk);
        if (index + 1) % presentation_chunks != 0 {
            continue;
        }

        let (cell, _idle) = controller.on_commit_tick_batch(usize::MAX);
        if let Some(cell) = cell {
            emitted_lines.extend(strip_agent_prefix(cell.transcript_lines(u16::MAX)));
        }

        let mut visible = emitted_lines.clone();
        visible.extend(visible_lines(controller.current_tail_lines()));
        assert_eq!(
            plain_lines(&visible),
            expected_lines(&source),
            "{} stream diverged at presentation boundary after {} chunks",
            fixture.kind,
            index + 1,
        );
    }

    let (cell, finalized_source) = controller.finalize();
    assert_eq!(
        finalized_source.as_deref(),
        Some(source.as_str()),
        "{} stream lost source while finalizing {} chunks",
        fixture.kind,
        fixture.chunk_count,
    );
    if let Some(cell) = cell {
        emitted_lines.extend(strip_agent_prefix(cell.transcript_lines(u16::MAX)));
    }
    assert_eq!(
        plain_lines(&emitted_lines),
        expected_lines(&source),
        "{} stream diverged after finalizing {} chunks",
        fixture.kind,
        fixture.chunk_count,
    );
}

#[test]
fn presentation_boundaries_preserve_output() {
    let cwd = test_cwd();
    for fixture in fixtures() {
        verify_fixture_at_presentation_boundaries(&fixture, &cwd);
    }
}

fn consume_cell(cell: Option<Box<dyn HistoryCell>>) -> usize {
    let was_present = if cell.is_some() { 1 } else { 0 };
    black_box(cell);
    was_present
}

fn run_fixture(fixture: &Fixture, cwd: &Path) -> usize {
    let presentation_chunks = presentation_chunks();
    let mut controller = StreamController::new(Some(RENDER_WIDTH), cwd, HistoryRenderMode::Rich);
    let mut checksum = 0usize;

    for (index, chunk) in fixture.chunks.iter().enumerate() {
        let enqueued = controller.push(black_box(chunk.as_str()));
        checksum = checksum.wrapping_add(if enqueued { 1 } else { 0 });
        if (index + 1) % presentation_chunks != 0 {
            continue;
        }

        let (cell, _idle) = controller.on_commit_tick_batch(usize::MAX);
        checksum = checksum.wrapping_add(consume_cell(cell));
        let tail = controller.current_tail_lines();
        checksum = checksum.wrapping_add(tail.len());
        black_box(tail);
    }

    let (cell, source) = controller.finalize();
    checksum = checksum.wrapping_add(consume_cell(cell));
    checksum = checksum.wrapping_add(source.as_ref().map_or(0, String::len));
    black_box(source);
    checksum
}

fn run_workload(fixtures: &[Fixture], cwd: &Path) -> usize {
    fixtures.iter().fold(0usize, |checksum, fixture| {
        checksum.wrapping_add(run_fixture(black_box(fixture), cwd))
    })
}

/// Emits one proof-JSONL sample for a balanced 10/100/1000-chunk workload.
///
/// The counter metrics count calls and bytes passed to the streaming markdown
/// renderer. They are test-only instrumentation, reset after the warm-up, and
/// prove that the timed controller path reaches the renderer.
#[allow(clippy::print_stdout)]
#[test]
#[ignore = "Perfloop per-sample benchmark"]
fn perfloop_streaming_rerender_sample() {
    let fixtures = fixtures();
    let cwd = test_cwd();

    black_box(run_fixture(&fixtures[0], &cwd));
    reset_streaming_render_stats();

    let started = Instant::now();
    let checksum = run_workload(&fixtures, &cwd);
    let elapsed_ns = started.elapsed().as_nanos();
    let stats = streaming_render_stats();
    black_box(checksum);

    assert!(
        stats.calls > 0,
        "workload did not reach the streaming renderer"
    );
    assert!(
        stats.input_bytes > 0,
        "workload did not present input to the streaming renderer"
    );

    println!("{{\"metric\":\"streaming_rerender_ns/op\",\"value\":{elapsed_ns}}}");
    println!(
        "{{\"metric\":\"streaming_renderer_calls/op\",\"value\":{}}}",
        stats.calls
    );
    println!(
        "{{\"metric\":\"streaming_renderer_input_bytes/op\",\"value\":{}}}",
        stats.input_bytes
    );
}

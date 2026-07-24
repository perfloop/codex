//! A focused, in-memory measurement of `custom_terminal` frame output.
//!
//! The driver imports the production source directly, then exercises
//! `Terminal::draw -> flush -> diff_buffers -> draw` through a VT100 parser.

use std::env;
use std::hint::black_box;
use std::io;
use std::io::Write;
use std::mem;
use std::time::Instant;

// `custom_terminal` only uses this warning on the constructor path that probes
// a real terminal. The driver supplies a cursor position, so a no-op shim keeps
// the measured source and its dependencies focused on the terminal path.
#[macro_export]
macro_rules! warn {
    ($($tokens:tt)*) => {};
}
extern crate self as tracing;

#[path = "../../../src/custom_terminal.rs"]
mod custom_terminal;

use custom_terminal::Terminal;
use ratatui::backend::Backend;
use ratatui::backend::ClearType;
use ratatui::backend::WindowSize;
use ratatui::buffer::Buffer;
use ratatui::buffer::Cell;
use ratatui::layout::Position;
use ratatui::layout::Rect;
use ratatui::layout::Size;
use ratatui::style::Color;
use ratatui::style::Modifier;
use ratatui::style::Style;

const WIDTH: u16 = 120;
const HEIGHT: u16 = 40;
const FRAMES_PER_SAMPLE: u64 = 1_024;

struct CaptureBackend {
    output: Vec<u8>,
    parser: vt100::Parser,
    size: Size,
    cursor: Position,
}

impl CaptureBackend {
    fn new(width: u16, height: u16) -> Self {
        Self {
            output: Vec::new(),
            parser: vt100::Parser::new(height, width, 0),
            size: Size { width, height },
            cursor: Position { x: 0, y: 0 },
        }
    }

    fn take_output(&mut self) -> Vec<u8> {
        mem::take(&mut self.output)
    }
}

impl Write for CaptureBackend {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.output.extend_from_slice(bytes);
        self.parser.process(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Backend for CaptureBackend {
    fn draw<'a, I>(&mut self, _content: I) -> io::Result<()>
    where
        I: Iterator<Item = (u16, u16, &'a Cell)>,
    {
        Ok(())
    }

    fn hide_cursor(&mut self) -> io::Result<()> {
        Ok(())
    }

    fn show_cursor(&mut self) -> io::Result<()> {
        Ok(())
    }

    fn get_cursor_position(&mut self) -> io::Result<Position> {
        Ok(self.cursor)
    }

    fn set_cursor_position<P: Into<Position>>(&mut self, position: P) -> io::Result<()> {
        self.cursor = position.into();
        Ok(())
    }

    fn clear(&mut self) -> io::Result<()> {
        Ok(())
    }

    fn clear_region(&mut self, _clear_type: ClearType) -> io::Result<()> {
        Ok(())
    }

    fn append_lines(&mut self, _line_count: u16) -> io::Result<()> {
        Ok(())
    }

    fn scroll_region_up(
        &mut self,
        _region: std::ops::Range<u16>,
        _scroll_by: u16,
    ) -> io::Result<()> {
        Ok(())
    }

    fn scroll_region_down(
        &mut self,
        _region: std::ops::Range<u16>,
        _scroll_by: u16,
    ) -> io::Result<()> {
        Ok(())
    }

    fn size(&self) -> io::Result<Size> {
        Ok(self.size)
    }

    fn window_size(&mut self) -> io::Result<WindowSize> {
        Ok(WindowSize {
            columns_rows: self.size,
            pixels: self.size,
        })
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn terminal(width: u16, height: u16) -> Terminal<CaptureBackend> {
    let backend = CaptureBackend::new(width, height);
    let mut terminal = Terminal::with_options_and_cursor_position(backend, Position { x: 0, y: 0 })
        .expect("capture terminal should initialize");
    terminal.set_viewport_area(Rect::new(0, 0, width, height));
    terminal
}

fn render(terminal: &mut Terminal<CaptureBackend>, draw: impl FnOnce(&mut Buffer, Rect)) {
    terminal
        .draw(|frame| {
            let area = frame.area();
            draw(frame.buffer_mut(), area);
        })
        .expect("frame should draw");
}

fn sparse(terminal: &mut Terminal<CaptureBackend>, label: &str) {
    render(terminal, |buffer, area| {
        buffer.set_style(area, Style::default());
        buffer.set_string(0, 0, label, Style::default().fg(Color::Cyan));
        for y in (4..area.height).step_by(8) {
            buffer.set_string(
                0,
                y,
                "waiting for activity",
                Style::default().fg(Color::DarkGray),
            );
        }
    });
}

fn tail_content(terminal: &mut Terminal<CaptureBackend>, label: &str) {
    render(terminal, |buffer, area| {
        buffer.set_style(area, Style::default());
        buffer.set_string(0, 0, label, Style::default());
        for y in 0..area.height {
            buffer.set_string(
                area.width.saturating_sub(9),
                y,
                "old tail",
                Style::default().fg(Color::Yellow),
            );
        }
    });
}

fn empty(terminal: &mut Terminal<CaptureBackend>) {
    render(terminal, |buffer, area| {
        buffer.set_style(area, Style::default())
    });
}

fn clear_to_end_count(bytes: &[u8]) -> usize {
    bytes
        .windows(b"\x1b[K".len())
        .filter(|window| *window == b"\x1b[K")
        .count()
}

fn labels(tick: u64) -> Vec<String> {
    (0..FRAMES_PER_SAMPLE)
        .map(|frame| format!("stream tick {:08}", tick.saturating_add(frame)))
        .collect()
}

#[allow(clippy::print_stdout)]
fn sample(tick: u64, tail_changes: bool) {
    let labels = labels(tick);
    let mut terminal = terminal(WIDTH, HEIGHT);
    if tail_changes {
        tail_content(&mut terminal, &labels[0]);
    } else {
        let previous = format!("stream tick {:08}", tick.saturating_sub(1));
        sparse(&mut terminal, &previous);
    }
    terminal.backend_mut().take_output();
    terminal.backend_mut().output.reserve(2 * 1024 * 1024);

    let started = Instant::now();
    if tail_changes {
        for label in &labels {
            tail_content(&mut terminal, label);
            sparse(&mut terminal, label);
        }
    } else {
        for label in &labels {
            sparse(&mut terminal, label);
        }
    }
    let elapsed_ns = started.elapsed().as_nanos() as f64 / FRAMES_PER_SAMPLE as f64;
    let output = terminal.backend_mut().take_output();
    let bytes = black_box(output.len() as f64 / FRAMES_PER_SAMPLE as f64);
    let clears = black_box(clear_to_end_count(&output) as f64 / FRAMES_PER_SAMPLE as f64);
    let (time_metric, bytes_metric, clears_metric) = if tail_changes {
        (
            "in_memory_terminal_draw_ns_per_tail_change_cycle",
            "terminal_output_bytes_per_tail_change_cycle",
            "clear_to_end_commands_per_tail_change_cycle",
        )
    } else {
        (
            "in_memory_terminal_draw_ns_per_sparse_one_row_change_frame",
            "terminal_output_bytes_per_sparse_one_row_change",
            "clear_to_end_commands_per_sparse_one_row_change",
        )
    };
    println!("{{\"metric\":\"{time_metric}\",\"value\":{elapsed_ns}}}");
    println!("{{\"metric\":\"{bytes_metric}\",\"value\":{bytes}}}");
    println!("{{\"metric\":\"{clears_metric}\",\"value\":{clears}}}");
}

fn verify() {
    let mut stale = CaptureBackend::new(12, 1);
    stale.parser.process(b"\x1b[H    stale");
    let mut stale_terminal =
        Terminal::with_options_and_cursor_position(stale, Position { x: 0, y: 0 })
            .expect("capture terminal should initialize");
    stale_terminal.set_viewport_area(Rect::new(0, 0, 12, 1));
    empty(&mut stale_terminal);
    assert!(
        !stale_terminal
            .backend()
            .parser
            .screen()
            .contents()
            .contains("stale")
    );

    let mut wide = terminal(12, 1);
    render(&mut wide, |buffer, area| {
        buffer.set_style(area, Style::default());
        buffer.set_string(0, 0, "中文", Style::default());
    });
    wide.backend_mut().take_output();
    render(&mut wide, |buffer, area| {
        buffer.set_style(area, Style::default());
        buffer.set_string(0, 0, "中", Style::default());
    });
    let screen = wide.backend().parser.screen().contents();
    assert!(screen.contains('中') && !screen.contains('文'));

    let mut colors = terminal(10, 1);
    render(&mut colors, |buffer, area| {
        buffer.set_style(area, Style::default().bg(Color::Blue));
    });
    let blue = format!(
        "{:?}",
        colors
            .backend()
            .parser
            .screen()
            .cell(0, 8)
            .unwrap()
            .bgcolor()
    );
    colors.backend_mut().take_output();
    render(&mut colors, |buffer, area| {
        buffer.set_style(area, Style::default().bg(Color::Red));
    });
    let red = format!(
        "{:?}",
        colors
            .backend()
            .parser
            .screen()
            .cell(0, 8)
            .unwrap()
            .bgcolor()
    );
    assert_ne!(blue, red);
    assert_ne!(red, "Default");

    let mut modifiers = terminal(10, 1);
    render(&mut modifiers, |buffer, area| {
        buffer.set_style(area, Style::default().add_modifier(Modifier::UNDERLINED));
    });
    modifiers.backend_mut().take_output();
    empty(&mut modifiers);
    assert!(clear_to_end_count(&modifiers.backend_mut().take_output()) > 0);

    let mut unchanged = terminal(WIDTH, HEIGHT);
    sparse(&mut unchanged, "stream tick 00000017");
    let before = unchanged.backend().parser.screen().contents();
    unchanged.backend_mut().take_output();
    sparse(&mut unchanged, "stream tick 00000017");
    assert_eq!(before, unchanged.backend().parser.screen().contents());
}

fn tick() -> u64 {
    env::args()
        .nth(2)
        .expect("a runtime tick argument is required")
        .parse()
        .expect("tick must be an unsigned integer")
}

#[allow(clippy::print_stderr, clippy::print_stdout)]
fn main() {
    match env::args().nth(1).as_deref() {
        Some("sparse-one-row-change") => sample(tick(), false),
        Some("tail-change") => sample(tick(), true),
        Some("verify") => {
            verify();
            println!("custom-terminal visual verification passed");
        }
        _ => {
            eprintln!(
                "usage: perf_custom_terminal <sparse-one-row-change|tail-change|verify> [tick]"
            );
            std::process::exit(2);
        }
    }
}

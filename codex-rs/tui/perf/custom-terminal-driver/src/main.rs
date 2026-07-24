//! Measurement-only driver for the custom terminal's per-frame output.
//!
//! It runs the public `Terminal::draw` path against an in-memory ANSI terminal so
//! the sample includes `flush`, `diff_buffers`, `draw`, and backend flushing.

use std::env;
use std::hint::black_box;
use std::io;
use std::io::Write;
use std::mem;

#[path = "../../../src/custom_terminal.rs"]
mod custom_terminal;

use custom_terminal::Terminal;
use ratatui::backend::Backend;
use ratatui::backend::ClearType;
use ratatui::backend::WindowSize;
use ratatui::buffer::Cell;
use ratatui::layout::Position;
use ratatui::layout::Rect;
use ratatui::layout::Size;
use ratatui::style::Color;
use ratatui::style::Modifier;
use ratatui::style::Style;

const WIDTH: u16 = 120;
const HEIGHT: u16 = 40;

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

    fn preload(&mut self, bytes: &[u8]) {
        self.parser.process(bytes);
    }

    fn take_output(&mut self) -> Vec<u8> {
        mem::take(&mut self.output)
    }
}

impl Write for CaptureBackend {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.output.extend_from_slice(buf);
        self.parser.process(buf);
        Ok(buf.len())
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

fn terminal(backend: CaptureBackend) -> Terminal<CaptureBackend> {
    let size = backend.size;
    let mut terminal = Terminal::with_options_and_cursor_position(backend, Position { x: 0, y: 0 })
        .expect("capture backend should construct a terminal");
    terminal.set_viewport_area(Rect::new(0, 0, size.width, size.height));
    terminal
}

fn draw_sparse_frame(terminal: &mut Terminal<CaptureBackend>, tick: u64) {
    terminal
        .draw(|frame| {
            let area = frame.area();
            let buffer = frame.buffer_mut();
            buffer.set_style(area, Style::default());
            buffer.set_string(
                0,
                0,
                format!("stream tick {tick}"),
                Style::default().fg(Color::Cyan),
            );
            for y in (4..area.height).step_by(8) {
                buffer.set_string(
                    0,
                    y,
                    "waiting for activity",
                    Style::default().fg(Color::DarkGray),
                );
            }
        })
        .expect("sparse frame should draw");
}

fn draw_frame_with_tail_content(terminal: &mut Terminal<CaptureBackend>, tick: u64) {
    terminal
        .draw(|frame| {
            let area = frame.area();
            let buffer = frame.buffer_mut();
            buffer.set_style(area, Style::default());
            buffer.set_string(
                0,
                0,
                format!("stream tick {tick}"),
                Style::default().fg(Color::Cyan),
            );
            for y in 0..area.height {
                buffer.set_string(
                    area.width.saturating_sub(9),
                    y,
                    "old tail",
                    Style::default().fg(Color::Yellow),
                );
            }
        })
        .expect("tail-content frame should draw");
}

fn draw_empty_frame(terminal: &mut Terminal<CaptureBackend>) {
    terminal
        .draw(|frame| {
            let area = frame.area();
            frame.buffer_mut().set_style(area, Style::default());
        })
        .expect("empty frame should draw");
}

fn draw_background_frame(terminal: &mut Terminal<CaptureBackend>, color: Color) {
    terminal
        .draw(|frame| {
            let area = frame.area();
            frame
                .buffer_mut()
                .set_style(area, Style::default().bg(color));
        })
        .expect("background frame should draw");
}

fn draw_text_frame(terminal: &mut Terminal<CaptureBackend>, text: &str) {
    terminal
        .draw(|frame| {
            let area = frame.area();
            let buffer = frame.buffer_mut();
            buffer.set_style(area, Style::default());
            buffer.set_string(0, 0, text, Style::default());
        })
        .expect("text frame should draw");
}

fn draw_underlined_blanks(terminal: &mut Terminal<CaptureBackend>) {
    terminal
        .draw(|frame| {
            let area = frame.area();
            frame
                .buffer_mut()
                .set_style(area, Style::default().add_modifier(Modifier::UNDERLINED));
        })
        .expect("underlined frame should draw");
}

fn clear_to_end_count(bytes: &[u8]) -> usize {
    bytes
        .windows(b"\x1b[K".len())
        .filter(|window| *window == b"\x1b[K")
        .count()
}

fn sparse_one_row_change_sample(tick: u64) {
    let mut terminal = terminal(CaptureBackend::new(WIDTH, HEIGHT));
    draw_sparse_frame(&mut terminal, tick.saturating_sub(1));
    terminal.backend_mut().take_output();

    draw_sparse_frame(&mut terminal, tick);
    let output = terminal.backend_mut().take_output();
    let bytes = black_box(output.len());
    let clear_commands = black_box(clear_to_end_count(&output));

    println!(
        "{{\"metric\":\"terminal_output_bytes_per_sparse_one_row_change\",\"value\":{bytes}}}"
    );
    println!(
        "{{\"metric\":\"clear_to_end_commands_per_sparse_one_row_change\",\"value\":{clear_commands}}}"
    );
}

fn tail_change_sample(tick: u64) {
    let mut terminal = terminal(CaptureBackend::new(WIDTH, HEIGHT));
    draw_frame_with_tail_content(&mut terminal, tick.saturating_sub(1));
    terminal.backend_mut().take_output();

    draw_sparse_frame(&mut terminal, tick);
    let output = terminal.backend_mut().take_output();
    let bytes = black_box(output.len());
    let clear_commands = black_box(clear_to_end_count(&output));

    println!("{{\"metric\":\"terminal_output_bytes_per_tail_change\",\"value\":{bytes}}}");
    println!("{{\"metric\":\"clear_to_end_commands_per_tail_change\",\"value\":{clear_commands}}}");
}

fn verify_visual_differential() {
    let mut stale_backend = CaptureBackend::new(12, 1);
    stale_backend.preload(b"\x1b[H    stale");
    let mut stale_terminal = terminal(stale_backend);
    draw_empty_frame(&mut stale_terminal);
    assert!(
        !stale_terminal
            .backend()
            .parser
            .screen()
            .contents()
            .contains("stale"),
        "an initial frame must erase stale tail text"
    );

    let mut wide_terminal = terminal(CaptureBackend::new(12, 1));
    draw_text_frame(&mut wide_terminal, "中文");
    wide_terminal.backend_mut().take_output();
    draw_text_frame(&mut wide_terminal, "中");
    let wide_contents = wide_terminal.backend().parser.screen().contents();
    assert!(
        wide_contents.contains('中'),
        "remaining wide glyph was lost"
    );
    assert!(
        !wide_contents.contains('文'),
        "removed wide glyph remains visible: {wide_contents:?}"
    );

    let mut background_terminal = terminal(CaptureBackend::new(10, 1));
    draw_background_frame(&mut background_terminal, Color::Blue);
    let before_background = format!(
        "{:?}",
        background_terminal
            .backend()
            .parser
            .screen()
            .cell(0, 8)
            .expect("background cell should exist")
            .bgcolor()
    );
    background_terminal.backend_mut().take_output();
    draw_background_frame(&mut background_terminal, Color::Red);
    let after_background = format!(
        "{:?}",
        background_terminal
            .backend()
            .parser
            .screen()
            .cell(0, 8)
            .expect("background cell should exist")
            .bgcolor()
    );
    assert_ne!(
        before_background, after_background,
        "tail background did not update"
    );
    assert_ne!(after_background, "Default", "tail background was reset");

    let mut modifier_terminal = terminal(CaptureBackend::new(10, 1));
    draw_underlined_blanks(&mut modifier_terminal);
    modifier_terminal.backend_mut().take_output();
    draw_empty_frame(&mut modifier_terminal);
    let modifier_output = modifier_terminal.backend_mut().take_output();
    assert!(
        clear_to_end_count(&modifier_output) > 0,
        "a modifier-only tail must be cleared"
    );

    let mut static_terminal = terminal(CaptureBackend::new(WIDTH, HEIGHT));
    draw_sparse_frame(&mut static_terminal, 17);
    let before = static_terminal.backend().parser.screen().contents();
    static_terminal.backend_mut().take_output();
    draw_sparse_frame(&mut static_terminal, 17);
    let after = static_terminal.backend().parser.screen().contents();
    assert_eq!(
        before, after,
        "an unchanged frame changed the visible screen"
    );
}

fn parse_tick() -> u64 {
    env::args()
        .nth(2)
        .expect("a runtime tick argument is required")
        .parse()
        .expect("tick must be an unsigned integer")
}

#[allow(clippy::print_stderr, clippy::print_stdout)]
fn main() {
    match env::args().nth(1).as_deref() {
        Some("sparse-one-row-change") => sparse_one_row_change_sample(parse_tick()),
        Some("tail-change") => tail_change_sample(parse_tick()),
        Some("verify") => {
            verify_visual_differential();
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

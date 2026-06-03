use std::io::{stdout, Write};

use anyhow::Result;
use crossterm::{
    cursor,
    event::{self, Event, KeyCode},
    execute,
    style::Print,
    terminal::{self, ClearType},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Normal,
    Insert,
}

struct TerminalGuard;

impl TerminalGuard {
    fn new() -> Result<Self> {
        terminal::enable_raw_mode()?;

        if let Err(err) = execute!(
            stdout(),
            terminal::EnterAlternateScreen,
            cursor::Hide,
            terminal::Clear(ClearType::All),
            cursor::MoveTo(0, 0),
        ) {
            let _ = terminal::disable_raw_mode();
            return Err(err.into());
        }

        Ok(Self)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = execute!(
            stdout(),
            cursor::Show,
            terminal::LeaveAlternateScreen,
        );
        let _ = terminal::disable_raw_mode();
        let _ = stdout().flush();
    }
}

struct Document {
    lines: Vec<String>,
}

impl Document {
    fn empty() -> Self {
        Self { lines: Vec::new() }
    }

    fn open(path: &str) -> Result<Self> {
        let contents = std::fs::read_to_string(path)?;
        let lines = contents.lines().map(|line| line.to_string()).collect();
        Ok(Self { lines })
    }
}

struct Editor {
    cursor: Position,
    document: Document,
    mode: Mode,
}

impl Editor {
    fn new(document: Document) -> Self {
        Self {
            cursor: Position { row: 0, col: 0 },
            document,
            mode: Mode::Normal,
        }
    }

    fn current_line_len(&self) -> u16 {
        let row = self.cursor.row as usize;
        self.document
            .lines
            .get(row)
            .map(|line| line.len().min(u16::MAX as usize) as u16)
            .unwrap_or(0)
    }

    fn clamp_col(&mut self) {
        let max_col = self.current_line_len();
        self.cursor.col = self.cursor.col.min(max_col);
    }

    fn move_left(&mut self) {
        self.cursor.col = self.cursor.col.saturating_sub(1);
    }

    fn move_right(&mut self) {
        let max_col = self.current_line_len();
        if self.cursor.col < max_col {
            self.cursor.col += 1;
        }
    }

    fn move_up(&mut self) {
        self.cursor.row = self.cursor.row.saturating_sub(1);
        self.clamp_col();
    }

    fn move_down(&mut self) {
        let row = self.cursor.row as usize;
        if row + 1 < self.document.lines.len() {
            self.cursor.row += 1;
            self.clamp_col();
        }
    }

    fn enter_insert_mode(&mut self) {
        self.mode = Mode::Insert;
    }

    fn enter_normal_mode(&mut self) {
        self.mode = Mode::Normal;
    }

    fn draw(&self) -> Result<()> {
        let mut out = stdout();
        let (cols, rows) = terminal::size()?;

        // Hide the cursor while we redraw, so you don't see it "teleport" as we paint lines.
        execute!(
            out,
            cursor::Hide,
            cursor::MoveTo(0, 0),
            terminal::Clear(ClearType::All)
        )?;

        let content_rows = rows.saturating_sub(1);

        // Draw by absolute positioning each row to avoid scrolling artifacts from `\r\n`.
        for row in 0..content_rows {
            let line = self.document.lines.get(row as usize);
            execute!(out, cursor::MoveTo(0, row), terminal::Clear(ClearType::CurrentLine))?;
            match line {
                Some(text) => {
                    // Simple viewport: truncate by characters to avoid wrapping.
                    let visible: String = text.chars().take(cols as usize).collect();
                    execute!(out, Print(visible))?;
                }
                None => {
                    execute!(out, Print("~"))?;
                }
            }
        }

        let status_row = rows.saturating_sub(1);
        execute!(
            out,
            cursor::MoveTo(0, status_row),
            terminal::Clear(ClearType::CurrentLine)
        )?;
        let mode_text = match self.mode {
            Mode::Normal => "-- NORMAL --",
            Mode::Insert => "-- INSERT --",
        };
        execute!(out, Print(mode_text))?;

        let cursor_row = self.cursor.row.min(content_rows.saturating_sub(1));
        let cursor_col = self.cursor.col.min(cols.saturating_sub(1));
        execute!(out, cursor::MoveTo(cursor_col, cursor_row), cursor::Show)?;
        out.flush()?;
        Ok(())
    }
}

#[derive(Debug, Default, Clone, Copy)]
struct Position {
    row: u16,
    col: u16,
}

fn main() -> Result<()> {
    let _terminal = TerminalGuard::new()?;
    let file_path = std::env::args().nth(1);
    let document = match file_path.as_deref() {
        Some(path) => Document::open(path)?,
        None => Document::empty(),
    };
    let mut editor = Editor::new(document);

    loop {
        editor.draw()?;
        match event::read()? {
            Event::Key(key)
                if key.code == KeyCode::Char('q') && key.modifiers.is_empty() =>
            {
                if editor.mode == Mode::Normal {
                    break;
                }
            }
            Event::Key(key) => match editor.mode {
                Mode::Normal => match key.code {
                    KeyCode::Char('h') | KeyCode::Left => editor.move_left(),
                    KeyCode::Char('l') | KeyCode::Right => editor.move_right(),
                    KeyCode::Char('k') | KeyCode::Up => editor.move_up(),
                    KeyCode::Char('j') | KeyCode::Down => editor.move_down(),
                    KeyCode::Char('i') => editor.enter_insert_mode(),
                    _ => {}
                },
                Mode::Insert => match key.code {
                    KeyCode::Esc => editor.enter_normal_mode(),
                    _ => {}
                },
            },
            Event::Resize(_, _) => {}
            _ => {}
        }
    }
    Ok(())
}

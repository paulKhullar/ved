use std::io::{stdout, Write};

use anyhow::Result;
use crossterm::{
    cursor,
    event::{self, Event, KeyCode},
    execute,
    style::Print,
    terminal::{self, ClearType},
};

struct TerminalGuard;

impl TerminalGuard {
    fn new() -> Result<Self> {
        terminal::enable_raw_mode()?;

        if let Err(err) = execute!(
            stdout(),
            terminal::EnterAlternateScreen,
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
}

impl Editor {
    fn new(document: Document) -> Self {
        Self {
            cursor: Position { row: 0, col: 0 },
            document,
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

    fn draw(&self) -> Result<()> {
        let mut out = stdout();
        let (cols, rows) = terminal::size()?;

        execute!(out, cursor::MoveTo(0, 0), terminal::Clear(ClearType::All))?;

        for row in 0..rows {
            let line = self.document.lines.get(row as usize);
            match line {
                Some(text) => {
                    execute!(out, Print(text), Print("\r\n"))?;
                }
                None => {
                    execute!(out, Print("~"), Print("\r\n"))?;
                }
            }
        }

        let cursor_row = self.cursor.row.min(rows.saturating_sub(1));
        let cursor_col = self.cursor.col.min(cols.saturating_sub(1));
        execute!(out, cursor::MoveTo(cursor_col, cursor_row))?;
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
                break;
            }
            Event::Key(key) if matches!(key.code, KeyCode::Char('h') | KeyCode::Left) => editor.move_left(),
            Event::Key(key) if matches!(key.code, KeyCode::Char('l') | KeyCode::Right) => {
                editor.move_right();
            }
            Event::Key(key) if matches!(key.code, KeyCode::Char('k') | KeyCode::Up) => editor.move_up(),
            Event::Key(key) if matches!(key.code, KeyCode::Char('j') | KeyCode::Down) => {
                editor.move_down();
            }
            Event::Resize(_, _) => {}
            _ => {}
        }
    }
    Ok(())
}

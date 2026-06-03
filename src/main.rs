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
    document: Document,
}

impl Editor {
    fn new(document: Document) -> Self {
        Self { document }
    }

    fn draw(&self) -> Result<()> {
        let mut out = stdout();
        let (_cols, rows) = terminal::size()?;

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

        out.flush()?;
        Ok(())
    }
}

fn main() -> Result<()> {
    let _terminal = TerminalGuard::new()?;
    let file_path = std::env::args().nth(1);
    let document = match file_path.as_deref() {
        Some(path) => Document::open(path)?,
        None => Document::empty(),
    };
    let editor = Editor::new(document);

    loop {
        editor.draw()?;
        match event::read()? {
            Event::Key(key)
                if key.code == KeyCode::Char('q') && key.modifiers.is_empty() =>
            {
                break;
            }
            Event::Resize(_, _) => {}
            _ => {}
        }
    }
    Ok(())
}

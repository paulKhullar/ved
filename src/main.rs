mod cells;
mod document;
mod editor;

use std::io::{stdout, Write};

use anyhow::Result;
use crossterm::{
    cursor, event,
    event::Event,
    execute,
    terminal::{self, ClearType},
};

use document::Document;
use editor::Editor;

// TerminalGuard stays in main.rs — it's infrastructure, not editing logic.
// The Drop impl ensures the terminal is restored even if the program panics.
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
        let _ = execute!(stdout(), cursor::Show, terminal::LeaveAlternateScreen);
        let _ = terminal::disable_raw_mode();
        let _ = stdout().flush();
    }
}

fn main() -> Result<()> {
    let _terminal = TerminalGuard::new()?;
    let file_path = std::env::args().nth(1);
    let document = match file_path.as_deref() {
        Some(path) => Document::open(path)?,
        None => Document::empty(),
    };
    let mut editor = Editor::new(document, file_path);

    loop {
        editor.draw()?;
        match event::read()? {
            Event::Key(key) => {
                if editor.handle_key(key)? {
                    break;
                }
            }
            Event::Resize(_, _) => {} // draw() re-queries size each frame — resize is free
            _ => {}
        }
    }
    Ok(())
}

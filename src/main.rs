mod cells;
mod document;
mod editor;
mod repl;

use std::io::{stdout, Write};

use std::time::Duration;

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
        // Drain any REPL output that arrived since the last tick.
        // This happens before draw() so new output is always visible immediately.
        editor.drain_repl_output();
        editor.draw()?;

        // poll(50ms) instead of blocking read() so output appears promptly even
        // when the user isn't typing. If no key arrives within 50ms we loop back
        // and redraw — the REPL output panel updates at ~20fps.
        if event::poll(Duration::from_millis(50))? {
            match event::read()? {
                Event::Key(key) => {
                    if editor.handle_key(key)? {
                        break;
                    }
                }
                Event::Resize(_, _) => {}
                _ => {}
            }
        }
    }
    Ok(())
}

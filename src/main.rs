use std::io::{stdout, Write};

use anyhow::Result;
use crossterm::{
    cursor,
    event::{self, Event, KeyCode, KeyEvent, KeyModifiers},
    execute,
    style::Print,
    terminal::{self, ClearType},
};

// --- Phase 3: Mode as a state machine ---
// Adding Command here is Phase 5. The enum now covers all three states the editor can be in.
// Each variant is a different "world" with different key bindings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Normal,
    Insert,
    Command, // user is typing a :command at the bottom of the screen
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
    // Phase 6: scroll_offset is the document line shown at the top of the screen.
    // cursor.row is always a *document* coordinate.
    // screen row = cursor.row - scroll_offset.
    scroll_offset: usize,
    document: Document,
    // Phase 5: we need to know the file path to save back to it.
    file_path: Option<String>,
    mode: Mode,
    modified: bool,
    // Phase 5: the text the user is typing after `:`.
    command_buf: String,
    // Phase 5: a transient one-line message shown in the status bar (e.g. "No file to save to").
    // It's cleared on the next keypress.
    message: Option<String>,
}

impl Editor {
    fn new(document: Document, file_path: Option<String>) -> Self {
        Self {
            cursor: Position { row: 0, col: 0 },
            scroll_offset: 0,
            document,
            file_path,
            mode: Mode::Normal,
            modified: false,
            command_buf: String::new(),
            message: None,
        }
    }

    fn current_line_len(&self) -> u16 {
        let row = self.cursor.row as usize;
        self.document
            .lines
            .get(row)
            .map(|line| line.chars().count().min(u16::MAX as usize) as u16)
            .unwrap_or(0)
    }

    fn clamp_col(&mut self) {
        let max_col = self.current_line_len();
        self.cursor.col = self.cursor.col.min(max_col);
    }

    fn col_to_byte_index(line: &str, col: u16) -> usize {
        let mut current_col: u16 = 0;
        for (byte_idx, _) in line.char_indices() {
            if current_col == col {
                return byte_idx;
            }
            current_col = current_col.saturating_add(1);
        }
        line.len()
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

    fn ensure_line_exists(&mut self) {
        if self.document.lines.is_empty() {
            self.document.lines.push(String::new());
        }
        if (self.cursor.row as usize) >= self.document.lines.len() {
            self.cursor.row = (self.document.lines.len() - 1) as u16;
        }
    }

    fn insert_char(&mut self, ch: char) {
        self.ensure_line_exists();
        let row = self.cursor.row as usize;
        let line = &mut self.document.lines[row];
        let byte_idx = Self::col_to_byte_index(line, self.cursor.col);
        line.insert(byte_idx, ch);
        self.cursor.col = self.cursor.col.saturating_add(1);
        self.modified = true;
    }

    fn insert_newline(&mut self) {
        self.ensure_line_exists();
        let row = self.cursor.row as usize;

        let byte_idx = {
            let line = &self.document.lines[row];
            Self::col_to_byte_index(line, self.cursor.col)
        };

        let new_line = self.document.lines[row].split_off(byte_idx);
        self.document.lines.insert(row + 1, new_line);
        self.cursor.row = self.cursor.row.saturating_add(1);
        self.cursor.col = 0;
        self.modified = true;
    }

    fn backspace(&mut self) {
        self.ensure_line_exists();
        let row = self.cursor.row as usize;

        if self.cursor.col > 0 {
            let line = &mut self.document.lines[row];
            let delete_col = self.cursor.col.saturating_sub(1);
            let delete_byte_idx = Self::col_to_byte_index(line, delete_col);
            line.remove(delete_byte_idx);
            self.cursor.col = delete_col;
            self.modified = true;
            return;
        }

        if row == 0 {
            return;
        }

        let current = self.document.lines.remove(row);
        let prev_row = row - 1;

        let prev_len_chars = self.document.lines[prev_row].chars().count().min(u16::MAX as usize) as u16;
        self.document.lines[prev_row].push_str(&current);
        self.cursor.row = prev_row as u16;
        self.cursor.col = prev_len_chars;
        self.modified = true;
    }

    // --- Phase 5: saving and command execution ---

    fn save(&mut self) -> Result<()> {
        match &self.file_path {
            None => {
                self.message = Some("No file name — use :w <filename>".to_string());
            }
            Some(path) => {
                // join("\n") gives us "line1\nline2\nline3"; the trailing "\n" adds the final newline
                // that most editors and tools expect at the end of a text file.
                let content = self.document.lines.join("\n") + "\n";
                std::fs::write(path, content)?;
                self.modified = false;
                self.message = Some(format!("\"{}\" written", path));
            }
        }
        Ok(())
    }

    // Executes whatever is in command_buf and returns true if the editor should quit.
    fn execute_command(&mut self) -> Result<bool> {
        let cmd = self.command_buf.trim().to_string();
        self.command_buf.clear();
        self.mode = Mode::Normal;

        match cmd.as_str() {
            "w" | "write" => {
                self.save()?;
                Ok(false)
            }
            "q" => {
                if self.modified {
                    self.message = Some(
                        "Unsaved changes — use :wq to save and quit, or :q! to discard".to_string(),
                    );
                    Ok(false)
                } else {
                    Ok(true) // signal the main loop to exit
                }
            }
            "wq" | "x" => {
                self.save()?;
                Ok(true)
            }
            "q!" => Ok(true), // force quit, no save
            other => {
                self.message = Some(format!("Unknown command: {other}"));
                Ok(false)
            }
        }
    }

    // --- Phase 6: scroll to keep cursor visible ---

    // Called at the top of draw(). Adjusts scroll_offset so the cursor is always
    // within the visible viewport, with a small margin (scroll_off) above and below.
    //
    // The two invariants we maintain:
    //   scroll_offset ≤ cursor.row  (cursor can't be above the screen)
    //   cursor.row < scroll_offset + content_rows  (cursor can't be below the screen)
    fn scroll_to_cursor(&mut self, content_rows: usize) {
        let row = self.cursor.row as usize;
        // Keep this many lines of context above and below the cursor, like Vim's `scrolloff`.
        let scroll_off: usize = 3;

        // Scroll up: cursor is too close to (or above) the top of the viewport.
        if row < self.scroll_offset + scroll_off {
            self.scroll_offset = row.saturating_sub(scroll_off);
        }

        // Scroll down: cursor is too close to (or below) the bottom of the viewport.
        // We need: row + scroll_off < scroll_offset + content_rows
        // Rearranged: scroll_offset > row + scroll_off - content_rows
        let min_offset = (row + scroll_off + 1).saturating_sub(content_rows);
        if self.scroll_offset < min_offset {
            self.scroll_offset = min_offset;
        }
    }

    // draw now takes &mut self because scroll_to_cursor mutates scroll_offset.
    // A method needs &mut self any time it changes a field — even indirectly.
    fn draw(&mut self) -> Result<()> {
        let mut out = stdout();
        let (cols, rows) = terminal::size()?;

        // content_rows: screen rows available for document text (everything except the status bar).
        let content_rows = rows.saturating_sub(1) as usize;

        // Phase 6: adjust scroll before we paint, so the cursor is always in view.
        self.scroll_to_cursor(content_rows);

        execute!(
            out,
            cursor::Hide,
            cursor::MoveTo(0, 0),
            terminal::Clear(ClearType::All)
        )?;

        // Render document lines. screen_row 0 corresponds to document line scroll_offset.
        for screen_row in 0..content_rows {
            let doc_row = self.scroll_offset + screen_row;
            let line = self.document.lines.get(doc_row);
            execute!(out, cursor::MoveTo(0, screen_row as u16), terminal::Clear(ClearType::CurrentLine))?;
            match line {
                Some(text) => {
                    let visible: String = text.chars().take(cols as usize).collect();
                    execute!(out, Print(visible))?;
                }
                None => {
                    execute!(out, Print("~"))?;
                }
            }
        }

        // --- Status / command bar ---
        let status_row = rows.saturating_sub(1);
        execute!(
            out,
            cursor::MoveTo(0, status_row),
            terminal::Clear(ClearType::CurrentLine)
        )?;

        match self.mode {
            Mode::Command => {
                // In command mode, show what the user is typing, just like Vim.
                execute!(out, Print(format!(":{}", self.command_buf)))?;
            }
            _ => {
                // In other modes, show the mode indicator and any transient message.
                let mode_text = match self.mode {
                    Mode::Normal => "-- NORMAL --",
                    Mode::Insert => "-- INSERT --",
                    Mode::Command => unreachable!(),
                };
                let modified_marker = if self.modified { " [+]" } else { "" };
                if let Some(msg) = &self.message {
                    execute!(out, Print(format!("{mode_text}{modified_marker}  {msg}")))?;
                } else {
                    execute!(out, Print(format!("{mode_text}{modified_marker}")))?;
                }
            }
        }

        // --- Place the terminal cursor ---
        match self.mode {
            Mode::Command => {
                // In command mode the cursor belongs on the command line, after the typed text.
                let cmd_col = (self.command_buf.len() + 1) as u16; // +1 for the leading ':'
                execute!(out, cursor::MoveTo(cmd_col, status_row), cursor::Show)?;
            }
            _ => {
                // Convert document cursor row → screen row.
                let screen_row = (self.cursor.row as usize)
                    .saturating_sub(self.scroll_offset)
                    .min(content_rows.saturating_sub(1)) as u16;
                let screen_col = self.cursor.col.min(cols.saturating_sub(1));
                execute!(out, cursor::MoveTo(screen_col, screen_row), cursor::Show)?;
            }
        }

        out.flush()?;
        Ok(())
    }

    // Returns true if the editor should quit.
    fn handle_key(&mut self, key: KeyEvent) -> Result<bool> {
        // Any keypress clears the transient message.
        self.message = None;

        match self.mode {
            Mode::Normal => match key.code {
                KeyCode::Char('q') if key.modifiers.is_empty() => return Ok(true),
                KeyCode::Char('h') | KeyCode::Left => self.move_left(),
                KeyCode::Char('l') | KeyCode::Right => self.move_right(),
                KeyCode::Char('k') | KeyCode::Up => self.move_up(),
                KeyCode::Char('j') | KeyCode::Down => self.move_down(),
                KeyCode::Char('i') => self.enter_insert_mode(),
                // Phase 5: ':' enters command mode.
                KeyCode::Char(':') => {
                    self.mode = Mode::Command;
                    self.command_buf.clear();
                }
                _ => {}
            },
            Mode::Insert => match key.code {
                KeyCode::Esc => self.enter_normal_mode(),
                KeyCode::Enter => self.insert_newline(),
                KeyCode::Backspace => self.backspace(),
                KeyCode::Char(ch)
                    if !key.modifiers.contains(KeyModifiers::CONTROL)
                        && !key.modifiers.contains(KeyModifiers::ALT) =>
                {
                    self.insert_char(ch);
                }
                _ => {}
            },
            Mode::Command => match key.code {
                KeyCode::Esc => {
                    // Cancel the command; go back to Normal.
                    self.mode = Mode::Normal;
                    self.command_buf.clear();
                }
                KeyCode::Enter => {
                    // Execute whatever was typed and return its quit signal.
                    return self.execute_command();
                }
                KeyCode::Backspace => {
                    if self.command_buf.is_empty() {
                        // Backspace on an empty command line cancels, like Vim.
                        self.mode = Mode::Normal;
                    } else {
                        self.command_buf.pop();
                    }
                }
                KeyCode::Char(ch)
                    if !key.modifiers.contains(KeyModifiers::CONTROL)
                        && !key.modifiers.contains(KeyModifiers::ALT) =>
                {
                    self.command_buf.push(ch);
                }
                _ => {}
            },
        }

        Ok(false)
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
    let mut editor = Editor::new(document, file_path);

    loop {
        editor.draw()?;
        match event::read()? {
            Event::Key(key) => {
                if editor.handle_key(key)? {
                    break;
                }
            }
            Event::Resize(_, _) => {} // draw() re-queries terminal size each frame, so this is free
            _ => {}
        }
    }
    Ok(())
}

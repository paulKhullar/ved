use std::io::{stdout, Write};

use anyhow::Result;
use crossterm::{
    cursor,
    event::{KeyCode, KeyEvent, KeyModifiers},
    execute,
    style::{Attribute, Print, SetAttribute},
    terminal::{self, ClearType},
};

use crate::cells;
use crate::document::Document;
use crate::repl::Repl;

// --- Mode (Phase 3 + 5 + 7) ---
// OperatorPending is the key Phase 7 addition: it stores *which* operator was pressed
// ('d', 'c', 'y') and waits for the next keypress to supply the motion.
// The editor is now a three-state machine: normal editing → operator pressed → motion pressed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Normal,
    Insert,
    Command,
    OperatorPending(char), // 'd', 'c', 'y' — awaiting a motion
}

#[derive(Debug, Default, Clone, Copy)]
pub struct Position {
    pub row: u16,
    pub col: u16,
}

pub struct Editor {
    pub cursor: Position,
    scroll_offset: usize,
    document: Document,
    file_path: Option<String>,
    mode: Mode,
    modified: bool,
    command_buf: String,
    message: Option<String>,
    // Accumulates digit keypresses before a command, e.g. "10" in "10j".
    count_buf: String,
    // Tracks a partial two-key sequence. Currently only used for 'g' → 'g' (go to first line).
    // Option::take() is used to read-and-clear it atomically — a handy Rust idiom.
    pending_char: Option<char>,

    // Phase 8b — REPL integration
    repl: Option<Repl>,       // None until first Space+Enter in a .py file (lazy start)
    output_buf: Vec<String>,  // lines received from REPL, shown in the panel below the document
}

// Converts a # %% marker line into a full-width horizontal divider, e.g.
//   "# %% [Setup]"  →  "── [Setup] ────────────────────────────────────────"
// Text after "# %%" is used as a label; if blank the divider is just dashes.
// The file is never touched — this is a display-only transform.
fn render_cell_divider(line: &str, width: usize) -> String {
    let label_text = line["# %%".len()..].trim();
    let dash = '─';
    if label_text.is_empty() {
        dash.to_string().repeat(width)
    } else {
        let label = format!(" {label_text} ");
        // Leave at least 2 dashes on the left, fill the rest on the right.
        let left_dashes = 2;
        let right_dashes = width.saturating_sub(left_dashes + label.len());
        format!(
            "{}{}{}" ,
            dash.to_string().repeat(left_dashes),
            label,
            dash.to_string().repeat(right_dashes),
        )
    }
}

// --- Free helper — kept outside `impl` because it doesn't need `self` ---
// Converts a char-column index to a byte offset in a UTF-8 string.
// We can't just index `line[col]` in Rust because a `char` can be multiple bytes.
fn col_to_byte(line: &str, col: u16) -> usize {
    let mut c = 0u16;
    for (byte_idx, _) in line.char_indices() {
        if c == col {
            return byte_idx;
        }
        c += 1;
    }
    line.len() // col is past the end → return the end byte
}

// Number of output lines shown in the REPL panel (not counting the separator row).
const OUTPUT_PANEL_HEIGHT: usize = 8;

impl Editor {
    pub fn new(document: Document, file_path: Option<String>) -> Self {
        Self {
            cursor: Position::default(),
            scroll_offset: 0,
            document,
            file_path,
            mode: Mode::Normal,
            modified: false,
            command_buf: String::new(),
            message: None,
            count_buf: String::new(),
            pending_char: None,
            repl: None,
            output_buf: Vec::new(),
        }
    }

    // -------------------------------------------------------------------------
    // Internal helpers
    // -------------------------------------------------------------------------

    fn line_len(&self, row: usize) -> u16 {
        self.document
            .lines
            .get(row)
            .map(|l| l.chars().count().min(u16::MAX as usize) as u16)
            .unwrap_or(0)
    }

    fn current_line_len(&self) -> u16 {
        self.line_len(self.cursor.row as usize)
    }

    fn clamp_col(&mut self) {
        let max = self.current_line_len();
        self.cursor.col = self.cursor.col.min(max);
    }

    fn ensure_line_exists(&mut self) {
        if self.document.lines.is_empty() {
            self.document.lines.push(String::new());
        }
        let last = (self.document.lines.len() - 1) as u16;
        if self.cursor.row > last {
            self.cursor.row = last;
        }
    }

    // -------------------------------------------------------------------------
    // Navigation
    // -------------------------------------------------------------------------

    fn move_left(&mut self) {
        self.cursor.col = self.cursor.col.saturating_sub(1);
    }

    fn move_right(&mut self) {
        if self.cursor.col < self.current_line_len() {
            self.cursor.col += 1;
        }
    }

    fn move_up(&mut self) {
        if self.cursor.row == 0 {
            return;
        }
        self.cursor.row -= 1;
        // If we landed on a # %% marker, skip one more step up.
        // Exception: if the marker is at row 0 there's nowhere above it, so stay.
        let row = self.cursor.row as usize;
        if row > 0 && cells::is_cell_marker(&self.document.lines[row]) {
            self.cursor.row -= 1;
        }
        self.clamp_col();
    }

    fn move_down(&mut self) {
        let next = self.cursor.row as usize + 1;
        if next >= self.document.lines.len() {
            return;
        }
        self.cursor.row += 1;
        // If we landed on a # %% marker, skip one more step down.
        // Exception: if it's the last line there's nowhere below it, so stay.
        let row = self.cursor.row as usize;
        if cells::is_cell_marker(&self.document.lines[row]) {
            if row + 1 < self.document.lines.len() {
                self.cursor.row += 1;
            }
        }
        self.clamp_col();
    }

    fn go_to_line_start(&mut self) {
        self.cursor.col = 0;
    }

    fn go_to_line_end(&mut self) {
        // In Normal mode, Vim keeps the cursor on the last character, not past it.
        // current_line_len() returns the number of chars; the last valid col is len-1
        // (or 0 for an empty line).
        self.cursor.col = self.current_line_len().saturating_sub(1);
    }

    fn go_to_first_non_blank(&mut self) {
        let row = self.cursor.row as usize;
        if let Some(line) = self.document.lines.get(row) {
            let col = line.chars().position(|c| !c.is_whitespace()).unwrap_or(0);
            self.cursor.col = col as u16;
        }
    }

    fn go_to_first_line(&mut self) {
        self.cursor.row = 0;
        self.clamp_col();
    }

    fn go_to_last_line(&mut self) {
        self.cursor.row = self.document.lines.len().saturating_sub(1) as u16;
        self.clamp_col();
    }

    fn go_to_line(&mut self, line: usize) {
        let last = self.document.lines.len().saturating_sub(1);
        self.cursor.row = line.min(last) as u16;
        self.clamp_col();
    }

    // -------------------------------------------------------------------------
    // Word motions (Phase 7)
    // These treat a "word" as a run of non-whitespace — Vim's WORD semantics.
    // -------------------------------------------------------------------------

    fn word_forward(&mut self) {
        let row = self.cursor.row as usize;
        if let Some(line) = self.document.lines.get(row) {
            let chars: Vec<char> = line.chars().collect();
            let mut col = self.cursor.col as usize;
            // Step over current word (non-whitespace)
            while col < chars.len() && !chars[col].is_whitespace() {
                col += 1;
            }
            // Step over the whitespace gap
            while col < chars.len() && chars[col].is_whitespace() {
                col += 1;
            }
            if col < chars.len() {
                self.cursor.col = col as u16;
            } else {
                // End of line — jump to the start of the next line
                self.move_down();
                self.go_to_first_non_blank();
            }
        }
    }

    fn word_backward(&mut self) {
        let row = self.cursor.row as usize;
        if self.cursor.col == 0 {
            if row > 0 {
                self.cursor.row -= 1;
                // land on the last char of the previous line
                self.cursor.col = self.current_line_len().saturating_sub(1);
            }
            return;
        }
        if let Some(line) = self.document.lines.get(row) {
            let chars: Vec<char> = line.chars().collect();
            let mut col = self.cursor.col as usize;
            col = col.saturating_sub(1);
            // Skip whitespace backward
            while col > 0 && chars[col].is_whitespace() {
                col -= 1;
            }
            // Skip word chars backward to find the start
            while col > 0 && !chars[col - 1].is_whitespace() {
                col -= 1;
            }
            self.cursor.col = col as u16;
        }
    }

    fn word_end(&mut self) {
        let row = self.cursor.row as usize;
        if let Some(line) = self.document.lines.get(row) {
            let chars: Vec<char> = line.chars().collect();
            let mut col = self.cursor.col as usize;
            if col + 1 < chars.len() {
                col += 1;
            }
            // Skip whitespace
            while col < chars.len() && chars[col].is_whitespace() {
                col += 1;
            }
            // Advance to the last char of the word
            while col + 1 < chars.len() && !chars[col + 1].is_whitespace() {
                col += 1;
            }
            self.cursor.col = col.min(chars.len().saturating_sub(1)) as u16;
        }
    }

    // -------------------------------------------------------------------------
    // Editing — basic (Phases 3 & 4)
    // -------------------------------------------------------------------------

    fn enter_insert_mode(&mut self) {
        self.mode = Mode::Insert;
    }

    fn enter_normal_mode(&mut self) {
        // When leaving Insert mode, clamp col to last char (Normal mode doesn't allow past-end).
        let len = self.current_line_len();
        if len > 0 {
            self.cursor.col = self.cursor.col.min(len - 1);
        } else {
            self.cursor.col = 0;
        }
        self.mode = Mode::Normal;
    }

    fn insert_char(&mut self, ch: char) {
        self.ensure_line_exists();
        let row = self.cursor.row as usize;
        let byte_idx = col_to_byte(&self.document.lines[row], self.cursor.col);
        self.document.lines[row].insert(byte_idx, ch);
        self.cursor.col += 1;
        self.modified = true;
    }

    fn insert_newline(&mut self) {
        self.ensure_line_exists();
        let row = self.cursor.row as usize;
        let byte_idx = col_to_byte(&self.document.lines[row], self.cursor.col);
        let tail = self.document.lines[row].split_off(byte_idx);
        self.document.lines.insert(row + 1, tail);
        self.cursor.row += 1;
        self.cursor.col = 0;
        self.modified = true;
    }

    fn backspace(&mut self) {
        self.ensure_line_exists();
        let row = self.cursor.row as usize;
        if self.cursor.col > 0 {
            let del_col = self.cursor.col - 1;
            let byte_idx = col_to_byte(&self.document.lines[row], del_col);
            self.document.lines[row].remove(byte_idx);
            self.cursor.col = del_col;
            self.modified = true;
            return;
        }
        if row == 0 {
            return;
        }
        let current = self.document.lines.remove(row);
        let prev_len = self.document.lines[row - 1].chars().count() as u16;
        self.document.lines[row - 1].push_str(&current);
        self.cursor.row -= 1;
        self.cursor.col = prev_len;
        self.modified = true;
    }

    // -------------------------------------------------------------------------
    // Editing — Normal-mode commands (Phase 7)
    // -------------------------------------------------------------------------

    // x — delete the char under the cursor
    fn delete_char_at_cursor(&mut self) {
        self.ensure_line_exists();
        let row = self.cursor.row as usize;
        let len = self.document.lines[row].chars().count();
        if (self.cursor.col as usize) >= len {
            return;
        }
        let byte_idx = col_to_byte(&self.document.lines[row], self.cursor.col);
        self.document.lines[row].remove(byte_idx);
        self.clamp_col();
        self.modified = true;
    }

    // o — open a new line below and enter Insert
    fn open_line_below(&mut self) {
        self.ensure_line_exists();
        let row = self.cursor.row as usize;
        self.document.lines.insert(row + 1, String::new());
        self.cursor.row += 1;
        self.cursor.col = 0;
        self.mode = Mode::Insert;
        self.modified = true;
    }

    // O — open a new line above and enter Insert
    fn open_line_above(&mut self) {
        self.ensure_line_exists();
        let row = self.cursor.row as usize;
        self.document.lines.insert(row, String::new());
        // cursor.row stays the same — it now points to the new blank line
        self.cursor.col = 0;
        self.mode = Mode::Insert;
        self.modified = true;
    }

    // a — append: move one right, enter Insert
    fn append_after_cursor(&mut self) {
        if self.cursor.col < self.current_line_len() {
            self.cursor.col += 1;
        }
        self.mode = Mode::Insert;
    }

    // A — append at end of line
    fn append_at_line_end(&mut self) {
        self.cursor.col = self.current_line_len();
        self.mode = Mode::Insert;
    }

    // I — insert at first non-blank character
    fn insert_at_line_start(&mut self) {
        self.go_to_first_non_blank();
        self.mode = Mode::Insert;
    }

    // dd — delete entire current line
    fn delete_current_line(&mut self) {
        self.ensure_line_exists();
        let row = self.cursor.row as usize;
        self.document.lines.remove(row);
        if self.document.lines.is_empty() {
            self.document.lines.push(String::new());
        }
        if self.cursor.row as usize >= self.document.lines.len() {
            self.cursor.row = self.cursor.row.saturating_sub(1);
        }
        self.clamp_col();
        self.modified = true;
    }

    // D / d$ — delete from cursor to end of line
    fn delete_to_line_end(&mut self) {
        self.ensure_line_exists();
        let row = self.cursor.row as usize;
        let byte_idx = col_to_byte(&self.document.lines[row], self.cursor.col);
        self.document.lines[row].truncate(byte_idx);
        self.clamp_col();
        self.modified = true;
    }

    // d0 — delete from beginning of line to (not including) cursor
    fn delete_to_line_start(&mut self) {
        self.ensure_line_exists();
        let row = self.cursor.row as usize;
        let byte_idx = col_to_byte(&self.document.lines[row], self.cursor.col);
        self.document.lines[row].drain(..byte_idx);
        self.cursor.col = 0;
        self.modified = true;
    }

    // dw — delete from cursor to start of next word
    fn delete_word(&mut self) {
        self.ensure_line_exists();
        let row = self.cursor.row as usize;
        // We need to borrow `lines` immutably to compute byte indices, then mutably to drain.
        // Splitting the borrow into a scoped block lets the immutable borrow end first.
        let (start_byte, end_byte) = {
            let line = &self.document.lines[row];
            let chars: Vec<char> = line.chars().collect();
            let start = self.cursor.col as usize;
            let mut end = start;
            while end < chars.len() && !chars[end].is_whitespace() {
                end += 1;
            }
            while end < chars.len() && chars[end].is_whitespace() {
                end += 1;
            }
            (col_to_byte(line, start as u16), col_to_byte(line, end as u16))
        };
        self.document.lines[row].drain(start_byte..end_byte);
        self.clamp_col();
        self.modified = true;
    }

    // cc — clear the line and enter Insert (like Vim's S)
    fn change_line(&mut self) {
        self.ensure_line_exists();
        let row = self.cursor.row as usize;
        self.document.lines[row].clear();
        self.cursor.col = 0;
        self.mode = Mode::Insert;
        self.modified = true;
    }

    // C / c$ — delete to end of line, enter Insert
    fn change_to_line_end(&mut self) {
        self.delete_to_line_end();
        self.mode = Mode::Insert;
    }

    // cw — delete word, enter Insert
    fn change_word(&mut self) {
        self.delete_word();
        self.mode = Mode::Insert;
    }

    // -------------------------------------------------------------------------
    // Phase 8a — cell navigation
    // -------------------------------------------------------------------------

    // ]c — jump to the next # %% marker
    fn jump_to_next_cell(&mut self) {
        let row = self.cursor.row as usize;
        match cells::next_cell_marker(&self.document.lines, row) {
            Some(next) => {
                self.cursor.row = next as u16;
                self.cursor.col = 0;
            }
            None => self.message = Some("No next cell".into()),
        }
    }

    // [c — jump to the previous # %% marker
    fn jump_to_prev_cell(&mut self) {
        let row = self.cursor.row as usize;
        match cells::prev_cell_marker(&self.document.lines, row) {
            Some(prev) => {
                self.cursor.row = prev as u16;
                self.cursor.col = 0;
            }
            None => self.message = Some("No previous cell".into()),
        }
    }

    // Space+Enter — send the current cell to the REPL, starting it if needed.
    fn send_current_cell(&mut self) {
        if let Err(e) = self.try_send_current_cell() {
            self.message = Some(format!("REPL error: {e}"));
        }
    }

    fn try_send_current_cell(&mut self) -> anyhow::Result<()> {
        if !self.is_python_file() {
            self.message = Some("Space+Enter only works in .py files".into());
            return Ok(());
        }

        // Lazy start: spin up the REPL on the first send, not at startup.
        // This means non-.py files never pay the cost of spawning Python.
        if self.repl.is_none() {
            match Repl::start() {
                Ok(r) => {
                    self.repl = Some(r);
                    // Don't set message here — "Python started" noise is annoying;
                    // the panel appearing is signal enough.
                }
                Err(e) => {
                    self.message = Some(format!("{e}"));
                    return Ok(());
                }
            }
        }

        let row = self.cursor.row as usize;
        let range = cells::find_current_cell(&self.document.lines, row);
        let end = range.end.min(self.document.lines.len());
        let lines: Vec<String> = self.document.lines[range.start..end].to_vec();
        let n = lines.len();

        if let Some(repl) = &mut self.repl {
            repl.send_lines(&lines)?;
            self.message = Some(format!("▶ sent {n} lines"));
        }

        Ok(())
    }

    // Returns true if the open file is a Python file (by extension).
    // Used to decide whether to auto-start the REPL on Space+Enter.
    fn is_python_file(&self) -> bool {
        self.file_path
            .as_deref()
            .map(|p| p.ends_with(".py"))
            .unwrap_or(false)
    }

    // Drain any new lines from the REPL channel into output_buf.
    // Called from the main loop once per tick (before draw) so output appears promptly.
    pub fn drain_repl_output(&mut self) {
        if let Some(repl) = &self.repl {
            let new_lines = repl.poll_output();
            self.output_buf.extend(new_lines);
            // Cap so the buffer doesn't grow forever on chatty cells.
            const MAX_OUTPUT: usize = 1000;
            if self.output_buf.len() > MAX_OUTPUT {
                let excess = self.output_buf.len() - MAX_OUTPUT;
                self.output_buf.drain(..excess);
            }
        }
    }

    // How many screen rows to reserve for the output panel.
    // 0 when no REPL is running and there's nothing to show.
    fn panel_rows(&self) -> usize {
        if self.repl.is_some() || !self.output_buf.is_empty() {
            OUTPUT_PANEL_HEIGHT + 1 // +1 for the "── REPL ──" separator line
        } else {
            0
        }
    }

    // Returns a short cell indicator for the status bar, e.g. " [Cell 2/4]".
    // Returns an empty string if the document has no cell markers.
    fn cell_status_text(&self) -> String {
        let has_markers = self.document.lines.iter().any(|l| cells::is_cell_marker(l));
        if !has_markers {
            return String::new();
        }
        let row = self.cursor.row as usize;
        let num = cells::current_cell_number(&self.document.lines, row);
        let total = cells::count_cells(&self.document.lines);
        format!(" [Cell {num}/{total}]")
    }

    // -------------------------------------------------------------------------
    // Phase 5 — save & command mode
    // -------------------------------------------------------------------------

    fn save(&mut self) -> Result<()> {
        // Clone so we don't hold a borrow on `self` while also calling `self.message = ...`
        match self.file_path.clone() {
            None => {
                self.message = Some("No file name — open a file or use :w <filename>".into());
            }
            Some(path) => {
                self.document.save(&path)?;
                self.modified = false;
                self.message = Some(format!("\"{}\" written", path));
            }
        }
        Ok(())
    }

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
                    self.message =
                        Some("Unsaved changes — use :wq to save, :q! to discard".into());
                    Ok(false)
                } else {
                    Ok(true)
                }
            }
            "wq" | "x" => {
                self.save()?;
                Ok(true)
            }
            "q!" => Ok(true),
            other => {
                self.message = Some(format!("Unknown command: {other}"));
                Ok(false)
            }
        }
    }

    // -------------------------------------------------------------------------
    // Phase 6 — scrolling
    // -------------------------------------------------------------------------

    // Adjusts scroll_offset so the cursor stays inside the viewport with a small
    // margin (scroll_off) — like Vim's `set scrolloff=3`.
    fn scroll_to_cursor(&mut self, content_rows: usize) {
        if content_rows == 0 {
            return;
        }
        let row = self.cursor.row as usize;
        let scroll_off: usize = 3;
        // Scroll up if cursor is too close to the top.
        if row < self.scroll_offset + scroll_off {
            self.scroll_offset = row.saturating_sub(scroll_off);
        }
        // Scroll down if cursor is too close to the bottom.
        let min_offset = (row + scroll_off + 1).saturating_sub(content_rows);
        if self.scroll_offset < min_offset {
            self.scroll_offset = min_offset;
        }
    }

    // -------------------------------------------------------------------------
    // Rendering
    // -------------------------------------------------------------------------

    // draw() takes &mut self because scroll_to_cursor may modify scroll_offset.
    // Any method that changes a field — even indirectly — must be &mut self.
    pub fn draw(&mut self) -> Result<()> {
        let mut out = stdout();
        let (cols, rows) = terminal::size()?;
        // panel_h is 0 when no REPL is running, or OUTPUT_PANEL_HEIGHT+1 once it starts.
        // Subtracting it from content_rows shrinks the document area to make room.
        let panel_h = self.panel_rows();
        let content_rows = (rows as usize).saturating_sub(1 + panel_h);

        self.scroll_to_cursor(content_rows);

        execute!(out, cursor::Hide, cursor::MoveTo(0, 0), terminal::Clear(ClearType::All))?;

        // --- Document area ---
        for screen_row in 0..content_rows {
            let doc_row = self.scroll_offset + screen_row;
            execute!(
                out,
                cursor::MoveTo(0, screen_row as u16),
                terminal::Clear(ClearType::CurrentLine)
            )?;
            match self.document.lines.get(doc_row) {
                Some(text) if cells::is_cell_marker(text) => {
                    let divider = render_cell_divider(text, cols as usize);
                    execute!(
                        out,
                        SetAttribute(Attribute::Bold),
                        Print(divider),
                        SetAttribute(Attribute::Reset),
                    )?;
                }
                Some(text) => {
                    let visible: String = text.chars().take(cols as usize).collect();
                    execute!(out, Print(visible))?;
                }
                None => {
                    execute!(out, Print("~"))?;
                }
            }
        }

        // --- REPL output panel ---
        if panel_h > 0 {
            // Separator line
            let sep_row = content_rows as u16;
            execute!(
                out,
                cursor::MoveTo(0, sep_row),
                terminal::Clear(ClearType::CurrentLine)
            )?;
            let header = "── REPL ";
            let fill = "─".repeat((cols as usize).saturating_sub(header.len()));
            execute!(
                out,
                SetAttribute(Attribute::Bold),
                Print(format!("{header}{fill}")),
                SetAttribute(Attribute::Reset),
            )?;

            // Show the last OUTPUT_PANEL_HEIGHT lines of output.
            let skip = self.output_buf.len().saturating_sub(OUTPUT_PANEL_HEIGHT);
            let visible_lines: Vec<&String> = self.output_buf.iter().skip(skip).collect();
            for i in 0..OUTPUT_PANEL_HEIGHT {
                let screen_row = content_rows as u16 + 1 + i as u16;
                execute!(
                    out,
                    cursor::MoveTo(0, screen_row),
                    terminal::Clear(ClearType::CurrentLine)
                )?;
                if let Some(line) = visible_lines.get(i) {
                    let visible: String = line.chars().take(cols as usize).collect();
                    execute!(out, Print(visible))?;
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
                execute!(out, Print(format!(":{}", self.command_buf)))?;
                // Place the cursor right after the typed text on the command line.
                let cmd_col = self.command_buf.len() as u16 + 1; // +1 for the ':'
                execute!(out, cursor::MoveTo(cmd_col, status_row), cursor::Show)?;
            }
            _ => {
                let mode_label = match self.mode {
                    Mode::Normal => "-- NORMAL --".to_string(),
                    Mode::Insert => "-- INSERT --".to_string(),
                    // Show which operator is pending so the user can see what they typed.
                    Mode::OperatorPending(op) => format!("-- {op}? --"),
                    Mode::Command => unreachable!(),
                };
                let modified = if self.modified { " [+]" } else { "" };
                let count_hint = if self.count_buf.is_empty() {
                    String::new()
                } else {
                    format!("  {}", self.count_buf)
                };
                let cell_info = self.cell_status_text();
                match &self.message {
                    Some(msg) => {
                        execute!(out, Print(format!("{mode_label}{modified}{cell_info}  {msg}")))?
                    }
                    None => {
                        execute!(out, Print(format!("{mode_label}{modified}{cell_info}{count_hint}")))?
                    }
                }

                // Convert document cursor position to screen position.
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

    // -------------------------------------------------------------------------
    // Event dispatch
    // -------------------------------------------------------------------------

    pub fn handle_key(&mut self, key: KeyEvent) -> Result<bool> {
        self.message = None; // any keypress clears the transient message
        match self.mode {
            Mode::Normal => self.handle_normal(key),
            Mode::Insert => self.handle_insert(key),
            Mode::Command => self.handle_command(key),
            Mode::OperatorPending(op) => self.handle_operator_pending(op, key),
        }
    }

    fn handle_normal(&mut self, key: KeyEvent) -> Result<bool> {
        // --- Count accumulation ---
        // Digits build up a repeat count. '0' is special: it means go-to-BOL when the
        // count buffer is empty, but is a regular digit otherwise.
        if let KeyCode::Char(d @ '1'..='9') = key.code {
            self.pending_char = None; // a digit cancels any partial two-key sequence
            self.count_buf.push(d);
            return Ok(false);
        }
        if key.code == KeyCode::Char('0') {
            if self.count_buf.is_empty() {
                self.pending_char = None;
                self.go_to_line_start();
                return Ok(false);
            } else {
                self.count_buf.push('0');
                return Ok(false);
            }
        }

        // Parse (and clear) the count. has_count lets us distinguish "G" (last line) from "1G" (line 1).
        let has_count = !self.count_buf.is_empty();
        let count = if has_count {
            self.count_buf.parse::<u32>().unwrap_or(1)
        } else {
            1
        };
        self.count_buf.clear();

        // --- Handle partial two-key sequences (e.g. 'gg') ---
        // Option::take() is an idiom that reads the value and replaces it with None in one step,
        // so we don't hold a reference to self.pending_char while we use the value.
        if let Some(pending) = self.pending_char.take() {
            match (pending, key.code) {
                ('g', KeyCode::Char('g')) => { self.go_to_first_line(); return Ok(false); }
                (']', KeyCode::Char('c')) => { self.jump_to_next_cell(); return Ok(false); }
                ('[', KeyCode::Char('c')) => { self.jump_to_prev_cell(); return Ok(false); }
                // Space+Enter sends the current cell to the REPL (stub in 8a, real in 8b).
                (' ', KeyCode::Enter)    => { self.send_current_cell(); return Ok(false); }
                _ => {
                    // Unknown two-key sequence — ignore the pending char and fall through
                    // to handle the current key normally.
                }
            }
        }

        // --- Main Normal-mode dispatch ---
        match key.code {
            // Movement
            KeyCode::Char('h') | KeyCode::Left => {
                for _ in 0..count {
                    self.move_left();
                }
            }
            KeyCode::Char('l') | KeyCode::Right => {
                for _ in 0..count {
                    self.move_right();
                }
            }
            KeyCode::Char('k') | KeyCode::Up => {
                for _ in 0..count {
                    self.move_up();
                }
            }
            KeyCode::Char('j') | KeyCode::Down => {
                for _ in 0..count {
                    self.move_down();
                }
            }
            KeyCode::Char('w') => {
                for _ in 0..count {
                    self.word_forward();
                }
            }
            KeyCode::Char('b') => {
                for _ in 0..count {
                    self.word_backward();
                }
            }
            KeyCode::Char('e') => {
                for _ in 0..count {
                    self.word_end();
                }
            }
            KeyCode::Char('$') => self.go_to_line_end(),
            KeyCode::Char('^') => self.go_to_first_non_blank(),
            KeyCode::Char('g') => {
                // First keypress of 'gg' — wait for the second.
                self.pending_char = Some('g');
            }
            KeyCode::Char('G') => {
                if has_count {
                    // 5G → go to line 5 (1-indexed → 0-indexed)
                    self.go_to_line(count.saturating_sub(1) as usize);
                } else {
                    self.go_to_last_line();
                }
            }

            // Enter Insert mode
            KeyCode::Char('i') => self.enter_insert_mode(),
            KeyCode::Char('I') => self.insert_at_line_start(),
            KeyCode::Char('a') => self.append_after_cursor(),
            KeyCode::Char('A') => self.append_at_line_end(),
            KeyCode::Char('o') => self.open_line_below(),
            KeyCode::Char('O') => self.open_line_above(),

            // Single-key edits
            KeyCode::Char('x') => {
                for _ in 0..count {
                    self.delete_char_at_cursor();
                }
            }
            // s = substitute char: delete char, enter Insert (like xi)
            KeyCode::Char('s') => {
                self.delete_char_at_cursor();
                self.enter_insert_mode();
            }
            // D = delete to end of line (same as d$)
            KeyCode::Char('D') => self.delete_to_line_end(),
            // C = change to end of line (same as c$)
            KeyCode::Char('C') => self.change_to_line_end(),

            // Operators — enter OperatorPending to wait for the motion
            KeyCode::Char('d') => self.mode = Mode::OperatorPending('d'),
            KeyCode::Char('c') => self.mode = Mode::OperatorPending('c'),

            // Cell navigation — first key of a two-key sequence
            KeyCode::Char(']') => { self.pending_char = Some(']'); }
            KeyCode::Char('[') => { self.pending_char = Some('['); }
            // Space starts the Space+Enter "send cell" chord
            KeyCode::Char(' ') => { self.pending_char = Some(' '); }

            // Command mode
            KeyCode::Char(':') => {
                self.mode = Mode::Command;
                self.command_buf.clear();
            }

            // Quick quit (already familiar from Phase 0; :q/:wq/:q! are the canonical way)
            KeyCode::Char('q') if key.modifiers.is_empty() => return Ok(true),

            _ => {}
        }

        Ok(false)
    }

    fn handle_insert(&mut self, key: KeyEvent) -> Result<bool> {
        match key.code {
            KeyCode::Esc => self.enter_normal_mode(),
            KeyCode::Enter => self.insert_newline(),
            KeyCode::Backspace => self.backspace(),
            // Arrow keys work in Insert mode too
            KeyCode::Left => self.move_left(),
            KeyCode::Right => self.move_right(),
            KeyCode::Up => self.move_up(),
            KeyCode::Down => self.move_down(),
            KeyCode::Char(ch)
                if !key.modifiers.contains(KeyModifiers::CONTROL)
                    && !key.modifiers.contains(KeyModifiers::ALT) =>
            {
                self.insert_char(ch);
            }
            _ => {}
        }
        Ok(false)
    }

    fn handle_command(&mut self, key: KeyEvent) -> Result<bool> {
        match key.code {
            KeyCode::Esc => {
                self.mode = Mode::Normal;
                self.command_buf.clear();
            }
            KeyCode::Enter => return self.execute_command(),
            KeyCode::Backspace => {
                if self.command_buf.is_empty() {
                    // Backspace on an empty command line cancels, just like Vim.
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
        }
        Ok(false)
    }

    // Called when we're in OperatorPending(op) and the next key arrives.
    // `op` is the operator char ('d', 'c', …); the key supplies the motion.
    fn handle_operator_pending(&mut self, op: char, key: KeyEvent) -> Result<bool> {
        // Most paths return to Normal. We set it here and any Insert-triggering
        // paths (change ops) will overwrite it.
        self.mode = Mode::Normal;

        match key.code {
            KeyCode::Esc => {} // cancel operator

            // Doubled operator = operate on whole line  (dd, cc)
            KeyCode::Char('d') if op == 'd' => self.delete_current_line(),
            KeyCode::Char('c') if op == 'c' => self.change_line(),

            // Motions
            KeyCode::Char('w') => match op {
                'd' => self.delete_word(),
                'c' => self.change_word(),
                _ => {}
            },
            KeyCode::Char('$') => match op {
                'd' => self.delete_to_line_end(),
                'c' => self.change_to_line_end(),
                _ => {}
            },
            KeyCode::Char('0') => match op {
                'd' => self.delete_to_line_start(),
                _ => {}
            },
            KeyCode::Char('^') => {
                // d^ / c^ — operate from cursor to first non-blank
                let target = {
                    let row = self.cursor.row as usize;
                    self.document
                        .lines
                        .get(row)
                        .and_then(|l| l.chars().position(|c| !c.is_whitespace()))
                        .unwrap_or(0) as u16
                };
                if target < self.cursor.col {
                    let row = self.cursor.row as usize;
                    let start = col_to_byte(&self.document.lines[row], target);
                    let end = col_to_byte(&self.document.lines[row], self.cursor.col);
                    self.document.lines[row].drain(start..end);
                    self.cursor.col = target;
                    self.modified = true;
                    if op == 'c' {
                        self.mode = Mode::Insert;
                    }
                }
            }

            _ => {} // unknown motion — silently cancel
        }

        Ok(false)
    }
}

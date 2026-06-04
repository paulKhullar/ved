use std::io::{stdout, Write};

use anyhow::Result;
use crossterm::{
    cursor,
    event::{KeyCode, KeyEvent, KeyModifiers},
    execute,
    style::{
        Attribute, Color, Print, ResetColor, SetAttribute, SetBackgroundColor, SetForegroundColor,
    },
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
    Visual(VisualKind),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VisualKind {
    Char,
    Line,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Position {
    pub row: u16,
    pub col: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RangeKind {
    Char,
    Line,
}

// A selection in document coordinates.
// `end` is exclusive (Rust-style), which keeps slicing math simple.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Range {
    start: Position,
    end: Position,
    kind: RangeKind,
}

impl Range {
    fn normalized(self) -> Self {
        match self.kind {
            RangeKind::Line => {
                if self.start.row <= self.end.row {
                    self
                } else {
                    Self {
                        start: self.end,
                        end: self.start,
                        kind: self.kind,
                    }
                }
            }
            RangeKind::Char => {
                if (self.start.row, self.start.col) <= (self.end.row, self.end.col) {
                    self
                } else {
                    Self {
                        start: self.end,
                        end: self.start,
                        kind: self.kind,
                    }
                }
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Operator {
    Delete,
    Change,
    Yank,
}

pub struct Editor {
    pub cursor: Position,
    scroll_offset: usize,
    document: Document,
    file_path: Option<String>,
    treat_as_python: bool,
    mode: Mode,
    modified: bool,
    command_buf: String,
    message: Option<String>,
    register: Option<(String, RangeKind)>, // Vim unnamed register
    // Accumulates digit keypresses before a command, e.g. "10" in "10j".
    count_buf: String,
    // Tracks a partial two-key sequence. Currently only used for 'g' → 'g' (go to first line).
    // Option::take() is used to read-and-clear it atomically — a handy Rust idiom.
    pending_char: Option<char>,
    visual_anchor: Option<Position>,

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

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct SpanStyle {
    fg: Option<Color>,
    bg: Option<Color>,
    bold: bool,
    dim: bool,
    reverse: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Span {
    text: String,
    style: SpanStyle,
}

impl Span {
    fn new(text: impl Into<String>, style: SpanStyle) -> Self {
        Self {
            text: text.into(),
            style,
        }
    }
}

impl Editor {
    pub fn new(document: Document, file_path: Option<String>, treat_as_python: bool) -> Self {
        Self {
            cursor: Position::default(),
            scroll_offset: 0,
            document,
            file_path,
            treat_as_python,
            mode: Mode::Normal,
            modified: false,
            command_buf: String::new(),
            message: None,
            register: None,
            count_buf: String::new(),
            pending_char: None,
            visual_anchor: None,
            repl: None,
            output_buf: Vec::new(),
        }
    }

    pub fn set_message(&mut self, msg: impl Into<String>) {
        self.message = Some(msg.into());
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

    // -------------------------------------------------------------------------
    // Phase 9a — motions compute ranges (no mutation)
    // -------------------------------------------------------------------------

    fn range_current_line(&self) -> Range {
        let row = self.cursor.row;
        Range {
            start: Position { row, col: 0 },
            end: Position {
                row: row.saturating_add(1),
                col: 0,
            },
            kind: RangeKind::Line,
        }
    }

    fn range_to_line_end(&self) -> Range {
        let row = self.cursor.row;
        let end_col = self.current_line_len();
        Range {
            start: self.cursor,
            end: Position { row, col: end_col },
            kind: RangeKind::Char,
        }
    }

    fn range_to_line_start(&self) -> Range {
        let row = self.cursor.row;
        Range {
            start: Position { row, col: 0 },
            end: self.cursor,
            kind: RangeKind::Char,
        }
        .normalized()
    }

    fn range_word(&self) -> Range {
        let row = self.cursor.row as usize;
        let start = self.cursor;
        let mut end_col = start.col;

        if let Some(line) = self.document.lines.get(row) {
            let chars: Vec<char> = line.chars().collect();
            let mut end = start.col as usize;
            while end < chars.len() && !chars[end].is_whitespace() {
                end += 1;
            }
            while end < chars.len() && chars[end].is_whitespace() {
                end += 1;
            }
            end_col = end.min(u16::MAX as usize) as u16;
        }

        Range {
            start,
            end: Position {
                row: start.row,
                col: end_col,
            },
            kind: RangeKind::Char,
        }
    }

    fn range_to_first_nonblank(&self) -> Option<Range> {
        let row = self.cursor.row as usize;
        let target = self
            .document
            .lines
            .get(row)
            .and_then(|l| l.chars().position(|c| !c.is_whitespace()))
            .unwrap_or(0) as u16;

        // Preserve existing behavior: only d^/c^ when target is to the *left*.
        if target < self.cursor.col {
            Some(Range {
                start: Position {
                    row: self.cursor.row,
                    col: target,
                },
                end: self.cursor,
                kind: RangeKind::Char,
            })
        } else {
            None
        }
    }

    fn visual_range(&self, kind: VisualKind) -> Option<Range> {
        let anchor = self.visual_anchor?;
        match kind {
            VisualKind::Line => {
                let a = anchor.row.min(self.cursor.row);
                let b = anchor.row.max(self.cursor.row);
                Some(Range {
                    start: Position { row: a, col: 0 },
                    end: Position {
                        row: b.saturating_add(1),
                        col: 0,
                    },
                    kind: RangeKind::Line,
                })
            }
            VisualKind::Char => {
                let (start, end_inclusive) = if (anchor.row, anchor.col) <= (self.cursor.row, self.cursor.col) {
                    (anchor, self.cursor)
                } else {
                    (self.cursor, anchor)
                };

                let end_row = end_inclusive.row as usize;
                let line_len = self.line_len(end_row);
                let end_exclusive_col = end_inclusive.col.saturating_add(1).min(line_len);

                Some(
                    Range {
                        start,
                        end: Position {
                            row: end_inclusive.row,
                            col: end_exclusive_col,
                        },
                        kind: RangeKind::Char,
                    }
                    .normalized(),
                )
            }
        }
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
    // Phase 9a — core editing primitives
    // -------------------------------------------------------------------------

    #[allow(dead_code)]
    fn text_in(&self, range: Range) -> String {
        let range = range.normalized();
        match range.kind {
            RangeKind::Line => {
                let start = range.start.row as usize;
                let end = range.end.row as usize;
                let end = end.min(self.document.lines.len());
                if start >= end {
                    return String::new();
                }
                self.document.lines[start..end].join("\n") + "\n"
            }
            RangeKind::Char => {
                let start_row = range.start.row as usize;
                let end_row = range.end.row as usize;
                if start_row >= self.document.lines.len() || end_row >= self.document.lines.len()
                {
                    return String::new();
                }

                if start_row == end_row {
                    let line = &self.document.lines[start_row];
                    let a = col_to_byte(line, range.start.col);
                    let b = col_to_byte(line, range.end.col);
                    return line[a..b].to_string();
                }

                let mut out = String::new();
                // start line tail
                {
                    let line = &self.document.lines[start_row];
                    let a = col_to_byte(line, range.start.col);
                    out.push_str(&line[a..]);
                    out.push('\n');
                }
                // full middle lines
                for row in (start_row + 1)..end_row {
                    out.push_str(&self.document.lines[row]);
                    out.push('\n');
                }
                // end line head
                {
                    let line = &self.document.lines[end_row];
                    let b = col_to_byte(line, range.end.col);
                    out.push_str(&line[..b]);
                }
                out
            }
        }
    }

    fn delete_range(&mut self, range: Range) {
        let range = range.normalized();
        self.ensure_line_exists();

        match range.kind {
            RangeKind::Line => {
                let start_row = range.start.row as usize;
                let end_row = range.end.row as usize;
                let end_row = end_row.min(self.document.lines.len());

                // Preserve dd behavior: keep the current column when possible.
                let desired_col = self.cursor.col;

                if start_row < end_row {
                    self.document.lines.drain(start_row..end_row);
                }

                // dd on the last line should still leave the document editable.
                if self.document.lines.is_empty() {
                    self.document.lines.push(String::new());
                }

                let last = self.document.lines.len().saturating_sub(1);
                self.cursor.row = start_row.min(last) as u16;
                self.cursor.col = desired_col;
                self.clamp_col();
                self.modified = true;
            }
            RangeKind::Char => {
                let start_row = range.start.row as usize;
                let end_row = range.end.row as usize;
                if start_row >= self.document.lines.len() || end_row >= self.document.lines.len() {
                    return;
                }

                if start_row == end_row {
                    let line = &mut self.document.lines[start_row];
                    let a = col_to_byte(line, range.start.col);
                    let b = col_to_byte(line, range.end.col);
                    line.drain(a..b);
                } else {
                    // Merge: prefix of start line + suffix of end line, then remove the middle lines.
                    let (start_prefix, end_suffix) = {
                        let start_line = &self.document.lines[start_row];
                        let end_line = &self.document.lines[end_row];
                        let a = col_to_byte(start_line, range.start.col);
                        let b = col_to_byte(end_line, range.end.col);
                        (start_line[..a].to_string(), end_line[b..].to_string())
                    };

                    // Remove end_row down to start_row+1 (inclusive) so indices stay valid.
                    self.document.lines.drain((start_row + 1)..=end_row);
                    self.document.lines[start_row] = start_prefix + &end_suffix;
                }

                self.cursor = range.start;
                self.clamp_col();
                self.modified = true;
            }
        }
    }

    fn apply_operator(&mut self, op: Operator, range: Range) {
        let range = range.normalized();

        // Vim behavior: d/c/y all update the unnamed register with the operated text.
        // For delete/change, capture the text *before* mutation.
        let yanked_text = self.text_in(range);
        if matches!(op, Operator::Delete | Operator::Change | Operator::Yank) {
            self.register = Some((yanked_text, range.kind));
        }

        match op {
            Operator::Delete => {
                self.delete_range(range);
                self.mode = Mode::Normal;
            }
            Operator::Change => match range.kind {
                RangeKind::Char => {
                    self.delete_range(range);
                    self.mode = Mode::Insert;
                }
                RangeKind::Line => {
                    // Preserve cc semantics: replace the selected lines with one empty line,
                    // then enter Insert at column 0.
                    let start_row = range.start.row as usize;
                    let end_row = (range.end.row as usize).min(self.document.lines.len());

                    if start_row < end_row {
                        self.document.lines.drain(start_row..end_row);
                    }

                    let row = start_row.min(self.document.lines.len());
                    self.document.lines.insert(row, String::new());
                    self.cursor.row = row as u16;
                    self.cursor.col = 0;
                    self.mode = Mode::Insert;
                    self.modified = true;
                }
            },
            Operator::Yank => {
                self.mode = Mode::Normal;
                // yanking does not modify the buffer
            }
        }
    }

    // -------------------------------------------------------------------------
    // Phase 9b — paste from register
    // -------------------------------------------------------------------------

    fn paste_after(&mut self) {
        let Some((text, kind)) = self.register.clone() else {
            return;
        };

        match kind {
            RangeKind::Line => self.paste_linewise(text, true),
            RangeKind::Char => self.paste_charwise(&text, true),
        }
    }

    fn paste_before(&mut self) {
        let Some((text, kind)) = self.register.clone() else {
            return;
        };

        match kind {
            RangeKind::Line => self.paste_linewise(text, false),
            RangeKind::Char => self.paste_charwise(&text, false),
        }
    }

    fn paste_linewise(&mut self, text: String, after: bool) {
        self.ensure_line_exists();
        let mut lines: Vec<String> = text.lines().map(|l| l.to_string()).collect();
        if lines.is_empty() {
            return;
        }

        let row = self.cursor.row as usize;
        let insert_at = if after { row + 1 } else { row };
        let insert_at = insert_at.min(self.document.lines.len());

        for (i, line) in lines.drain(..).enumerate() {
            self.document.lines.insert(insert_at + i, line);
        }

        self.cursor.row = insert_at as u16;
        self.cursor.col = 0;
        self.modified = true;
    }

    fn paste_charwise(&mut self, text: &str, after: bool) {
        self.ensure_line_exists();

        let row = self.cursor.row as usize;
        let line_len = self.line_len(row);

        let mut col = self.cursor.col;
        if after {
            // Paste after the cursor; clamp to end-of-line.
            col = col.saturating_add(1).min(line_len);
        } else {
            col = col.min(line_len);
        }

        let cursor_after = self.insert_text_at(Position { row: self.cursor.row, col }, text);
        self.cursor = cursor_after;
        self.modified = true;
    }

    // Insert arbitrary text at a character position, splitting lines on '\n' if needed.
    // Returns the cursor position at the last inserted character (Vim-like).
    fn insert_text_at(&mut self, pos: Position, text: &str) -> Position {
        let row = pos.row as usize;
        if row >= self.document.lines.len() {
            self.document.lines.push(String::new());
        }

        if !text.contains('\n') {
            let line = &mut self.document.lines[row];
            let byte_idx = col_to_byte(line, pos.col);
            line.insert_str(byte_idx, text);
            let inserted = text.chars().count().min(u16::MAX as usize) as u16;
            return Position {
                row: pos.row,
                col: pos.col.saturating_add(inserted).saturating_sub(1),
            };
        }

        let parts: Vec<&str> = text.split('\n').collect();
        let (prefix, suffix) = {
            let line = &self.document.lines[row];
            let byte_idx = col_to_byte(line, pos.col);
            (line[..byte_idx].to_string(), line[byte_idx..].to_string())
        };

        self.document.lines[row] = format!("{}{}", prefix, parts[0]);

        for (i, part) in parts.iter().enumerate().skip(1) {
            let insert_row = row + i;
            self.document.lines.insert(insert_row, part.to_string());
        }

        let last_row = row + parts.len() - 1;
        self.document.lines[last_row].push_str(&suffix);

        let last_part_len = parts
            .last()
            .map(|p| p.chars().count().min(u16::MAX as usize) as u16)
            .unwrap_or(0);

        Position {
            row: last_row as u16,
            col: last_part_len.saturating_sub(1),
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
            repl.send_cell(&lines)?;
            self.message = Some(format!("▶ sent {n} lines"));
        }

        Ok(())
    }

    // Returns true if the open file is a Python file (by extension).
    // Used to decide whether to auto-start the REPL on Space+Enter.
    fn is_python_file(&self) -> bool {
        if self.treat_as_python {
            return true;
        }
        self.file_path
            .as_deref()
            .map(|p| p.ends_with(".py"))
            .unwrap_or(false)
    }

    // Drain any new events from the REPL channel into output_buf.
    // Called from the main loop once per tick. Also checks for timeout.
    pub fn drain_repl_output(&mut self) -> bool {
        let mut changed = false;
        if let Some(repl) = &mut self.repl {
            // Timeout check: if the sentinel hasn't arrived in time, give up waiting.
            // Capture elapsed before mark_idle() resets the timer.
            if repl.is_timed_out() {
                let secs = repl.elapsed_secs();
                repl.mark_idle();
                self.output_buf
                    .push(format!("[timed out after {secs}s — Ctrl+C to interrupt]"));
                changed = true;
            }

            let (new_lines, done) = repl.poll_output();
            if !new_lines.is_empty() {
                self.output_buf.extend(new_lines);
                changed = true;
            }

            // Done = sentinel received = cell finished cleanly.
            if done {
                repl.mark_idle();
                changed = true;
            }

            // Cap so a chatty cell doesn't grow the buffer forever.
            const MAX: usize = 1000;
            if self.output_buf.len() > MAX {
                let excess = self.output_buf.len() - MAX;
                self.output_buf.drain(..excess);
                changed = true;
            }
        }
        changed
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

    // Returns a short indicator for the status bar.
    // When a cell is running, shows elapsed time instead of the cell number.
    fn cell_status_text(&self) -> String {
        // Running indicator takes priority — most useful thing to show right now.
        if let Some(repl) = &self.repl {
            if repl.is_running() {
                let secs = repl.elapsed_secs();
                return format!(" [running… {secs}s]");
            }
        }
        // Otherwise show which cell the cursor is in.
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

        let verb = cmd.split_whitespace().next().unwrap_or("");
        let arg = cmd.get(verb.len()..).unwrap_or("").trim();

        match verb {
            "w" | "write" => {
                if arg.is_empty() {
                    self.save()?;
                } else {
                    self.file_path = Some(arg.to_string());
                    self.save()?;
                }
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
                if arg.is_empty() {
                    self.save()?;
                    Ok(true)
                } else {
                    self.file_path = Some(arg.to_string());
                    self.save()?;
                    Ok(true)
                }
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

        // Avoid clearing the entire screen every frame; we clear per-line below.
        execute!(out, cursor::Hide, cursor::MoveTo(0, 0))?;

        // Phase 9d: draw each line as a sequence of (text, style) spans.
        // Visual selection is applied as a reverse-video overlay on top of spans.
        let visual_selection = match self.mode {
            Mode::Visual(kind) => self.visual_range(kind),
            _ => None,
        };

        // --- Document area ---
        for screen_row in 0..content_rows {
            let doc_row = self.scroll_offset + screen_row;
            execute!(
                out,
                cursor::MoveTo(0, screen_row as u16),
                terminal::Clear(ClearType::CurrentLine)
            )?;
            let mut spans = self.spans_for_document_line(doc_row, cols as usize);
            spans = truncate_spans(&spans, cols as usize);

            if let Some(sel) = visual_selection {
                if let Some((start, end)) = selection_segment_for_row(self, sel, doc_row) {
                    spans = apply_reverse_overlay(&spans, start, end);
                    spans = truncate_spans(&spans, cols as usize);
                }
            }

            render_spans(&mut out, &spans, cols as usize)?;
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
            let header_style = SpanStyle {
                fg: Some(Color::Magenta),
                bold: true,
                ..SpanStyle::default()
            };
            render_spans(
                &mut out,
                &[Span::new(format!("{header}{fill}"), header_style)],
                cols as usize,
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
                    let style = SpanStyle {
                        fg: Some(Color::DarkGrey),
                        dim: true,
                        ..SpanStyle::default()
                    };
                    let spans = truncate_spans(&[Span::new(line.as_str(), style)], cols as usize);
                    render_spans(&mut out, &spans, cols as usize)?;
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
                    Mode::Visual(kind) => match kind {
                        VisualKind::Char => "-- VISUAL --".to_string(),
                        VisualKind::Line => "-- VISUAL LINE --".to_string(),
                    },
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

    fn spans_for_document_line(&self, doc_row: usize, cols: usize) -> Vec<Span> {
        let marker_style = SpanStyle {
            fg: Some(Color::DarkYellow),
            bold: true,
            ..SpanStyle::default()
        };
        let tilde_style = SpanStyle {
            fg: Some(Color::DarkGrey),
            dim: true,
            ..SpanStyle::default()
        };

        match self.document.lines.get(doc_row) {
            Some(text) if cells::is_cell_marker(text) => {
                let divider = render_cell_divider(text, cols);
                vec![Span::new(divider, marker_style)]
            }
            Some(text) => {
                if self.is_python_file() {
                    python_spans(text)
                } else {
                    vec![Span::new(text, SpanStyle::default())]
                }
            }
            None => vec![Span::new("~", tilde_style)],
        }
    }

    // -------------------------------------------------------------------------
    // Event dispatch
    // -------------------------------------------------------------------------

    pub fn handle_key(&mut self, key: KeyEvent) -> Result<bool> {
        self.message = None; // any keypress clears the transient message

        // Ctrl+C interrupts a running REPL cell regardless of which mode we're in.
        // This works because the reader thread + poll design keeps the UI responsive —
        // we can always process keypresses even while Python is executing.
        if key.code == KeyCode::Char('c')
            && key.modifiers.contains(KeyModifiers::CONTROL)
        {
            if self.repl.as_ref().map(|r| r.is_running()).unwrap_or(false) {
                if let Some(repl) = &mut self.repl {
                    repl.interrupt();
                    self.output_buf.push("[interrupted]".into());
                }
                self.message = Some("Cell interrupted".into());
                return Ok(false);
            }
        }

        match self.mode {
            Mode::Normal => self.handle_normal(key),
            Mode::Insert => self.handle_insert(key),
            Mode::Command => self.handle_command(key),
            Mode::OperatorPending(op) => self.handle_operator_pending(op, key),
            Mode::Visual(kind) => self.handle_visual(kind, key),
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
            KeyCode::Char('D') => {
                let range = self.range_to_line_end();
                self.apply_operator(Operator::Delete, range);
            }
            // C = change to end of line (same as c$)
            KeyCode::Char('C') => {
                let range = self.range_to_line_end();
                self.apply_operator(Operator::Change, range);
            }

            // Operators — enter OperatorPending to wait for the motion
            KeyCode::Char('d') => self.mode = Mode::OperatorPending('d'),
            KeyCode::Char('c') => self.mode = Mode::OperatorPending('c'),
            KeyCode::Char('y') => self.mode = Mode::OperatorPending('y'),

            // Paste
            KeyCode::Char('p') => {
                for _ in 0..count {
                    self.paste_after();
                }
            }
            KeyCode::Char('P') => {
                for _ in 0..count {
                    self.paste_before();
                }
            }

            // Visual mode
            KeyCode::Char('v') => {
                self.visual_anchor = Some(self.cursor);
                self.mode = Mode::Visual(VisualKind::Char);
            }
            KeyCode::Char('V') => {
                self.visual_anchor = Some(Position { row: self.cursor.row, col: 0 });
                self.cursor.col = 0;
                self.mode = Mode::Visual(VisualKind::Line);
            }

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

    fn handle_visual(&mut self, kind: VisualKind, key: KeyEvent) -> Result<bool> {
        // Visual mode doesn't currently render highlights (that's Phase 9d),
        // but we do make d/c/y operate on the anchor↔cursor range.
        match key.code {
            KeyCode::Esc => {
                self.visual_anchor = None;
                self.mode = Mode::Normal;
            }
            KeyCode::Char('v') if kind == VisualKind::Char => {
                self.visual_anchor = None;
                self.mode = Mode::Normal;
            }
            KeyCode::Char('V') if kind == VisualKind::Line => {
                self.visual_anchor = None;
                self.mode = Mode::Normal;
            }
            KeyCode::Char('h') | KeyCode::Left => self.move_left(),
            KeyCode::Char('l') | KeyCode::Right => self.move_right(),
            KeyCode::Char('k') | KeyCode::Up => self.move_up(),
            KeyCode::Char('j') | KeyCode::Down => self.move_down(),
            KeyCode::Char('w') => self.word_forward(),
            KeyCode::Char('b') => self.word_backward(),
            KeyCode::Char('e') => self.word_end(),
            KeyCode::Char('$') => self.go_to_line_end(),
            KeyCode::Char('^') => self.go_to_first_non_blank(),
            KeyCode::Char('d') | KeyCode::Char('c') | KeyCode::Char('y') => {
                let op = match key.code {
                    KeyCode::Char('d') => Operator::Delete,
                    KeyCode::Char('c') => Operator::Change,
                    KeyCode::Char('y') => Operator::Yank,
                    _ => unreachable!(),
                };
                if let Some(range) = self.visual_range(kind) {
                    self.apply_operator(op, range);
                }
                self.visual_anchor = None;
                // apply_operator already set mode appropriately.
            }
            _ => {}
        }

        if kind == VisualKind::Line {
            self.cursor.col = 0;
        } else {
            self.clamp_col();
        }

        Ok(false)
    }

    // Called when we're in OperatorPending(op) and the next key arrives.
    // `op` is the operator char ('d', 'c', …); the key supplies the motion.
    fn handle_operator_pending(&mut self, op: char, key: KeyEvent) -> Result<bool> {
        // Most paths return to Normal. We set it here and any Insert-triggering
        // paths (change ops) will overwrite it.
        self.mode = Mode::Normal;

        let op = match op {
            'd' => Operator::Delete,
            'c' => Operator::Change,
            'y' => Operator::Yank,
            _ => return Ok(false),
        };

        match key.code {
            KeyCode::Esc => {} // cancel operator

            // Doubled operator = operate on whole line  (dd, cc)
            KeyCode::Char('d') if op == Operator::Delete => {
                let range = self.range_current_line();
                self.apply_operator(op, range);
            }
            KeyCode::Char('c') if op == Operator::Change => {
                let range = self.range_current_line();
                self.apply_operator(op, range);
            }
            KeyCode::Char('y') if op == Operator::Yank => {
                let range = self.range_current_line();
                self.apply_operator(op, range);
            }

            // Motions
            KeyCode::Char('w') => {
                let range = self.range_word();
                self.apply_operator(op, range);
            }
            KeyCode::Char('$') => {
                let range = self.range_to_line_end();
                self.apply_operator(op, range);
            }
            KeyCode::Char('0') => {
                if op == Operator::Delete || op == Operator::Yank {
                    let range = self.range_to_line_start();
                    self.apply_operator(op, range);
                }
            }
            KeyCode::Char('^') => {
                if let Some(range) = self.range_to_first_nonblank() {
                    self.apply_operator(op, range);
                }
            }

            _ => {} // unknown motion — silently cancel
        }

        Ok(false)
    }
}

// Returns an inclusive-exclusive (start_col, end_col) segment to highlight for a single
// document row, or None if the selection does not cover this row.
fn selection_segment_for_row(editor: &Editor, selection: Range, doc_row: usize) -> Option<(u16, u16)> {
    let selection = selection.normalized();

    match selection.kind {
        RangeKind::Line => {
            let start_row = selection.start.row as usize;
            let end_row = selection.end.row as usize; // exclusive
            if doc_row >= start_row && doc_row < end_row {
                // Highlight the full visible width.
                Some((0, u16::MAX))
            } else {
                None
            }
        }
        RangeKind::Char => {
            let start_row = selection.start.row as usize;
            let end_row = selection.end.row as usize;

            if doc_row < start_row || doc_row > end_row {
                return None;
            }

            if start_row == end_row {
                return Some((selection.start.col, selection.end.col));
            }

            if doc_row == start_row {
                // From start col to end of line.
                let len = editor.line_len(doc_row);
                return Some((selection.start.col, len));
            }

            if doc_row == end_row {
                // From start of line to end col.
                return Some((0, selection.end.col));
            }

            // Full middle lines.
            Some((0, editor.line_len(doc_row)))
        }
    }
}

fn apply_style(out: &mut std::io::Stdout, style: SpanStyle) -> Result<()> {
    execute!(out, SetAttribute(Attribute::Reset), ResetColor)?;

    if style.bold {
        execute!(out, SetAttribute(Attribute::Bold))?;
    }
    if style.dim {
        execute!(out, SetAttribute(Attribute::Dim))?;
    }
    if style.reverse {
        execute!(out, SetAttribute(Attribute::Reverse))?;
    }
    if let Some(fg) = style.fg {
        execute!(out, SetForegroundColor(fg))?;
    }
    if let Some(bg) = style.bg {
        execute!(out, SetBackgroundColor(bg))?;
    }

    Ok(())
}

fn render_spans(out: &mut std::io::Stdout, spans: &[Span], cols: usize) -> Result<()> {
    let mut used: usize = 0;
    let mut current = SpanStyle::default();
    apply_style(out, current)?;

    for span in spans {
        if used >= cols {
            break;
        }

        if span.style != current {
            current = span.style;
            apply_style(out, current)?;
        }

        let remaining = cols - used;
        let part: String = span.text.chars().take(remaining).collect();
        used += part.chars().count();
        execute!(out, Print(part))?;
    }

    apply_style(out, SpanStyle::default())?;
    Ok(())
}

fn truncate_spans(spans: &[Span], cols: usize) -> Vec<Span> {
    let mut out: Vec<Span> = Vec::new();
    let mut used: usize = 0;

    for span in spans {
        if used >= cols {
            break;
        }
        let remaining = cols - used;
        let text: String = span.text.chars().take(remaining).collect();
        used += text.chars().count();
        if !text.is_empty() {
            out.push(Span::new(text, span.style));
        }
    }

    out
}

fn apply_reverse_overlay(spans: &[Span], start: u16, end: u16) -> Vec<Span> {
    let start = start as usize;
    let end = end as usize;

    let mut out: Vec<Span> = Vec::new();
    let mut col: usize = 0;

    for span in spans {
        if span.text.is_empty() {
            continue;
        }

        let chars: Vec<char> = span.text.chars().collect();
        let span_start = col;
        let span_end = col + chars.len();
        col = span_end;

        if end <= span_start || start >= span_end {
            out.push(span.clone());
            continue;
        }

        let sel_start = start.saturating_sub(span_start).min(chars.len());
        let sel_end = end.saturating_sub(span_start).min(chars.len());

        if sel_start > 0 {
            out.push(Span::new(
                chars[..sel_start].iter().collect::<String>(),
                span.style,
            ));
        }

        if sel_start < sel_end {
            let mut style = span.style;
            style.reverse = true;
            out.push(Span::new(
                chars[sel_start..sel_end].iter().collect::<String>(),
                style,
            ));
        }

        if sel_end < chars.len() {
            out.push(Span::new(
                chars[sel_end..].iter().collect::<String>(),
                span.style,
            ));
        }
    }

    out
}

fn python_spans(line: &str) -> Vec<Span> {
    // Phase 9e (simple version): highlight a few token classes without a real parser.
    // - keywords: cyan + bold
    // - strings: green
    // - comments: dark grey + dim
    const KEYWORDS: &[&str] = &[
        "False", "None", "True", "and", "as", "assert", "async", "await", "break", "class",
        "continue", "def", "del", "elif", "else", "except", "finally", "for", "from", "global",
        "if", "import", "in", "is", "lambda", "nonlocal", "not", "or", "pass", "raise", "return",
        "try", "while", "with", "yield",
    ];

    let kw_style = SpanStyle {
        fg: Some(Color::Cyan),
        bold: true,
        ..SpanStyle::default()
    };
    let str_style = SpanStyle {
        fg: Some(Color::Green),
        ..SpanStyle::default()
    };
    let comment_style = SpanStyle {
        fg: Some(Color::DarkGrey),
        dim: true,
        ..SpanStyle::default()
    };

    let mut spans: Vec<Span> = Vec::new();
    let mut buf = String::new();

    enum State {
        Normal,
        String { quote: char, escaped: bool },
    }
    let mut state = State::Normal;

    let mut it = line.chars().peekable();
    while let Some(ch) = it.next() {
        match state {
            State::Normal => {
                if ch == '#' {
                    if !buf.is_empty() {
                        spans.push(Span::new(std::mem::take(&mut buf), SpanStyle::default()));
                    }
                    let mut rest = String::new();
                    rest.push(ch);
                    rest.extend(it);
                    spans.push(Span::new(rest, comment_style));
                    break;
                }

                if ch == '\'' || ch == '"' {
                    if !buf.is_empty() {
                        spans.push(Span::new(std::mem::take(&mut buf), SpanStyle::default()));
                    }
                    spans.push(Span::new(ch.to_string(), str_style));
                    state = State::String {
                        quote: ch,
                        escaped: false,
                    };
                    continue;
                }

                if is_ident_start(ch) {
                    if !buf.is_empty() {
                        spans.push(Span::new(std::mem::take(&mut buf), SpanStyle::default()));
                    }
                    let mut ident = String::new();
                    ident.push(ch);
                    while let Some(&next) = it.peek() {
                        if is_ident_continue(next) {
                            ident.push(next);
                            it.next();
                        } else {
                            break;
                        }
                    }
                    if KEYWORDS.contains(&ident.as_str()) {
                        spans.push(Span::new(ident, kw_style));
                    } else {
                        spans.push(Span::new(ident, SpanStyle::default()));
                    }
                } else {
                    buf.push(ch);
                }
            }
            State::String { quote, escaped } => {
                let last_is_string = spans.last().is_some_and(|s| s.style == str_style);
                if !last_is_string {
                    spans.push(Span::new(String::new(), str_style));
                }
                spans.last_mut().unwrap().text.push(ch);

                if escaped {
                    state = State::String {
                        quote,
                        escaped: false,
                    };
                } else if ch == '\\' {
                    state = State::String {
                        quote,
                        escaped: true,
                    };
                } else if ch == quote {
                    state = State::Normal;
                }
            }
        }
    }

    if !buf.is_empty() {
        spans.push(Span::new(buf, SpanStyle::default()));
    }

    spans
}

fn is_ident_start(ch: char) -> bool {
    ch == '_' || ch.is_ascii_alphabetic()
}

fn is_ident_continue(ch: char) -> bool {
    ch == '_' || ch.is_ascii_alphanumeric()
}

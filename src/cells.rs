/// A range of document line indices for one `# %%` cell.
/// `start` is inclusive (the `# %%` marker line, or 0 for the implicit first cell).
/// `end` is exclusive (the next `# %%` marker, or `lines.len()` for the last cell).
///
/// Using an exclusive end is idiomatic Rust — it matches how `..` ranges work,
/// and `lines[range.start..range.end]` is always a valid slice.
pub struct CellRange {
    pub start: usize,
    pub end: usize,
}

impl CellRange {
    /// Borrow the actual lines belonging to this cell.
    pub fn as_lines<'a>(&self, all_lines: &'a [String]) -> &'a [String] {
        let end = self.end.min(all_lines.len());
        &all_lines[self.start..end]
    }

    pub fn is_empty(&self) -> bool {
        self.start >= self.end
    }
}

/// A line is a cell separator if it starts with `# %%`.
/// Trailing text after `# %%` is allowed — Jupyter uses it for cell names.
pub fn is_cell_marker(line: &str) -> bool {
    line.starts_with("# %%")
}

/// Find the CellRange containing `cursor_row`.
///
/// Scans upward (inclusive of cursor_row) for the most recent `# %%`.
/// If none is found the cell starts at line 0 (the implicit first cell).
/// Scans downward (strictly after cursor_row) for the next `# %%`.
/// If none is found the cell ends at `lines.len()`.
pub fn find_current_cell(lines: &[String], cursor_row: usize) -> CellRange {
    if lines.is_empty() {
        return CellRange { start: 0, end: 0 };
    }

    let clamped = cursor_row.min(lines.len() - 1);

    // Upward scan: find the nearest marker at or above cursor.
    let start = (0..=clamped)
        .rev()
        .find(|&r| is_cell_marker(&lines[r]))
        .unwrap_or(0);

    // Downward scan: find the nearest marker strictly below cursor.
    let end = ((clamped + 1)..lines.len())
        .find(|&r| is_cell_marker(&lines[r]))
        .unwrap_or(lines.len());

    CellRange { start, end }
}

/// Return the line index of the next `# %%` marker strictly after `from`, or None.
pub fn next_cell_marker(lines: &[String], from: usize) -> Option<usize> {
    ((from + 1)..lines.len()).find(|&r| is_cell_marker(&lines[r]))
}

/// Return the line index of the previous `# %%` marker strictly before `from`, or None.
/// If `from` is itself a marker, this returns the one before it.
pub fn prev_cell_marker(lines: &[String], from: usize) -> Option<usize> {
    (0..from).rev().find(|&r| is_cell_marker(&lines[r]))
}

/// Count how many cells exist in the document (= number of `# %%` markers + 1 if there
/// is content before the first marker).
pub fn count_cells(lines: &[String]) -> usize {
    let markers = lines.iter().filter(|l| is_cell_marker(l)).count();
    if markers == 0 {
        // If there are no markers at all there's one implicit cell (or no cells).
        if lines.iter().any(|l| !l.trim().is_empty()) { 1 } else { 0 }
    } else {
        // Each marker starts a new cell. Lines before the first marker form cell 1
        // only if they exist (some files start with # %% on line 0).
        let first_marker = lines.iter().position(|l| is_cell_marker(l)).unwrap_or(0);
        if first_marker == 0 { markers } else { markers + 1 }
    }
}

/// Return the 1-based cell number containing `cursor_row`.
pub fn current_cell_number(lines: &[String], cursor_row: usize) -> usize {
    let range = find_current_cell(lines, cursor_row);
    // Count markers before the start of this cell.
    let markers_before = lines[..range.start]
        .iter()
        .filter(|l| is_cell_marker(l))
        .count();
    // If nothing before start is a marker, this is cell 1.
    // Otherwise each marker increments the cell count, and we may add 1 for any
    // implicit first cell.
    let first_marker_pos = lines.iter().position(|l| is_cell_marker(l)).unwrap_or(0);
    if first_marker_pos == 0 {
        markers_before + 1
    } else {
        markers_before + 2 // +1 for implicit first cell, +1 for 1-based
    }
    .max(1)
}

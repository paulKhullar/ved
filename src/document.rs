use anyhow::{anyhow, Result};
use serde_json::Value;

pub struct Document {
    pub lines: Vec<String>,
}

impl Document {
    pub fn empty() -> Self {
        Self { lines: Vec::new() }
    }

    pub fn open(path: &str) -> Result<Self> {
        let bytes = std::fs::read(path)?;
        let contents = String::from_utf8_lossy(&bytes);
        let lines = contents.lines().map(|l| l.to_string()).collect();
        Ok(Self { lines })
    }

    pub fn open_ipynb_as_cells(path: &str) -> Result<Self> {
        let bytes = std::fs::read(path)?;
        let contents = String::from_utf8_lossy(&bytes);
        let notebook: Value = serde_json::from_str(&contents)
            .map_err(|e| anyhow!("failed to parse ipynb JSON: {e}"))?;

        let Some(cells) = notebook.get("cells").and_then(|v| v.as_array()) else {
            return Err(anyhow!("ipynb missing top-level 'cells' array"));
        };

        let mut lines: Vec<String> = Vec::new();
        lines.push("# NOTE: opened .ipynb as a text cell view (outputs/metadata hidden)".into());
        lines.push("# Use :w <file.py> to export; saving back to .ipynb is not supported yet.".into());
        lines.push(String::new());

        for (idx, cell) in cells.iter().enumerate() {
            let cell_type = cell
                .get("cell_type")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");

            lines.push(format!("# %% [{cell_type}] ({})", idx + 1));

            let source_text = match cell.get("source") {
                Some(Value::Array(arr)) => arr
                    .iter()
                    .filter_map(|v| v.as_str())
                    .collect::<Vec<&str>>()
                    .join(""),
                Some(Value::String(s)) => s.clone(),
                _ => String::new(),
            };

            match cell_type {
                "markdown" => {
                    for l in source_text.lines() {
                        lines.push(format!("# {l}"));
                    }
                }
                _ => {
                    for l in source_text.lines() {
                        lines.push(l.to_string());
                    }
                }
            }

            lines.push(String::new());
        }

        Ok(Self { lines })
    }

    pub fn save(&self, path: &str) -> Result<()> {
        // join("\n") + "\n" gives every line its own newline, including the last —
        // that trailing newline is the POSIX convention and what most tools expect.
        let content = self.lines.join("\n") + "\n";
        std::fs::write(path, content)?;
        Ok(())
    }
}

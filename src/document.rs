use anyhow::Result;

pub struct Document {
    pub lines: Vec<String>,
}

impl Document {
    pub fn empty() -> Self {
        Self { lines: Vec::new() }
    }

    pub fn open(path: &str) -> Result<Self> {
        let contents = std::fs::read_to_string(path)?;
        let lines = contents.lines().map(|l| l.to_string()).collect();
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

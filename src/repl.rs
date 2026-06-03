// Phase 8b — long-lived REPL subprocess
//
// New Rust concepts here:
//   • std::process::Command + Child  — spawn and own a subprocess
//   • ChildStdin / ChildStdout       — owned I/O handles to the child
//   • child.stdout.take()            — moves a field out of a struct you don't own
//   • std::sync::mpsc                — multi-producer, single-consumer channel
//   • thread::spawn + move closure   — transfer ownership into a new OS thread
//   • try_recv()                     — non-blocking channel read

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};

use anyhow::{anyhow, Result};

pub struct Repl {
    // Keeping _child alive is the entire point of this field.
    // When a Child is dropped, the OS kills the process — so we must hold it
    // for as long as we want the REPL to exist.
    // The underscore prefix signals to Rust (and readers) that we hold it
    // intentionally for its side-effect, not to read from it.
    _child: Child,

    // We write cell text here. This is the "upstream" end of the pipe.
    stdin: ChildStdin,

    // We read output here. Filled by the reader threads (see start() below).
    rx: Receiver<String>,
}

impl Repl {
    pub fn start() -> Result<Self> {
        let cmd = find_python()
            .ok_or_else(|| anyhow!(
                "no python found on PATH — activate your venv before launching ved"
            ))?;

        // Pipe all three streams so nothing bleeds into our alternate-screen terminal.
        // -i  keeps Python in interactive (REPL) mode even with piped stdin.
        // -u  disables output buffering — without this, print() output gets stuck
        //     and your cells appear to produce no output.
        // -q  suppresses the version banner on startup.
        let mut child = Command::new(cmd)
            .args(["-i", "-u", "-q"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| anyhow!("failed to spawn {cmd}: {e}"))?;

        // .take() is a method on Option<T> that replaces the field with None
        // and returns the value. After this, child.stdin is None — we own the handle.
        // This is necessary because ChildStdin doesn't implement Copy; there can
        // only be one owner.
        let stdin = child.stdin.take().expect("stdin was piped");
        let stdout = child.stdout.take().expect("stdout was piped");
        let stderr = child.stderr.take().expect("stderr was piped");

        // Build the channel. tx (Sender) goes into the threads; rx (Receiver) stays here.
        // Sender is Clone, so both threads can send to the same Receiver.
        let (tx, rx) = mpsc::channel::<String>();

        // Stdout reader thread.
        // `move` transfers ownership of `stdout` and `tx_out` into the closure.
        // After this line the main thread can never touch `stdout` again — the borrow
        // checker enforces it. This is the core of the thread safety guarantee.
        let tx_out = tx.clone();
        std::thread::spawn(move || {
            let reader = BufReader::new(stdout);
            for line in reader.lines() {
                match line {
                    Ok(l) => {
                        // If the receiver has been dropped (editor closed), ignore errors.
                        let _ = tx_out.send(l);
                    }
                    Err(_) => break, // pipe closed — process died
                }
            }
        });

        // Stderr reader thread — same channel so errors appear in the output panel.
        // Python tracebacks go to stderr; we must read it or the OS buffer fills
        // and the child process blocks.
        std::thread::spawn(move || {
            let reader = BufReader::new(stderr);
            for line in reader.lines() {
                match line {
                    Ok(l) => { let _ = tx.send(l); }
                    Err(_) => break,
                }
            }
        });

        // Silence the interactive prompts (>>> and ...) so they don't litter the
        // output panel. Python sends these to stderr, but setting ps1/ps2 to empty
        // strings stops them from appearing after our startup command runs.
        // We send this via stdin so it executes inside our Python session.
        let mut repl = Self { _child: child, stdin, rx };
        let _ = repl.send_lines(&[
            "import sys; sys.ps1 = ''; sys.ps2 = ''".to_string()
        ]);

        Ok(repl)
    }

    // Write lines to the REPL's stdin. Python executes them as if you typed them.
    // The trailing newline after the last line is important — without it, Python
    // waits for more input and never executes the final statement.
    pub fn send_lines(&mut self, lines: &[String]) -> Result<()> {
        for line in lines {
            writeln!(self.stdin, "{line}")?;
        }
        // A blank line after a block (e.g. a function def) tells Python the block ended.
        writeln!(self.stdin)?;
        self.stdin.flush()?;
        Ok(())
    }

    // Drain any lines that have arrived from the REPL since the last call.
    // try_recv() is non-blocking: it returns immediately with Err(Empty) if
    // nothing is waiting, so this never stalls the UI thread.
    pub fn poll_output(&self) -> Vec<String> {
        let mut lines = Vec::new();
        while let Ok(line) = self.rx.try_recv() {
            lines.push(line);
        }
        lines
    }
}

// Try python3 first, then python.
// Whatever is on the inherited PATH is used — if the user activated a venv
// before launching ved, that venv's interpreter is what they get.
// Do NOT walk directories or read pyproject.toml: that's out of scope.
fn find_python() -> Option<&'static str> {
    for cmd in ["python3", "python"] {
        if Command::new(cmd).arg("--version").output().is_ok() {
            return Some(cmd);
        }
    }
    None
}

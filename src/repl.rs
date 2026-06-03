// Phase 8b/8c — long-lived REPL subprocess with sentinel-based completion detection.
//
// Rust concepts in play:
//   • std::process::Child / ChildStdin  — own a subprocess and its I/O handles
//   • child.stdout.take()              — moves a field out of a struct you don't own
//   • mpsc channel                     — thread-safe message passing
//   • thread::spawn + move closure     — ownership transfer into an OS thread
//   • enum over channel                — typed messages, not raw strings
//   • Instant / Duration               — measuring elapsed time for timeout

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};

// The sentinel Python prints to stdout when a cell finishes executing.
// Chosen to be unlikely to appear in real output. The reader thread watches
// for this line and sends Done instead of Line — it never reaches output_buf.
const SENTINEL: &str = "<<<VED_CELL_DONE>>>";

// How long to wait before declaring a cell hung.
const TIMEOUT: Duration = Duration::from_secs(30);

// Typed messages over the channel.
// Using an enum rather than String is the key Phase 8c insight: it lets the
// sentinel be handled structurally (a variant) rather than by string matching
// scattered across the codebase.
enum ReplEvent {
    Line(String), // a line of output to show in the panel
    Done,         // sentinel received — cell has finished
}

pub struct Repl {
    // Held alive for its side-effect: dropping Child kills the process.
    _child: Child,
    stdin: ChildStdin,
    rx: Receiver<ReplEvent>,
    // Tracks whether we're waiting for a sentinel from the last send.
    running: bool,
    cell_start: Option<Instant>,
}

impl Repl {
    pub fn start() -> Result<Self> {
        let cmd = find_python().ok_or_else(|| {
            anyhow!("no python found on PATH — activate your venv before launching ved")
        })?;

        let mut child = Command::new(cmd)
            .args(["-i", "-u", "-q"]) // interactive, unbuffered, quiet (no banner)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| anyhow!("failed to spawn {cmd}: {e}"))?;

        let stdin = child.stdin.take().expect("stdin was piped");
        let stdout = child.stdout.take().expect("stdout was piped");
        let stderr = child.stderr.take().expect("stderr was piped");

        let (tx, rx) = mpsc::channel::<ReplEvent>();

        // Stdout reader: watches for the sentinel and converts it to Done.
        // Everything else becomes a Line. The sentinel itself is swallowed here
        // and never reaches the output panel.
        let tx_out = tx.clone();
        std::thread::spawn(move || {
            let reader = BufReader::new(stdout);
            for line in reader.lines() {
                match line {
                    Ok(l) if l.trim() == SENTINEL => {
                        let _ = tx_out.send(ReplEvent::Done);
                    }
                    Ok(l) => {
                        let _ = tx_out.send(ReplEvent::Line(l));
                    }
                    Err(_) => break,
                }
            }
        });

        // Stderr reader: tracebacks, errors, and prompts all land here.
        // We must drain stderr or the OS buffer fills and the child blocks.
        std::thread::spawn(move || {
            let reader = BufReader::new(stderr);
            for line in reader.lines() {
                match line {
                    Ok(l) => {
                        let _ = tx.send(ReplEvent::Line(l));
                    }
                    Err(_) => break,
                }
            }
        });

        let mut repl = Self {
            _child: child,
            stdin,
            rx,
            running: false,
            cell_start: None,
        };

        // Silence the >>> and ... prompts so they don't clutter the output panel.
        let _ = repl.send_raw("import sys; sys.ps1 = ''; sys.ps2 = ''");

        Ok(repl)
    }

    // Internal: send a line without arming the sentinel or the timer.
    // Used for initialization commands only.
    fn send_raw(&mut self, code: &str) -> Result<()> {
        writeln!(self.stdin, "{code}")?;
        self.stdin.flush()?;
        Ok(())
    }

    // Send cell lines to Python, then write the sentinel.
    // Python will print the sentinel when it reaches that line, signalling Done.
    //
    // The blank line between the cell and the sentinel closes any open block
    // (a `def`, `for`, `if`, etc.) — Python's interactive mode needs a blank
    // line to know a block is complete.
    pub fn send_cell(&mut self, lines: &[String]) -> Result<()> {
        for line in lines {
            writeln!(self.stdin, "{line}")?;
        }
        writeln!(self.stdin)?; // close any open block
        writeln!(self.stdin, "print('{SENTINEL}')")?;
        self.stdin.flush()?;
        self.running = true;
        self.cell_start = Some(Instant::now());
        Ok(())
    }

    // Non-blocking drain. Returns (new output lines, whether Done was received).
    pub fn poll_output(&self) -> (Vec<String>, bool) {
        let mut lines = Vec::new();
        let mut done = false;
        while let Ok(event) = self.rx.try_recv() {
            match event {
                ReplEvent::Line(l) => lines.push(l),
                ReplEvent::Done => done = true,
            }
        }
        (lines, done)
    }

    pub fn is_running(&self) -> bool {
        self.running
    }

    pub fn is_timed_out(&self) -> bool {
        self.running
            && self
                .cell_start
                .map(|t| t.elapsed() > TIMEOUT)
                .unwrap_or(false)
    }

    pub fn elapsed_secs(&self) -> u64 {
        self.cell_start
            .map(|t| t.elapsed().as_secs())
            .unwrap_or(0)
    }

    // Called when we receive Done or decide the cell is over (timeout/interrupt).
    pub fn mark_idle(&mut self) {
        self.running = false;
        self.cell_start = None;
    }

    // Send SIGINT to interrupt a running cell without killing the REPL.
    // Uses the system `kill` command to avoid adding a dependency.
    // The process stays alive; only the current computation is interrupted.
    pub fn interrupt(&mut self) {
        let pid = self._child.id();
        let _ = Command::new("kill")
            .args(["-INT", &pid.to_string()])
            .output();
        self.mark_idle();
    }
}

fn find_python() -> Option<&'static str> {
    for cmd in ["python3", "python"] {
        if Command::new(cmd).arg("--version").output().is_ok() {
            return Some(cmd);
        }
    }
    None
}

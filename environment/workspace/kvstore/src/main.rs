// Stub: replace with the storage engine described in the task instructions.
// It reads commands from stdin and answers every one with an error.

use std::io::{self, BufRead, Write};

fn main() {
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut out = stdout.lock();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        let _ = line;
        let _ = writeln!(out, "ERR not implemented");
        let _ = out.flush();
    }
}

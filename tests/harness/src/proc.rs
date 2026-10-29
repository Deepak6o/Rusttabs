//! Running one engine process: pipes, reply reader, watchdog and RSS sampling.

use std::cell::Cell;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

pub struct Ctx {
    pub bin: PathBuf,
    pub work: PathBuf,
    pub uid: u32,
    pub gid: u32,
    counter: Cell<u64>,
}

impl Ctx {
    pub fn new(bin: PathBuf, work: PathBuf, uid: u32, gid: u32) -> Ctx {
        Ctx { bin, work, uid, gid, counter: Cell::new(0) }
    }

    fn next(&self) -> u64 {
        let n = self.counter.get() + 1;
        self.counter.set(n);
        n
    }

    /// A path for a new database directory (not created; the engine creates it).
    pub fn fresh_dir(&self, tag: &str) -> PathBuf {
        self.work.join("db").join(format!("{tag}-{}", self.next()))
    }

    /// Copies a database directory with `cp -a` (owner and contents preserved).
    pub fn copy_dir(&self, from: &Path, to: &Path) -> Result<(), String> {
        let st = Command::new("cp")
            .arg("-a")
            .arg(from)
            .arg(to)
            .status()
            .map_err(|e| format!("cp: {e}"))?;
        if st.success() {
            Ok(())
        } else {
            Err("cp -a failed".into())
        }
    }
}

#[derive(Clone)]
pub struct Opts {
    pub sync: bool,
    pub mem: Option<usize>,
    pub deadline: Duration,
}

impl Default for Opts {
    fn default() -> Opts {
        Opts { sync: false, mem: None, deadline: Duration::from_secs(600) }
    }
}

pub struct Proc {
    child: Arc<Mutex<Child>>,
    pid: u32,
    stdin: Option<BufWriter<ChildStdin>>,
    rx: mpsc::Receiver<Vec<u8>>,
    stderr_path: PathBuf,
    done: Arc<AtomicBool>,
    peak_kb: Arc<AtomicU64>,
    pub broken: bool,
}

fn status_kb(pid: u32, field: &str) -> Option<u64> {
    let s = fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    let line = s.lines().find(|l| l.starts_with(field))?;
    line[field.len()..].trim().trim_end_matches("kB").trim().parse().ok()
}

impl Proc {
    pub fn spawn(ctx: &Ctx, dir: &Path, o: &Opts) -> Result<Proc, String> {
        let stderr_path = ctx.work.join("logs").join(format!("engine-{}.stderr", ctx.next()));
        let errf = File::create(&stderr_path).map_err(|e| e.to_string())?;
        let mut cmd = Command::new(&ctx.bin);
        cmd.arg("--dir").arg(dir);
        if o.sync {
            cmd.arg("--sync");
        }
        if let Some(m) = o.mem {
            cmd.arg("--memtable-bytes").arg(m.to_string());
        }
        cmd.env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", ctx.work.join("home"))
            .current_dir(ctx.work.join("home"))
            .uid(ctx.uid)
            .gid(ctx.gid)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::from(errf));
        let mut child = cmd.spawn().map_err(|e| format!("cannot start engine: {e}"))?;
        let pid = child.id();
        let stdout = child.stdout.take().unwrap();
        let stdin = child.stdin.take().unwrap();
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let mut r = BufReader::with_capacity(1 << 20, stdout);
            loop {
                let mut l = Vec::new();
                match r.read_until(b'\n', &mut l) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        if l.last() == Some(&b'\n') {
                            l.pop();
                        }
                        if tx.send(l).is_err() {
                            break;
                        }
                    }
                }
            }
        });
        let child = Arc::new(Mutex::new(child));
        let done = Arc::new(AtomicBool::new(false));
        let peak_kb = Arc::new(AtomicU64::new(0));
        {
            // Samples memory use and enforces the per-process deadline.
            let (done, peak, child) = (done.clone(), peak_kb.clone(), child.clone());
            let deadline = o.deadline;
            thread::spawn(move || {
                let start = Instant::now();
                while !done.load(Ordering::SeqCst) {
                    for f in ["VmRSS:", "VmHWM:"] {
                        if let Some(kb) = status_kb(pid, f) {
                            peak.fetch_max(kb, Ordering::SeqCst);
                        }
                    }
                    if start.elapsed() > deadline {
                        if let Ok(mut c) = child.lock() {
                            let _ = c.kill();
                        }
                        break;
                    }
                    thread::sleep(Duration::from_millis(20));
                }
            });
        }
        Ok(Proc {
            child,
            pid,
            stdin: Some(BufWriter::with_capacity(1 << 20, stdin)),
            rx,
            stderr_path,
            done,
            peak_kb,
            broken: false,
        })
    }

    /// Queues bytes for the engine. Returns false once the pipe is broken.
    pub fn send(&mut self, data: &[u8]) -> bool {
        if self.broken {
            return false;
        }
        let ok = self.stdin.as_mut().is_some_and(|w| w.write_all(data).is_ok());
        match ok {
            true => true,
            false => {
                self.broken = true;
                false
            }
        }
    }

    pub fn flush(&mut self) -> bool {
        if self.broken {
            return false;
        }
        let ok = self.stdin.as_mut().is_some_and(|w| w.flush().is_ok());
        match ok {
            true => true,
            false => {
                self.broken = true;
                false
            }
        }
    }

    pub fn recv(&self, t: Duration) -> Result<Vec<u8>, String> {
        match self.rx.recv_timeout(t) {
            Ok(l) => Ok(l),
            Err(mpsc::RecvTimeoutError::Timeout) => Err(format!("no reply within {}s", t.as_secs())),
            Err(mpsc::RecvTimeoutError::Disconnected) => Err(format!(
                "engine closed its output (exited or crashed){}",
                self.stderr_hint()
            )),
        }
    }

    /// Collects the replies that were already written when the engine was killed.
    pub fn drain(&self) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        while let Ok(l) = self.rx.recv_timeout(Duration::from_secs(10)) {
            out.push(l);
        }
        out
    }

    pub fn close_stdin(&mut self) {
        if let Some(mut w) = self.stdin.take() {
            let _ = w.flush();
        }
    }

    pub fn wait(&mut self, t: Duration) -> Result<ExitStatus, String> {
        let start = Instant::now();
        loop {
            let r = self.child.lock().unwrap().try_wait();
            match r {
                Ok(Some(st)) => {
                    self.done.store(true, Ordering::SeqCst);
                    return Ok(st);
                }
                Ok(None) => {}
                Err(e) => return Err(e.to_string()),
            }
            if start.elapsed() > t {
                self.kill();
                return Err(format!("engine did not exit within {}s", t.as_secs()));
            }
            thread::sleep(Duration::from_millis(5));
        }
    }

    /// Clean shutdown: close stdin and require exit status 0.
    pub fn shutdown(&mut self, t: Duration) -> Result<(), String> {
        self.close_stdin();
        let st = self.wait(t)?;
        if !st.success() {
            return Err(format!("engine exited with {st} after end of input{}", self.stderr_hint()));
        }
        Ok(())
    }

    /// SIGKILL, then reap.
    pub fn kill(&mut self) {
        self.done.store(true, Ordering::SeqCst);
        let mut c = self.child.lock().unwrap();
        let _ = c.kill();
        let _ = c.wait();
    }

    pub fn peak_rss_kb(&self) -> u64 {
        for f in ["VmRSS:", "VmHWM:"] {
            if let Some(kb) = status_kb(self.pid, f) {
                self.peak_kb.fetch_max(kb, Ordering::SeqCst);
            }
        }
        self.peak_kb.load(Ordering::SeqCst)
    }

    fn stderr_hint(&self) -> String {
        let mut f = match File::open(&self.stderr_path) {
            Ok(f) => f,
            Err(_) => return String::new(),
        };
        let len = f.metadata().map(|m| m.len()).unwrap_or(0);
        let _ = f.seek(SeekFrom::Start(len.saturating_sub(300)));
        let mut s = String::new();
        let _ = f.read_to_string(&mut s);
        let s = s.trim();
        if s.is_empty() {
            String::new()
        } else {
            format!("; stderr: {}", s.replace('\n', " | "))
        }
    }
}

impl Drop for Proc {
    fn drop(&mut self) {
        self.kill();
    }
}

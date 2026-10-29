//! A small LSM-tree: memtable + write-ahead log, immutable sorted tables, and a
//! manifest that names the live tables and the oldest log still needed.
//!
//! Invariants that make crashes safe:
//! - A command reaches the log (one record) before it touches the memtable.
//! - A table is fully written, fsynced and renamed before the manifest mentions it.
//! - The manifest is replaced atomically (write temp, fsync, rename, fsync dir).
//! - A log is deleted only after a manifest with a higher `minwal` is durable.
//! - Files not named by the manifest are leftovers and are deleted on sight.

use crate::sst::{Lookup, Sst, SstIter, SstWriter};
use crate::util::{corrupt, crc32, sync_dir};
use crate::wal::{self, wal_path, Op, Wal};
use std::collections::btree_map;
use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Write};
use std::iter::Peekable;
use std::ops::Bound;
use std::path::{Path, PathBuf};

const MAX_TABLES: usize = 8;
const MANIFEST: &str = "MANIFEST";

struct Manifest {
    next: u64,
    min_wal: u64,
    tables: Vec<u64>,
}

fn read_manifest(dir: &Path) -> io::Result<Option<Manifest>> {
    let text = match fs::read(dir.join(MANIFEST)) {
        Ok(t) => t,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let crc_at = text
        .windows(5)
        .rposition(|w| w == b"\ncrc ")
        .ok_or_else(|| corrupt("manifest has no checksum"))?
        + 1;
    let body = &text[..crc_at];
    let crc_line = std::str::from_utf8(&text[crc_at..]).map_err(|_| corrupt("manifest"))?;
    let want = u32::from_str_radix(crc_line.trim().trim_start_matches("crc ").trim(), 16)
        .map_err(|_| corrupt("manifest checksum"))?;
    if crc32(body) != want {
        return Err(corrupt("manifest checksum mismatch"));
    }
    let body = std::str::from_utf8(body).map_err(|_| corrupt("manifest"))?;
    let mut m = Manifest { next: 0, min_wal: 0, tables: Vec::new() };
    let num = |s: &str| s.parse::<u64>().map_err(|_| corrupt("manifest number"));
    for line in body.lines() {
        let mut parts = line.split(' ');
        match parts.next() {
            Some("kvstore-manifest") => {}
            Some("next") => m.next = num(parts.next().unwrap_or(""))?,
            Some("minwal") => m.min_wal = num(parts.next().unwrap_or(""))?,
            Some("tables") => {
                for p in parts.filter(|p| !p.is_empty()) {
                    m.tables.push(num(p)?);
                }
            }
            _ => return Err(corrupt("manifest line")),
        }
    }
    Ok(Some(m))
}

fn write_manifest(dir: &Path, m: &Manifest) -> io::Result<()> {
    let mut text = format!("kvstore-manifest 1\nnext {}\nminwal {}\ntables", m.next, m.min_wal);
    for t in &m.tables {
        text.push_str(&format!(" {t}"));
    }
    text.push('\n');
    let crc = crc32(text.as_bytes());
    text.push_str(&format!("crc {crc:08x}\n"));
    let tmp = dir.join("MANIFEST.tmp");
    let mut f = fs::File::create(&tmp)?;
    f.write_all(text.as_bytes())?;
    f.sync_all()?;
    drop(f);
    fs::rename(&tmp, dir.join(MANIFEST))?;
    sync_dir(dir)
}

fn file_num(name: &str, ext: &str) -> Option<u64> {
    name.strip_suffix(ext)?.parse().ok()
}

fn apply_op(mem: &mut BTreeMap<Vec<u8>, Option<Vec<u8>>>, bytes: &mut usize, op: Op) {
    let (key, val) = match op {
        Op::Put(k, v) => (k, Some(v)),
        Op::Del(k) => (k, None),
    };
    let klen = key.len();
    *bytes += klen + val.as_ref().map_or(0, |v| v.len());
    if let Some(old) = mem.insert(key, val) {
        *bytes -= klen + old.map_or(0, |v| v.len());
    }
}

enum Src<'a> {
    Mem(Peekable<btree_map::Range<'a, Vec<u8>, Option<Vec<u8>>>>),
    Sst(SstIter<'a>),
}

impl<'a> Src<'a> {
    fn peek(&mut self) -> io::Result<Option<(&[u8], Option<&[u8]>)>> {
        match self {
            Src::Mem(it) => Ok(it.peek().map(|(k, v)| (k.as_slice(), v.as_deref()))),
            Src::Sst(it) => it.peek(),
        }
    }

    fn advance(&mut self) -> io::Result<()> {
        match self {
            Src::Mem(it) => {
                it.next();
                Ok(())
            }
            Src::Sst(it) => it.advance(),
        }
    }
}

/// Merges sources (newest first). For each key, the newest version wins.
/// `emit` gets tombstones too and returns false to stop.
fn merge(
    srcs: &mut [Src],
    end: Option<&[u8]>,
    mut emit: impl FnMut(&[u8], Option<&[u8]>) -> io::Result<bool>,
) -> io::Result<()> {
    let mut cur: Vec<u8> = Vec::new();
    loop {
        let mut found = false;
        for s in srcs.iter_mut() {
            if let Some((k, _)) = s.peek()? {
                if !found || k < cur.as_slice() {
                    cur.clear();
                    cur.extend_from_slice(k);
                    found = true;
                }
            }
        }
        if !found || end.is_some_and(|e| cur.as_slice() >= e) {
            return Ok(());
        }
        let mut emitted = false;
        let mut keep_going = true;
        for s in srcs.iter_mut() {
            let is_match = {
                let p = s.peek()?;
                let m = matches!(p, Some((k, _)) if k == cur.as_slice());
                if m && !emitted {
                    keep_going = emit(&cur, p.unwrap().1)?;
                    emitted = true;
                }
                m
            };
            if is_match {
                s.advance()?;
            }
        }
        if !keep_going {
            return Ok(());
        }
    }
}

pub struct Engine {
    dir: PathBuf,
    sync: bool,
    mem_limit: usize,
    mem: BTreeMap<Vec<u8>, Option<Vec<u8>>>,
    mem_bytes: usize,
    wal: Wal,
    tables: Vec<Sst>, // newest first
    next_num: u64,
}

impl Engine {
    pub fn open(dir: &Path, sync: bool, mem_limit: usize) -> io::Result<Engine> {
        fs::create_dir_all(dir)?;
        let dir = fs::canonicalize(dir)?;
        let man = match read_manifest(&dir)? {
            Some(m) => m,
            None => {
                let m = Manifest { next: 2, min_wal: 1, tables: Vec::new() };
                write_manifest(&dir, &m)?;
                m
            }
        };

        let mut wals = Vec::new();
        let mut max_num = 0;
        for entry in fs::read_dir(&dir)? {
            let name = entry?.file_name().to_string_lossy().into_owned();
            if let Some(n) = file_num(&name, ".log") {
                wals.push(n);
                max_num = max_num.max(n);
            } else if let Some(n) = file_num(&name, ".sst") {
                max_num = max_num.max(n);
            }
        }
        wals.sort_unstable();

        let mut tables = Vec::new();
        for &n in &man.tables {
            tables.push(Sst::open(&dir, n)?);
        }

        // Replay logs oldest first. The first torn record ends recovery: everything
        // after it is cut, which keeps the recovered state a prefix of the history.
        let mut mem = BTreeMap::new();
        let mut mem_bytes = 0usize;
        let mut stop = false;
        for &n in wals.iter().filter(|&&n| n >= man.min_wal) {
            let p = wal_path(&dir, n);
            if stop {
                fs::remove_file(&p)?;
                continue;
            }
            let (good, clean) = wal::replay(&p, |ops| {
                for op in ops {
                    apply_op(&mut mem, &mut mem_bytes, op);
                }
            })?;
            if !clean {
                wal::truncate(&p, good)?;
                stop = true;
            }
        }

        let mut next_num = man.next.max(max_num + 1);
        let wal_num = next_num;
        next_num += 1;
        let wal = Wal::create(&dir, wal_num)?;
        let mut eng = Engine { dir, sync, mem_limit, mem, mem_bytes, wal, tables, next_num };
        if !eng.mem.is_empty() {
            let t = eng.write_memtable()?;
            eng.tables.insert(0, t);
        }
        eng.save_manifest()?;
        eng.mem.clear();
        eng.mem_bytes = 0;
        eng.remove_obsolete()?;
        eng.maybe_compact()?;
        Ok(eng)
    }

    fn alloc(&mut self) -> u64 {
        let n = self.next_num;
        self.next_num += 1;
        n
    }

    fn save_manifest(&self) -> io::Result<()> {
        let m = Manifest {
            next: self.next_num,
            min_wal: self.wal.num,
            tables: self.tables.iter().map(|t| t.num).collect(),
        };
        write_manifest(&self.dir, &m)
    }

    fn remove_obsolete(&self) -> io::Result<()> {
        for entry in fs::read_dir(&self.dir)? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            let stale = if name.ends_with(".tmp") {
                true
            } else if let Some(n) = file_num(&name, ".sst") {
                !self.tables.iter().any(|t| t.num == n)
            } else if let Some(n) = file_num(&name, ".log") {
                n < self.wal.num
            } else {
                false
            };
            if stale {
                fs::remove_file(entry.path())?;
            }
        }
        Ok(())
    }

    fn write_memtable(&mut self) -> io::Result<Sst> {
        let num = self.alloc();
        let mut w = SstWriter::create(&self.dir, num)?;
        for (k, v) in &self.mem {
            w.add(k, v.as_deref())?;
        }
        w.finish()
    }

    /// Moves the memtable into a new table and starts a fresh log.
    fn flush(&mut self) -> io::Result<()> {
        if self.mem.is_empty() && self.wal.len == 0 {
            return Ok(());
        }
        let wal_num = self.alloc();
        let new_wal = Wal::create(&self.dir, wal_num)?;
        if !self.mem.is_empty() {
            let t = self.write_memtable()?;
            self.tables.insert(0, t);
        }
        self.wal = new_wal;
        self.save_manifest()?;
        self.mem.clear();
        self.mem_bytes = 0;
        self.remove_obsolete()
    }

    fn maybe_compact(&mut self) -> io::Result<()> {
        if self.tables.len() > MAX_TABLES {
            self.compact_tables()?;
        }
        Ok(())
    }

    /// Merges every table into one, dropping overwritten versions and tombstones.
    fn compact_tables(&mut self) -> io::Result<()> {
        if self.tables.is_empty() {
            return Ok(());
        }
        let num = self.alloc();
        let mut w = SstWriter::create(&self.dir, num)?;
        {
            let mut srcs = Vec::with_capacity(self.tables.len());
            for t in &self.tables {
                srcs.push(Src::Sst(t.iter_from(None)?));
            }
            merge(&mut srcs, None, |k, v| {
                if let Some(v) = v {
                    w.add(k, Some(v))?;
                }
                Ok(true)
            })?;
        }
        let t = w.finish()?;
        self.tables = if t.entries == 0 { Vec::new() } else { vec![t] };
        self.save_manifest()?;
        self.remove_obsolete()
    }

    pub fn apply(&mut self, ops: Vec<Op>) -> io::Result<()> {
        self.wal.append(&ops)?;
        if self.sync {
            self.wal.sync()?;
        }
        for op in ops {
            apply_op(&mut self.mem, &mut self.mem_bytes, op);
        }
        let wal_cap = 2 * self.mem_limit as u64 + (1 << 20);
        if self.mem_bytes >= self.mem_limit || self.wal.len >= wal_cap {
            self.flush()?;
            self.maybe_compact()?;
        }
        Ok(())
    }

    pub fn get(&self, key: &[u8]) -> io::Result<Option<Vec<u8>>> {
        if let Some(v) = self.mem.get(key) {
            return Ok(v.clone());
        }
        for t in &self.tables {
            match t.get(key)? {
                Lookup::Value(v) => return Ok(Some(v)),
                Lookup::Deleted => return Ok(None),
                Lookup::Absent => {}
            }
        }
        Ok(None)
    }

    pub fn scan(
        &self,
        start: Option<&[u8]>,
        end: Option<&[u8]>,
        limit: usize,
        mut emit: impl FnMut(&[u8], &[u8]) -> io::Result<()>,
    ) -> io::Result<()> {
        let lower = match start {
            Some(s) => Bound::Included(s),
            None => Bound::Unbounded,
        };
        let mut srcs = Vec::with_capacity(self.tables.len() + 1);
        srcs.push(Src::Mem(self.mem.range::<[u8], _>((lower, Bound::Unbounded)).peekable()));
        for t in &self.tables {
            srcs.push(Src::Sst(t.iter_from(start)?));
        }
        let mut n = 0;
        merge(&mut srcs, end, |k, v| {
            if let Some(v) = v {
                emit(k, v)?;
                n += 1;
            }
            Ok(n < limit)
        })
    }

    pub fn sync(&mut self) -> io::Result<()> {
        self.wal.sync()
    }

    pub fn compact(&mut self) -> io::Result<()> {
        self.flush()?;
        self.compact_tables()
    }
}

//! Randomness, hex, the reference model and protocol helpers shared by all checks.

use crate::proc::Proc;
use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::time::Duration;

pub type Key = Vec<u8>;
pub type Val = Vec<u8>;
pub type Model = BTreeMap<Key, Val>;
/// One atomic mutation: a single PUT/DEL or a whole BATCH.
pub type Unit = Vec<(Key, Option<Val>)>;

pub const T: Duration = Duration::from_secs(120);

pub fn mix(mut z: u64) -> u64 {
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Rng {
        Rng(mix(seed ^ 0x5DEE_CE66_D1CE_4E5B))
    }
    pub fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        mix(self.0)
    }
    pub fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }
    /// Uniform in lo..=hi.
    pub fn range(&mut self, lo: usize, hi: usize) -> usize {
        lo + self.below((hi - lo + 1) as u64) as usize
    }
    pub fn chance(&mut self, p: f64) -> bool {
        ((self.next() >> 11) as f64 / (1u64 << 53) as f64) < p
    }
    pub fn bytes(&mut self, n: usize) -> Vec<u8> {
        let mut v = Vec::with_capacity(n + 8);
        while v.len() < n {
            v.extend_from_slice(&self.next().to_le_bytes());
        }
        v.truncate(n);
        v
    }
    pub fn pick<'a, T>(&mut self, v: &'a [T]) -> &'a T {
        &v[self.below(v.len() as u64) as usize]
    }
    pub fn shuffle<X>(&mut self, v: &mut [X]) {
        for i in (1..v.len()).rev() {
            let j = self.below(i as u64 + 1) as usize;
            v.swap(i, j);
        }
    }
}

/// Deterministic pseudo-random bytes from a 64-bit seed (fast, for bulk values).
pub fn stream_bytes(seed: u64, n: usize, out: &mut Vec<u8>) {
    let mut s = seed;
    let start = out.len();
    while out.len() - start < n {
        s = s.wrapping_add(0x9E37_79B9_7F4A_7C15);
        out.extend_from_slice(&mix(s).to_le_bytes());
    }
    out.truncate(start + n);
}

const HEX_LO: &[u8; 16] = b"0123456789abcdef";
const HEX_UP: &[u8; 16] = b"0123456789ABCDEF";

pub fn hex_into(out: &mut Vec<u8>, data: &[u8], upper: bool) {
    let t = if upper { HEX_UP } else { HEX_LO };
    out.reserve(data.len() * 2);
    for &b in data {
        out.push(t[(b >> 4) as usize]);
        out.push(t[(b & 15) as usize]);
    }
}

pub fn val_into(out: &mut Vec<u8>, v: &[u8], upper: bool) {
    if v.is_empty() {
        out.push(b'-');
    } else {
        hex_into(out, v, upper);
    }
}

pub fn hex(data: &[u8]) -> String {
    let mut v = Vec::new();
    hex_into(&mut v, data, false);
    String::from_utf8(v).unwrap()
}

pub fn short(data: &[u8]) -> String {
    let h = hex(data);
    if h.len() > 40 {
        format!("{}..({} bytes)", &h[..40], data.len())
    } else {
        h
    }
}

/// Strict decoder for replies: lower-case hex only.
pub fn unhex_lower(s: &[u8]) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 {
        return None;
    }
    let n = |c: u8| match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        _ => None,
    };
    s.chunks_exact(2).map(|p| Some((n(p[0])? << 4) | n(p[1])?)).collect()
}

pub fn unval(s: &[u8]) -> Option<Vec<u8>> {
    if s == b"-" {
        Some(Vec::new())
    } else if s.is_empty() {
        None
    } else {
        unhex_lower(s)
    }
}

pub fn encode_unit(out: &mut Vec<u8>, u: &Unit, as_batch: bool, upper: bool) {
    let line = |out: &mut Vec<u8>, k: &Key, v: &Option<Val>| match v {
        Some(v) => {
            out.extend_from_slice(b"PUT ");
            hex_into(out, k, upper);
            out.push(b' ');
            val_into(out, v, upper);
            out.push(b'\n');
        }
        None => {
            out.extend_from_slice(b"DEL ");
            hex_into(out, k, upper);
            out.push(b'\n');
        }
    };
    if u.len() == 1 && !as_batch {
        line(out, &u[0].0, &u[0].1);
    } else {
        out.extend_from_slice(format!("BATCH {}\n", u.len()).as_bytes());
        for (k, v) in u {
            line(out, k, v);
        }
    }
}

pub fn apply(m: &mut Model, u: &Unit) {
    for (k, v) in u {
        match v {
            Some(v) => {
                m.insert(k.clone(), v.clone());
            }
            None => {
                m.remove(k);
            }
        }
    }
}

pub fn expect_ok(p: &Proc, what: &str) -> Result<(), String> {
    let r = p.recv(T)?;
    if r != b"OK" {
        return Err(format!("{what}: expected OK, got {:?}", String::from_utf8_lossy(&r[..r.len().min(80)])));
    }
    Ok(())
}

/// Reads ROW lines until END; checks strict ascending order.
pub fn read_rows(p: &Proc) -> Result<Vec<(Key, Val)>, String> {
    let mut rows: Vec<(Key, Val)> = Vec::new();
    loop {
        let l = p.recv(T)?;
        if l == b"END" {
            return Ok(rows);
        }
        let mut it = l.splitn(3, |&c| c == b' ');
        let (tag, k, v) = (it.next(), it.next(), it.next());
        let (Some(b"ROW"), Some(k), Some(v)) = (tag, k, v) else {
            return Err(format!("bad SCAN line {:?}", String::from_utf8_lossy(&l[..l.len().min(80)])));
        };
        let k = unhex_lower(k).ok_or("SCAN row key is not lower-case hex")?;
        let v = unval(v).ok_or("SCAN row value is not lower-case hex or -")?;
        if let Some((last, _)) = rows.last() {
            if *last >= k {
                return Err("SCAN rows are not in strictly ascending key order".into());
            }
        }
        rows.push((k, v));
    }
}

pub fn full_scan(p: &mut Proc) -> Result<Model, String> {
    p.send(b"SCAN - - 1000000\n");
    p.flush();
    Ok(read_rows(p)?.into_iter().collect())
}

pub fn diff(got: &Model, want: &Model) -> Option<String> {
    if got == want {
        return None;
    }
    for (k, v) in want {
        match got.get(k) {
            None => return Some(format!("key {} missing ({} keys expected, {} found)", short(k), want.len(), got.len())),
            Some(g) if g != v => return Some(format!("key {} has the wrong value", short(k))),
            _ => {}
        }
    }
    for k in got.keys() {
        if !want.contains_key(k) {
            return Some(format!("unexpected key {} ({} keys expected, {} found)", short(k), want.len(), got.len()));
        }
    }
    Some("contents differ".into())
}

/// Finds a prefix length k in floor..=units.len() such that base + units[..k] equals
/// `rec`. Returns k and that model.
pub fn prefix_match(rec: &Model, base: &Model, units: &[Unit], floor: usize) -> Result<(usize, Model), String> {
    let mut m = base.clone();
    for u in &units[..floor] {
        apply(&mut m, u);
    }
    let mut mismatched: usize = 0;
    for (k, v) in &m {
        if rec.get(k) != Some(v) {
            mismatched += 1;
        }
    }
    for k in rec.keys() {
        if !m.contains_key(k) {
            mismatched += 1;
        }
    }
    let first_diff = diff(rec, &m);
    for k in floor..=units.len() {
        if mismatched == 0 {
            return Ok((k, m));
        }
        if k == units.len() {
            break;
        }
        for (key, v) in &units[k] {
            let before = rec.get(key) != m.get(key);
            match v {
                Some(v) => {
                    m.insert(key.clone(), v.clone());
                }
                None => {
                    m.remove(key);
                }
            }
            let after = rec.get(key) != m.get(key);
            match (before, after) {
                (true, false) => mismatched -= 1,
                (false, true) => mismatched += 1,
                _ => {}
            }
        }
    }
    Err(format!(
        "recovered state is not the result of any allowed prefix (need at least {floor} of {} mutations); vs. the shortest allowed prefix: {}",
        units.len(),
        first_diff.unwrap_or_default()
    ))
}

/// Allocated bytes of every file under `dir`.
pub fn disk_usage(dir: &Path) -> u64 {
    let mut total = 0;
    let Ok(rd) = fs::read_dir(dir) else { return 0 };
    for e in rd.flatten() {
        let Ok(md) = fs::symlink_metadata(e.path()) else { continue };
        if md.is_dir() {
            total += disk_usage(&e.path());
        } else {
            total += md.blocks() * 512;
        }
    }
    total
}

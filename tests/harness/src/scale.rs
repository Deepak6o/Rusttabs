//! About 2M keys with the default memtable: load speed, random reads, scans, reopen,
//! and a 128 MiB peak-RSS cap. Keys and values are derived from their index, so the
//! harness never has to hold the values.

use crate::proc::{Ctx, Opts, Proc};
use crate::util::*;
use std::time::{Duration, Instant};

const LOAD_LIMIT: Duration = Duration::from_secs(300);
const GET_LIMIT: Duration = Duration::from_secs(180);
const SCAN_LIMIT: Duration = Duration::from_secs(180);
const OPEN_LIMIT: Duration = Duration::from_secs(60);
const RSS_LIMIT_KB: u64 = 128 * 1024;
const UPDATES: usize = 200_000;

struct Data {
    seed: u64,
    n: usize,
    ver: Vec<u8>, // 0 = deleted
}

impl Data {
    /// 16..48 bytes; the first 8 are a bijective mix of the index, so keys are unique.
    fn key(&self, i: usize, out: &mut Vec<u8>) {
        let i = i as u64;
        out.extend_from_slice(&mix(i.wrapping_add(self.seed)).to_be_bytes());
        let extra = 8 + (mix(i ^ self.seed ^ 0xA5A5_5A5A_1234_5678) % 33) as usize;
        stream_bytes(mix(i.wrapping_mul(3)) ^ self.seed, extra, out);
    }

    fn val(&self, i: usize, ver: u8, out: &mut Vec<u8>) {
        let h = mix(((i as u64) << 8 | ver as u64) ^ self.seed.rotate_left(17));
        stream_bytes(h, 50 + (h % 101) as usize, out);
    }

    fn expected_get(&self, i: usize, buf: &mut Vec<u8>) {
        buf.clear();
        if i >= self.n || self.ver[i] == 0 {
            buf.extend_from_slice(b"NONE");
            return;
        }
        let mut v = Vec::new();
        self.val(i, self.ver[i], &mut v);
        buf.extend_from_slice(b"VAL ");
        hex_into(buf, &v, false);
    }
}

fn put_line(buf: &mut Vec<u8>, d: &Data, i: usize, ver: u8, tmp: &mut Vec<u8>) {
    tmp.clear();
    if ver == 0 {
        buf.extend_from_slice(b"DEL ");
        d.key(i, tmp);
        hex_into(buf, tmp, false);
    } else {
        buf.extend_from_slice(b"PUT ");
        d.key(i, tmp);
        hex_into(buf, tmp, false);
        buf.push(b' ');
        tmp.clear();
        d.val(i, ver, tmp);
        hex_into(buf, tmp, false);
    }
    buf.push(b'\n');
}

/// Sends batches of 1000 with up to 4 outstanding.
fn send_batches(p: &mut Proc, ops: impl Iterator<Item = (usize, u8)>, d: &mut Data) -> Result<(), String> {
    let mut buf = Vec::with_capacity(1 << 20);
    let mut body = Vec::with_capacity(1 << 20);
    let mut tmp = Vec::new();
    let mut in_batch = 0;
    let mut pending = 0;
    let mut flush_batch = |p: &mut Proc, body: &mut Vec<u8>, in_batch: &mut usize, pending: &mut usize| -> Result<(), String> {
        buf.clear();
        buf.extend_from_slice(format!("BATCH {}\n", *in_batch).as_bytes());
        buf.extend_from_slice(body);
        if !p.send(&buf) || !p.flush() {
            return Err("engine stopped reading input".into());
        }
        body.clear();
        *in_batch = 0;
        *pending += 1;
        while *pending > 4 {
            expect_ok(p, "BATCH")?;
            *pending -= 1;
        }
        Ok(())
    };
    for (i, ver) in ops {
        put_line(&mut body, d, i, ver, &mut tmp);
        d.ver[i] = ver;
        in_batch += 1;
        if in_batch == 1000 {
            flush_batch(p, &mut body, &mut in_batch, &mut pending)?;
        }
    }
    if in_batch > 0 {
        flush_batch(p, &mut body, &mut in_batch, &mut pending)?;
    }
    for _ in 0..pending {
        expect_ok(p, "BATCH")?;
    }
    Ok(())
}

fn gets(p: &mut Proc, rng: &mut Rng, d: &Data, count: usize) -> Result<(), String> {
    let mut want = Vec::new();
    let mut tmp = Vec::new();
    let mut done = 0;
    while done < count {
        let chunk = 1000.min(count - done);
        let mut buf = Vec::new();
        let mut idx = Vec::with_capacity(chunk);
        for _ in 0..chunk {
            // 10% are keys that were never inserted (indexes n..2n).
            let i = if rng.chance(0.1) { d.n + rng.below(d.n as u64) as usize } else { rng.below(d.n as u64) as usize };
            tmp.clear();
            d.key(i, &mut tmp);
            buf.extend_from_slice(b"GET ");
            hex_into(&mut buf, &tmp, false);
            buf.push(b'\n');
            idx.push(i);
        }
        p.send(&buf);
        p.flush();
        for &i in &idx {
            let got = p.recv(T)?;
            d.expected_get(i, &mut want);
            if got != want {
                let mut k = Vec::new();
                d.key(i, &mut k);
                return Err(format!("GET {} returned the wrong answer", short(&k)));
            }
        }
        done += chunk;
    }
    Ok(())
}

fn scans(p: &mut Proc, rng: &mut Rng, d: &Data, sorted: &[(Vec<u8>, u32)], count: usize) -> Result<(), String> {
    let mut done = 0;
    let mut v = Vec::new();
    while done < count {
        let chunk = 50.min(count - done);
        let mut buf = Vec::new();
        let mut starts = Vec::with_capacity(chunk);
        for _ in 0..chunk {
            let mut k = Vec::new();
            d.key(rng.below(d.n as u64) as usize, &mut k);
            buf.extend_from_slice(b"SCAN ");
            hex_into(&mut buf, &k, false);
            buf.extend_from_slice(b" - 100\n");
            starts.push(k);
        }
        p.send(&buf);
        p.flush();
        for start in &starts {
            let rows = read_rows(p)?;
            let mut pos = sorted.partition_point(|(k, _)| k < start);
            let mut want = 0;
            while want < 100 && pos < sorted.len() {
                let (k, i) = &sorted[pos];
                pos += 1;
                let ver = d.ver[*i as usize];
                if ver == 0 {
                    continue;
                }
                let Some((gk, gv)) = rows.get(want) else {
                    return Err(format!("SCAN from {} returned too few rows", short(start)));
                };
                v.clear();
                d.val(*i as usize, ver, &mut v);
                if gk != k || *gv != v {
                    return Err(format!("SCAN from {}: row {want} is wrong", short(start)));
                }
                want += 1;
            }
            if rows.len() != want {
                return Err(format!("SCAN from {} returned {} rows, expected {want}", short(start), rows.len()));
            }
        }
        done += chunk;
    }
    Ok(())
}

fn timed(limit: Duration, f: impl FnOnce() -> Result<(), String>) -> Result<(), String> {
    let t0 = Instant::now();
    f()?;
    let el = t0.elapsed();
    if el > limit {
        return Err(format!("correct, but took {:.0}s (limit {}s)", el.as_secs_f64(), limit.as_secs()));
    }
    Ok(())
}

pub fn scale(ctx: &Ctx, seed: u64, n: usize) -> Vec<(String, Result<(), String>)> {
    let mut rng = Rng::new(seed);
    let mut d = Data { seed: mix(seed), n, ver: vec![0; n] };
    let dir = ctx.fresh_dir("scale");
    let opts = Opts { deadline: Duration::from_secs(1800), ..Default::default() };
    let mut out: Vec<(String, Result<(), String>)> = Vec::new();
    let names = ["load", "random GETs", "SCANs", "reopen and re-read", "peak RSS under 128 MiB"];
    let fail_rest = |out: &mut Vec<(String, Result<(), String>)>, why: &str| {
        for name in &names[out.len()..] {
            out.push((name.to_string(), Err(format!("skipped: {why}"))));
        }
    };

    let mut p = match Proc::spawn(ctx, &dir, &opts) {
        Ok(p) => p,
        Err(e) => {
            fail_rest(&mut out, &e);
            return out;
        }
    };
    let load = timed(LOAD_LIMIT, || {
        send_batches(&mut p, (0..n).map(|i| (i, 1u8)), &mut d)?;
        let updates: Vec<(usize, u8)> = (0..UPDATES)
            .map(|_| (rng.below(n as u64) as usize, if rng.chance(0.5) { 2 } else { 0 }))
            .collect();
        send_batches(&mut p, updates.into_iter(), &mut d)
    });
    let load_ok = load.is_ok();
    out.push((names[0].into(), load));
    if !load_ok {
        fail_rest(&mut out, "load failed");
        return out;
    }

    let r = timed(GET_LIMIT, || gets(&mut p, &mut rng, &d, 100_000));
    out.push((names[1].into(), r));

    let mut sorted: Vec<(Vec<u8>, u32)> = (0..n)
        .map(|i| {
            let mut k = Vec::with_capacity(48);
            d.key(i, &mut k);
            (k, i as u32)
        })
        .collect();
    sorted.sort_unstable();
    let r = timed(SCAN_LIMIT, || scans(&mut p, &mut rng, &d, &sorted, 2000));
    out.push((names[2].into(), r));

    let mut peak = p.peak_rss_kb();
    let reopen = (|| -> Result<(), String> {
        p.shutdown(Duration::from_secs(300))?;
        let mut q = Proc::spawn(ctx, &dir, &opts)?;
        timed(OPEN_LIMIT, || {
            let mut k = Vec::new();
            d.key(0, &mut k);
            let mut buf = b"GET ".to_vec();
            hex_into(&mut buf, &k, false);
            buf.push(b'\n');
            q.send(&buf);
            q.flush();
            q.recv(OPEN_LIMIT).map(|_| ())
        })
        .map_err(|e| format!("first reply after reopening: {e}"))?;
        gets(&mut q, &mut rng, &d, 20_000)?;
        scans(&mut q, &mut rng, &d, &sorted, 200)?;
        peak = peak.max(q.peak_rss_kb());
        q.shutdown(Duration::from_secs(300))
    })();
    out.push((names[3].into(), reopen));

    let data_ok = out.iter().all(|(_, r)| r.is_ok());
    let mem = if !data_ok {
        Err("skipped: memory only counts when the data read back is correct".to_string())
    } else if peak > RSS_LIMIT_KB {
        Err(format!("peak RSS {} MiB exceeds 128 MiB", peak / 1024))
    } else {
        Ok(())
    };
    out.push((names[4].into(), mem));
    println!("    scale: peak RSS {} MiB", peak / 1024);
    out
}

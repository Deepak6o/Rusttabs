//! Disk space under overwrite/delete churn: bounded without COMPACT, tight after it.

use crate::proc::{Ctx, Opts, Proc};
use crate::util::*;
use std::time::Duration;

const KEYS: usize = 60_000;
const MIB: u64 = 1 << 20;

struct Data {
    seed: u64,
    ver: Vec<u8>, // 0 = absent
    live: u64,
}

impl Data {
    fn key(&self, i: usize) -> Vec<u8> {
        let mut k = mix((i as u64).wrapping_add(self.seed)).to_be_bytes().to_vec();
        k.extend_from_slice(&mix(i as u64 ^ self.seed.rotate_left(29)).to_be_bytes());
        k
    }
    fn vlen(&self, i: usize, ver: u8) -> usize {
        1000 + (self.h(i, ver) % 101) as usize
    }
    fn h(&self, i: usize, ver: u8) -> u64 {
        mix(((i as u64) << 8 | ver as u64) ^ self.seed)
    }
    fn val(&self, i: usize, ver: u8) -> Vec<u8> {
        let mut v = Vec::new();
        stream_bytes(self.h(i, ver), self.vlen(i, ver), &mut v);
        v
    }
    fn set(&mut self, i: usize, ver: u8) {
        let old = self.ver[i];
        if old != 0 {
            self.live -= (16 + self.vlen(i, old)) as u64;
        }
        if ver != 0 {
            self.live += (16 + self.vlen(i, ver)) as u64;
        }
        self.ver[i] = ver;
    }
    fn model(&self) -> Model {
        (0..KEYS).filter(|&i| self.ver[i] != 0).map(|i| (self.key(i), self.val(i, self.ver[i]))).collect()
    }
}

struct Churn<'a> {
    p: &'a mut Proc,
    d: Data,
    dir: std::path::PathBuf,
    batches: usize,
    worst: Option<String>,
}

impl Churn<'_> {
    fn run(&mut self, ops: &[(usize, u8)]) -> Result<(), String> {
        for chunk in ops.chunks(100) {
            let mut buf = format!("BATCH {}\n", chunk.len()).into_bytes();
            for &(i, ver) in chunk {
                if ver == 0 {
                    buf.extend_from_slice(b"DEL ");
                    hex_into(&mut buf, &self.d.key(i), false);
                } else {
                    buf.extend_from_slice(b"PUT ");
                    hex_into(&mut buf, &self.d.key(i), false);
                    buf.push(b' ');
                    hex_into(&mut buf, &self.d.val(i, ver), false);
                }
                buf.push(b'\n');
                self.d.set(i, ver);
            }
            if !self.p.send(&buf) || !self.p.flush() {
                return Err("engine stopped reading input".into());
            }
            expect_ok(self.p, "BATCH")?;
            self.batches += 1;
            if self.batches % 50 == 0 {
                let used = disk_usage(&self.dir);
                let bound = 4 * self.d.live + 64 * MIB;
                if used > bound && self.worst.is_none() {
                    self.worst = Some(format!(
                        "disk usage {} MiB exceeded 4 x live ({} MiB) + 64 MiB after {} batches",
                        used / MIB,
                        self.d.live / MIB,
                        self.batches
                    ));
                }
            }
        }
        Ok(())
    }
}

pub fn space(ctx: &Ctx, seed: u64) -> Vec<(String, Result<(), String>)> {
    let names = ["bounded growth without COMPACT", "space after COMPACT", "data correct after churn and COMPACT"];
    match run(ctx, seed) {
        Ok((growth, compact, data)) => {
            let gate = |r: Result<(), String>| match (&data, r) {
                (Ok(()), r) => r,
                (Err(_), _) => Err("skipped: space only counts when the data read back is correct".into()),
            };
            vec![
                (names[0].into(), gate(growth)),
                (names[1].into(), gate(compact)),
                (names[2].into(), data),
            ]
        }
        Err(e) => names.iter().map(|n| (n.to_string(), Err(e.clone()))).collect(),
    }
}

type Three = (Result<(), String>, Result<(), String>, Result<(), String>);

fn run(ctx: &Ctx, seed: u64) -> Result<Three, String> {
    let mut rng = Rng::new(seed);
    let dir = ctx.fresh_dir("space");
    let mut p = Proc::spawn(ctx, &dir, &Opts::default())?;
    let d = Data { seed: mix(seed ^ 0x5bd1_e995), ver: vec![0; KEYS], live: 0 };
    let mut c = Churn { p: &mut p, d, dir: dir.clone(), batches: 0, worst: None };

    let mut order: Vec<usize> = (0..KEYS).collect();
    for ver in 1..=3u8 {
        rng.shuffle(&mut order);
        let ops: Vec<(usize, u8)> = order.iter().map(|&i| (i, ver)).collect();
        c.run(&ops)?;
    }
    rng.shuffle(&mut order);
    let ops: Vec<(usize, u8)> = order.iter().map(|&i| (i, if rng.chance(0.5) { 0 } else { 4 })).collect();
    c.run(&ops)?;
    rng.shuffle(&mut order);
    let ops: Vec<(usize, u8)> = order
        .iter()
        .filter_map(|&i| if c.d.ver[i] != 0 || rng.chance(0.2) { Some((i, 5)) } else { None })
        .collect();
    c.run(&ops)?;
    let growth = match c.worst.take() {
        Some(w) => Err(w),
        None => Ok(()),
    };

    c.p.send(b"COMPACT\n");
    c.p.flush();
    let r = c.p.recv(Duration::from_secs(600))?;
    let tight = 3 * c.d.live / 2 + 8 * MIB;
    let compact = if r != b"OK" {
        Err(format!("COMPACT replied {:?}", String::from_utf8_lossy(&r[..r.len().min(60)])))
    } else {
        let used = disk_usage(&dir);
        if used > tight {
            Err(format!("after COMPACT disk usage is {} MiB; limit is 1.5 x live ({} MiB) + 8 MiB", used / MIB, c.d.live / MIB))
        } else {
            Ok(())
        }
    };

    let model = c.d.model();
    let live = c.d.live;
    drop(c);
    let data = (|| -> Result<(), String> {
        let got = full_scan(&mut p)?;
        if let Some(d) = diff(&got, &model) {
            return Err(format!("full SCAN after churn: {d}"));
        }
        p.shutdown(T)?;
        let mut q = Proc::spawn(ctx, &dir, &Opts::default())?;
        let got = full_scan(&mut q)?;
        if let Some(d) = diff(&got, &model) {
            return Err(format!("full SCAN after reopen: {d}"));
        }
        q.shutdown(T)
    })();
    let compact = compact.and_then(|()| {
        let used = disk_usage(&dir);
        if used > tight {
            Err(format!("after COMPACT and reopen disk usage is {} MiB; limit {} MiB", used / MIB, tight / MIB))
        } else {
            Ok(())
        }
    });
    println!("    space: live {} MiB, final usage {} MiB", live / MIB, disk_usage(&dir) / MIB);
    Ok((growth, compact, data))
}

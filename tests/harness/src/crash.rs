//! SIGKILL recovery: after a kill, the reopened store must equal the state after some
//! prefix of the sent mutations that includes everything known to be durable.

use crate::persistence::{gen_unit, mutate, verify};
use crate::proc::{Ctx, Opts, Proc};
use crate::util::*;
use std::thread;
use std::time::Duration;

enum Sent {
    Mutation(Unit),
    Sync,
}

/// How many mutations must survive: all acknowledged ones with --sync, otherwise those
/// acknowledged before the last acknowledged SYNC.
fn durable_floor(sent: &[Sent], acked: usize, sync_mode: bool) -> usize {
    let mut units = 0;
    let mut floor = 0;
    for s in &sent[..acked] {
        match s {
            Sent::Mutation(_) => {
                units += 1;
                if sync_mode {
                    floor = units;
                }
            }
            Sent::Sync => floor = units,
        }
    }
    floor
}

/// Streams pipelined commands and SIGKILLs the engine after a random round.
/// Returns what was sent and how many replies came back.
fn crash_session(ctx: &Ctx, dir: &std::path::Path, opts: &Opts, rng: &mut Rng, keys: &[Key]) -> Result<(Vec<Sent>, usize), String> {
    let mut p = Proc::spawn(ctx, dir, opts)?;
    let rounds = rng.range(5, 60);
    let kill_round = rng.below(rounds as u64) as usize;
    let mut sent = Vec::new();
    let mut acked = 0;
    for round in 0..rounds {
        let mut buf = Vec::new();
        let n = rng.range(1, 16);
        for _ in 0..n {
            if !opts.sync && rng.chance(0.08) {
                buf.extend_from_slice(b"SYNC\n");
                sent.push(Sent::Sync);
            } else {
                let (u, as_batch) = gen_unit(rng, keys);
                encode_unit(&mut buf, &u, as_batch, false);
                sent.push(Sent::Mutation(u));
            }
        }
        if !p.send(&buf) || !p.flush() {
            return Err("engine stopped reading input before it was killed".into());
        }
        if round == kill_round {
            thread::sleep(Duration::from_micros(rng.below(3000)));
            p.kill();
            break;
        }
        for _ in 0..n {
            expect_ok(&p, "command before the kill")?;
            acked += 1;
        }
    }
    // Replies the engine wrote before it died also count as acknowledgements.
    for l in p.drain() {
        if l != b"OK" {
            return Err(format!("expected OK, got {:?}", String::from_utf8_lossy(&l[..l.len().min(60)])));
        }
        acked += 1;
    }
    Ok((sent, acked))
}

/// One trial: seed data, then 1-3 crash cycles on the same directory, each followed by
/// a prefix check and more writes; ends with a clean restart and a full comparison.
pub fn crash_trial(ctx: &Ctx, seed: u64) -> Result<(), String> {
    let mut rng = Rng::new(seed);
    let keys: Vec<Key> = (0..400)
        .map(|_| {
            let n = rng.range(4, 40);
            rng.bytes(n)
        })
        .collect();
    let dir = ctx.fresh_dir("crash");
    let mut model = Model::new();
    let mut p = Proc::spawn(ctx, &dir, &Opts::default())?;
    mutate(&mut p, &mut rng, &keys, &mut model, 300)?;
    p.shutdown(T)?;

    let cycles = rng.range(1, 3);
    for c in 0..cycles {
        let opts = Opts {
            sync: rng.chance(0.5),
            mem: *rng.pick(&[Some(16_384), Some(65_536), None]),
            ..Default::default()
        };
        let (sent, acked) = crash_session(ctx, &dir, &opts, &mut rng, &keys)?;
        let floor = durable_floor(&sent, acked, opts.sync);
        let units: Vec<Unit> = sent
            .into_iter()
            .filter_map(|s| match s {
                Sent::Mutation(u) => Some(u),
                Sent::Sync => None,
            })
            .collect();

        let mut p = Proc::spawn(ctx, &dir, &Opts::default())?;
        let got = full_scan(&mut p).map_err(|e| format!("crash {}: reopening: {e}", c + 1))?;
        let (_, recovered) = prefix_match(&got, &model, &units, floor).map_err(|e| format!("crash {}: {e}", c + 1))?;
        model = recovered;

        // The recovered store must keep working. End this session with a SYNC and a
        // kill, so the next cycle also starts from a crash.
        mutate(&mut p, &mut rng, &keys, &mut model, 100)?;
        p.send(b"SYNC\n");
        p.flush();
        expect_ok(&p, "SYNC after recovery")?;
        p.kill();
    }

    let mut p = Proc::spawn(ctx, &dir, &Opts::default())?;
    verify(&mut p, &model, "after the last crash (everything was acknowledged before SYNC)")?;
    mutate(&mut p, &mut rng, &keys, &mut model, 100)?;
    p.shutdown(T)?;
    let mut p = Proc::spawn(ctx, &dir, &Opts::default())?;
    verify(&mut p, &model, "clean restart after recovery")?;
    p.shutdown(T)
}

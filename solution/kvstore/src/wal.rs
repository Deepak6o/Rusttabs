//! Write-ahead log. One record per command (a BATCH is one record), so a torn
//! write can only damage the last record, which recovery detects and drops.
//!
//! Record: [u32 payload_len][u32 crc32(payload)][payload]
//! Payload: [u32 op_count] then per op: [u8 kind][u32 klen][key] and, for a put,
//! [u32 vlen][value].

use crate::util::{corrupt, crc32, get_u32, put_u32};
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

pub enum Op {
    Put(Vec<u8>, Vec<u8>),
    Del(Vec<u8>),
}

const KIND_PUT: u8 = 1;
const KIND_DEL: u8 = 2;
const MAX_RECORD: u32 = 1 << 31;

pub fn wal_path(dir: &Path, num: u64) -> PathBuf {
    dir.join(format!("{num:06}.log"))
}

pub struct Wal {
    file: File,
    pub num: u64,
    pub len: u64,
    buf: Vec<u8>,
}

impl Wal {
    pub fn create(dir: &Path, num: u64) -> io::Result<Wal> {
        let file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(wal_path(dir, num))?;
        Ok(Wal { file, num, len: 0, buf: Vec::new() })
    }

    /// Appends one record with a single write so it is either whole or torn at the tail.
    pub fn append(&mut self, ops: &[Op]) -> io::Result<()> {
        let mut payload = std::mem::take(&mut self.buf);
        payload.clear();
        payload.extend_from_slice(&[0u8; 8]);
        put_u32(&mut payload, ops.len() as u32);
        for op in ops {
            match op {
                Op::Put(k, v) => {
                    payload.push(KIND_PUT);
                    put_u32(&mut payload, k.len() as u32);
                    payload.extend_from_slice(k);
                    put_u32(&mut payload, v.len() as u32);
                    payload.extend_from_slice(v);
                }
                Op::Del(k) => {
                    payload.push(KIND_DEL);
                    put_u32(&mut payload, k.len() as u32);
                    payload.extend_from_slice(k);
                }
            }
        }
        let body_len = (payload.len() - 8) as u32;
        let crc = crc32(&payload[8..]);
        payload[0..4].copy_from_slice(&body_len.to_le_bytes());
        payload[4..8].copy_from_slice(&crc.to_le_bytes());
        let res = self.file.write_all(&payload);
        self.len += payload.len() as u64;
        self.buf = payload;
        if self.buf.capacity() > (8 << 20) {
            self.buf = Vec::new();
        }
        res
    }

    pub fn sync(&mut self) -> io::Result<()> {
        self.file.sync_data()
    }
}

fn decode_payload(p: &[u8]) -> Option<Vec<Op>> {
    let count = get_u32(p, 0)? as usize;
    let mut pos = 4;
    let mut ops = Vec::with_capacity(count.min(1 << 16));
    for _ in 0..count {
        let kind = *p.get(pos)?;
        pos += 1;
        let klen = get_u32(p, pos)? as usize;
        pos += 4;
        let key = p.get(pos..pos + klen)?.to_vec();
        pos += klen;
        match kind {
            KIND_PUT => {
                let vlen = get_u32(p, pos)? as usize;
                pos += 4;
                let val = p.get(pos..pos + vlen)?.to_vec();
                pos += vlen;
                ops.push(Op::Put(key, val));
            }
            KIND_DEL => ops.push(Op::Del(key)),
            _ => return None,
        }
    }
    if pos != p.len() {
        return None;
    }
    Some(ops)
}

/// Replays every intact record of a log through `apply`. Returns (valid_length, clean),
/// where `clean` is false if the log ended in a torn or corrupt record.
pub fn replay(path: &Path, mut apply: impl FnMut(Vec<Op>)) -> io::Result<(u64, bool)> {
    let mut file = File::open(path)?;
    let file_len = file.metadata()?.len();
    let mut reader = io::BufReader::with_capacity(1 << 20, &mut file);
    let mut good: u64 = 0;
    let mut header = [0u8; 8];
    loop {
        if good == file_len {
            return Ok((good, true));
        }
        if file_len - good < 8 || reader.read_exact(&mut header).is_err() {
            return Ok((good, false));
        }
        let len = u32::from_le_bytes(header[0..4].try_into().unwrap());
        let crc = u32::from_le_bytes(header[4..8].try_into().unwrap());
        if len > MAX_RECORD || (len as u64) > file_len - good - 8 {
            return Ok((good, false));
        }
        let mut payload = vec![0u8; len as usize];
        if reader.read_exact(&mut payload).is_err() || crc32(&payload) != crc {
            return Ok((good, false));
        }
        match decode_payload(&payload) {
            Some(ops) => apply(ops),
            None => return Ok((good, false)),
        }
        good += 8 + len as u64;
    }
}

/// Cuts a log back to its last intact record.
pub fn truncate(path: &Path, len: u64) -> io::Result<()> {
    let f = OpenOptions::new().write(true).open(path)?;
    f.set_len(len)?;
    f.sync_all()?;
    if f.metadata()?.len() != len {
        return Err(corrupt("log truncate failed"));
    }
    Ok(())
}

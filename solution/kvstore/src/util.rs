//! Checksums, hex coding and little-endian helpers.

use std::fs::File;
use std::io;
use std::path::Path;

// ---------- CRC-32 (IEEE), slicing-by-8 ----------

static CRC_TABLES: std::sync::OnceLock<[[u32; 256]; 8]> = std::sync::OnceLock::new();

fn tables() -> &'static [[u32; 256]; 8] {
    CRC_TABLES.get_or_init(|| {
        let mut t = [[0u32; 256]; 8];
        for i in 0..256u32 {
            let mut c = i;
            for _ in 0..8 {
                c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
            }
            t[0][i as usize] = c;
        }
        for i in 0..256 {
            for k in 1..8 {
                let prev = t[k - 1][i];
                t[k][i] = (prev >> 8) ^ t[0][(prev & 0xff) as usize];
            }
        }
        t
    })
}

pub fn crc32(data: &[u8]) -> u32 {
    let t = tables();
    let mut crc = !0u32;
    let mut chunks = data.chunks_exact(8);
    for c in &mut chunks {
        let lo = u32::from_le_bytes([c[0], c[1], c[2], c[3]]) ^ crc;
        let hi = u32::from_le_bytes([c[4], c[5], c[6], c[7]]);
        crc = t[7][(lo & 0xff) as usize]
            ^ t[6][((lo >> 8) & 0xff) as usize]
            ^ t[5][((lo >> 16) & 0xff) as usize]
            ^ t[4][(lo >> 24) as usize]
            ^ t[3][(hi & 0xff) as usize]
            ^ t[2][((hi >> 8) & 0xff) as usize]
            ^ t[1][((hi >> 16) & 0xff) as usize]
            ^ t[0][(hi >> 24) as usize];
    }
    for &b in chunks.remainder() {
        crc = (crc >> 8) ^ t[0][((crc ^ b as u32) & 0xff) as usize];
    }
    !crc
}

// ---------- hex ----------

const HEX: &[u8; 16] = b"0123456789abcdef";

fn nibble(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

pub fn hex_decode(s: &[u8]) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 {
        return None;
    }
    let mut out = Vec::with_capacity(s.len() / 2);
    for pair in s.chunks_exact(2) {
        out.push((nibble(pair[0])? << 4) | nibble(pair[1])?);
    }
    Some(out)
}

pub fn hex_encode_into(out: &mut Vec<u8>, data: &[u8]) {
    out.reserve(data.len() * 2);
    for &b in data {
        out.push(HEX[(b >> 4) as usize]);
        out.push(HEX[(b & 0xf) as usize]);
    }
}

// ---------- byte helpers ----------

pub fn put_u32(buf: &mut Vec<u8>, v: u32) {
    buf.extend_from_slice(&v.to_le_bytes());
}

pub fn put_u64(buf: &mut Vec<u8>, v: u64) {
    buf.extend_from_slice(&v.to_le_bytes());
}

pub fn get_u32(buf: &[u8], pos: usize) -> Option<u32> {
    buf.get(pos..pos + 4).map(|b| u32::from_le_bytes(b.try_into().unwrap()))
}

pub fn get_u64(buf: &[u8], pos: usize) -> Option<u64> {
    buf.get(pos..pos + 8).map(|b| u64::from_le_bytes(b.try_into().unwrap()))
}

pub fn corrupt(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.to_string())
}

/// fsync a directory so that file creations, renames and deletions in it are durable.
pub fn sync_dir(dir: &Path) -> io::Result<()> {
    File::open(dir)?.sync_all()
}

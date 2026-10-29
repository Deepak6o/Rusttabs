//! Immutable sorted table files.
//!
//! Layout: data blocks, then an index block, then a 32-byte footer.
//!   block  = entries... [u32 crc32(entries)]
//!   entry  = [u32 klen][u32 vlen | TOMBSTONE][key][value]
//!   index  = per block: [u32 klen][last key][u64 offset][u32 length incl. crc]
//!   footer = [u64 index_offset][u32 index_len][u32 index_crc][u64 entries][u64 MAGIC]
//!
//! Tables are written to `<num>.sst.tmp`, fsynced, then renamed, so a torn write can
//! only ever damage a file that no manifest refers to.

use crate::util::{corrupt, crc32, get_u32, get_u64, put_u32, put_u64, sync_dir};
use std::fs::{self, File};
use std::io::{self, BufWriter, Write};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};

const TOMBSTONE: u32 = u32::MAX;
const MAGIC: u64 = 0x4b56_5354_4f52_4531; // "KVSTORE1"
const FOOTER: usize = 32;
const BLOCK_TARGET: usize = 4096;

pub fn sst_path(dir: &Path, num: u64) -> PathBuf {
    dir.join(format!("{num:06}.sst"))
}

pub struct SstWriter {
    w: BufWriter<File>,
    tmp: PathBuf,
    path: PathBuf,
    dir: PathBuf,
    num: u64,
    offset: u64,
    block: Vec<u8>,
    last_key: Vec<u8>,
    index: Vec<u8>,
    pub entries: u64,
}

impl SstWriter {
    pub fn create(dir: &Path, num: u64) -> io::Result<SstWriter> {
        let path = sst_path(dir, num);
        let tmp = dir.join(format!("{num:06}.sst.tmp"));
        let file = File::create(&tmp)?;
        Ok(SstWriter {
            w: BufWriter::with_capacity(1 << 16, file),
            tmp,
            path,
            dir: dir.to_path_buf(),
            num,
            offset: 0,
            block: Vec::with_capacity(BLOCK_TARGET * 2),
            last_key: Vec::new(),
            index: Vec::new(),
            entries: 0,
        })
    }

    /// Keys must be added in strictly increasing order.
    pub fn add(&mut self, key: &[u8], val: Option<&[u8]>) -> io::Result<()> {
        put_u32(&mut self.block, key.len() as u32);
        put_u32(&mut self.block, val.map_or(TOMBSTONE, |v| v.len() as u32));
        self.block.extend_from_slice(key);
        if let Some(v) = val {
            self.block.extend_from_slice(v);
        }
        self.last_key.clear();
        self.last_key.extend_from_slice(key);
        self.entries += 1;
        if self.block.len() >= BLOCK_TARGET {
            self.finish_block()?;
        }
        Ok(())
    }

    fn finish_block(&mut self) -> io::Result<()> {
        if self.block.is_empty() {
            return Ok(());
        }
        let crc = crc32(&self.block);
        put_u32(&mut self.block, crc);
        self.w.write_all(&self.block)?;
        put_u32(&mut self.index, self.last_key.len() as u32);
        self.index.extend_from_slice(&self.last_key);
        put_u64(&mut self.index, self.offset);
        put_u32(&mut self.index, self.block.len() as u32);
        self.offset += self.block.len() as u64;
        self.block.clear();
        Ok(())
    }

    /// Writes index and footer, fsyncs, renames into place and opens the result.
    pub fn finish(mut self) -> io::Result<Sst> {
        self.finish_block()?;
        let index_offset = self.offset;
        let mut tail = std::mem::take(&mut self.index);
        let index_len = tail.len() as u32;
        let index_crc = crc32(&tail);
        put_u64(&mut tail, index_offset);
        put_u32(&mut tail, index_len);
        put_u32(&mut tail, index_crc);
        put_u64(&mut tail, self.entries);
        put_u64(&mut tail, MAGIC);
        self.w.write_all(&tail)?;
        let file = self.w.into_inner().map_err(|e| e.into_error())?;
        file.sync_all()?;
        drop(file);
        fs::rename(&self.tmp, &self.path)?;
        sync_dir(&self.dir)?;
        Sst::open(&self.dir, self.num)
    }
}

struct IndexEntry {
    last_key: Vec<u8>,
    offset: u64,
    len: u32,
}

pub struct Sst {
    pub num: u64,
    file: File,
    index: Vec<IndexEntry>,
    pub entries: u64,
}

pub enum Lookup {
    Absent,
    Deleted,
    Value(Vec<u8>),
}

impl Sst {
    pub fn open(dir: &Path, num: u64) -> io::Result<Sst> {
        let file = File::open(sst_path(dir, num))?;
        let size = file.metadata()?.len();
        if size < FOOTER as u64 {
            return Err(corrupt("table too short"));
        }
        let mut footer = [0u8; FOOTER];
        file.read_exact_at(&mut footer, size - FOOTER as u64)?;
        let index_offset = get_u64(&footer, 0).unwrap();
        let index_len = get_u32(&footer, 8).unwrap() as u64;
        let index_crc = get_u32(&footer, 12).unwrap();
        let entries = get_u64(&footer, 16).unwrap();
        if get_u64(&footer, 24).unwrap() != MAGIC || index_offset + index_len + FOOTER as u64 != size {
            return Err(corrupt("bad table footer"));
        }
        let mut raw = vec![0u8; index_len as usize];
        file.read_exact_at(&mut raw, index_offset)?;
        if crc32(&raw) != index_crc {
            return Err(corrupt("bad table index checksum"));
        }
        let mut index = Vec::new();
        let mut pos = 0;
        while pos < raw.len() {
            let klen = get_u32(&raw, pos).ok_or_else(|| corrupt("index"))? as usize;
            pos += 4;
            let last_key = raw.get(pos..pos + klen).ok_or_else(|| corrupt("index"))?.to_vec();
            pos += klen;
            let offset = get_u64(&raw, pos).ok_or_else(|| corrupt("index"))?;
            let len = get_u32(&raw, pos + 8).ok_or_else(|| corrupt("index"))?;
            pos += 12;
            index.push(IndexEntry { last_key, offset, len });
        }
        Ok(Sst { num, file, index, entries })
    }

    fn read_block(&self, i: usize) -> io::Result<Vec<u8>> {
        let e = &self.index[i];
        let mut buf = vec![0u8; e.len as usize];
        self.file.read_exact_at(&mut buf, e.offset)?;
        if buf.len() < 4 {
            return Err(corrupt("short block"));
        }
        let body = buf.len() - 4;
        if crc32(&buf[..body]) != get_u32(&buf, body).unwrap() {
            return Err(corrupt("bad block checksum"));
        }
        buf.truncate(body);
        Ok(buf)
    }

    /// Index of the first block whose last key is >= key.
    fn block_for(&self, key: &[u8]) -> usize {
        self.index.partition_point(|e| e.last_key.as_slice() < key)
    }

    pub fn get(&self, key: &[u8]) -> io::Result<Lookup> {
        let i = self.block_for(key);
        if i == self.index.len() {
            return Ok(Lookup::Absent);
        }
        let block = self.read_block(i)?;
        let mut pos = 0;
        while pos < block.len() {
            let (k, v, next) = decode_entry(&block, pos)?;
            match k.cmp(key) {
                std::cmp::Ordering::Less => pos = next,
                std::cmp::Ordering::Equal => {
                    return Ok(match v {
                        Some(v) => Lookup::Value(v.to_vec()),
                        None => Lookup::Deleted,
                    })
                }
                std::cmp::Ordering::Greater => break,
            }
        }
        Ok(Lookup::Absent)
    }

    pub fn iter_from<'a>(&'a self, start: Option<&[u8]>) -> io::Result<SstIter<'a>> {
        let block_idx = match start {
            Some(k) => self.block_for(k),
            None => 0,
        };
        let mut it = SstIter { sst: self, block_idx, block: Vec::new(), pos: 0 };
        it.load()?;
        if let Some(k) = start {
            while let Some((key, _)) = it.peek()? {
                if key >= k {
                    break;
                }
                it.advance()?;
            }
        }
        Ok(it)
    }
}

fn decode_entry(block: &[u8], pos: usize) -> io::Result<(&[u8], Option<&[u8]>, usize)> {
    let klen = get_u32(block, pos).ok_or_else(|| corrupt("entry"))? as usize;
    let vlen = get_u32(block, pos + 4).ok_or_else(|| corrupt("entry"))?;
    let kstart = pos + 8;
    let key = block.get(kstart..kstart + klen).ok_or_else(|| corrupt("entry"))?;
    if vlen == TOMBSTONE {
        return Ok((key, None, kstart + klen));
    }
    let vstart = kstart + klen;
    let val = block.get(vstart..vstart + vlen as usize).ok_or_else(|| corrupt("entry"))?;
    Ok((key, Some(val), vstart + vlen as usize))
}

pub struct SstIter<'a> {
    sst: &'a Sst,
    block_idx: usize,
    block: Vec<u8>,
    pos: usize,
}

impl<'a> SstIter<'a> {
    fn load(&mut self) -> io::Result<()> {
        self.pos = 0;
        self.block.clear();
        while self.block_idx < self.sst.index.len() {
            self.block = self.sst.read_block(self.block_idx)?;
            if !self.block.is_empty() {
                break;
            }
            self.block_idx += 1;
        }
        Ok(())
    }

    pub fn peek(&self) -> io::Result<Option<(&[u8], Option<&[u8]>)>> {
        if self.pos >= self.block.len() {
            return Ok(None);
        }
        let (k, v, _) = decode_entry(&self.block, self.pos)?;
        Ok(Some((k, v)))
    }

    pub fn advance(&mut self) -> io::Result<()> {
        if self.pos >= self.block.len() {
            return Ok(());
        }
        let (_, _, next) = decode_entry(&self.block, self.pos)?;
        self.pos = next;
        if self.pos >= self.block.len() {
            self.block_idx += 1;
            self.load()?;
        }
        Ok(())
    }
}

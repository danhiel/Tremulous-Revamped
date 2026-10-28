//! The 4K texture pack's files (`.hdtex` albedo, `.hdnrm` normal map;
//! trem_godot hdtex.rs, dec044 / dec055), without Godot.
//!
//! * `TREMHD01`: a 40-byte header (magic, width, height, Godot image
//!   format, mip count, alpha flag, mean rgb) and the GPU-ready mip chain
//!   (S3TC blocks or RGBA8) as Godot's `Image::get_data` lays it out.
//! * `TREMHD02` (dec055): the same header, then the level count, each
//!   level's compressed size and the levels, each one zstd frame. The
//!   levels decompress to exactly the v1 bytes (lossless: the GPU gets the
//!   same blocks), and the larger ones can still be skipped unread
//!   (`r_hdMaxSize`).
//! * `TREMHDLN` (dec055): a byte-identical duplicate stands in for by a
//!   link to the file that has the data (its path in the pack, '/').

use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

pub const MAGIC_V1: &[u8; 8] = b"TREMHD01";
pub const MAGIC_V2: &[u8; 8] = b"TREMHD02";
pub const MAGIC_LINK: &[u8; 8] = b"TREMHDLN";
/// magic, w, h, format, mip count, alpha flag, mean rgb
pub const HEADER: usize = 8 + 4 * 5 + 4 * 3;

/// Godot `Image.Format` numbers of the block-compressed formats the pack uses.
pub const FORMAT_DXT1: u32 = 17;
pub const FORMAT_DXT3: u32 = 18;
pub const FORMAT_DXT5: u32 = 19;
pub const FORMAT_RGTC_R: u32 = 20;
pub const FORMAT_RGTC_RG: u32 = 21;
pub const FORMAT_BPTC_RGBA: u32 = 22;

/// Bytes per 4x4 block, None for the uncompressed formats.
pub fn block_bytes(fmt: u32) -> Option<usize> {
    match fmt {
        FORMAT_DXT1 | FORMAT_RGTC_R => Some(8),
        FORMAT_DXT3 | FORMAT_DXT5 | FORMAT_RGTC_RG | FORMAT_BPTC_RGBA => Some(16),
        _ => None,
    }
}

/// One mip level's size (the uncompressed formats the pack writes are RGBA8).
pub fn level_bytes(fmt: u32, w: usize, h: usize) -> usize {
    match block_bytes(fmt) {
        Some(b) => w.div_ceil(4).max(1) * h.div_ceil(4).max(1) * b,
        None => w * h * 4,
    }
}

/// Every level's size, from the full size down.
pub fn level_sizes(fmt: u32, w: usize, h: usize, mips: usize) -> Vec<usize> {
    let (mut w, mut h) = (w, h);
    let mut v = Vec::new();
    for _ in 0..mips.max(1) {
        v.push(level_bytes(fmt, w, h));
        w = (w / 2).max(1);
        h = (h / 2).max(1);
    }
    v
}

/// The header's fields.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Header {
    pub w: usize,
    pub h: usize,
    pub fmt: u32,
    pub mips: usize,
    pub alpha: bool,
    pub mean: [f32; 3],
}

impl Header {
    pub fn parse(b: &[u8]) -> Option<Header> {
        if b.len() < HEADER {
            return None;
        }
        let u = |i: usize| u32::from_le_bytes(b[8 + 4 * i..12 + 4 * i].try_into().unwrap());
        let f = |k: usize| f32::from_le_bytes(b[28 + 4 * k..32 + 4 * k].try_into().unwrap());
        Some(Header { w: u(0) as usize, h: u(1) as usize, fmt: u(2), mips: u(3) as usize, alpha: u(4) != 0, mean: [f(0), f(1), f(2)] })
    }

    pub fn bytes(&self, magic: &[u8; 8]) -> Vec<u8> {
        let mut b = magic.to_vec();
        for v in [self.w as u32, self.h as u32, self.fmt, self.mips as u32, self.alpha as u32] {
            b.extend_from_slice(&v.to_le_bytes());
        }
        for m in self.mean {
            b.extend_from_slice(&m.to_le_bytes());
        }
        b
    }
}

/// What `read` hands back: the levels kept (after skipping those larger
/// than `max_size`), their size and data.
#[derive(Debug, PartialEq)]
pub struct Texture {
    pub header: Header,
    pub w: usize,
    pub h: usize,
    pub levels: usize,
    pub data: Vec<u8>,
}

/// How many leading levels are larger than `max_size` (one is always kept).
fn levels_to_skip(hd: &Header, max_size: usize) -> usize {
    let (mut w, mut h, mut skip) = (hd.w, hd.h, 0);
    while (w > max_size || h > max_size) && skip + 1 < hd.mips.max(1) {
        w = (w / 2).max(1);
        h = (h / 2).max(1);
        skip += 1;
    }
    skip
}

/// The v2 file of a v1 file's bytes, each level at zstd `level`.
pub fn encode_v2(v1: &[u8], level: i32) -> std::io::Result<Vec<u8>> {
    let bad = || std::io::Error::new(std::io::ErrorKind::InvalidData, "not a TREMHD01 file");
    if v1.get(..8) != Some(MAGIC_V1.as_slice()) {
        return Err(bad());
    }
    let hd = Header::parse(v1).ok_or_else(bad)?;
    encode_levels(&hd, &v1[HEADER..], level)
}

/// The v2 file of a header and its mip chain.
pub fn encode_levels(hd: &Header, body: &[u8], level: i32) -> std::io::Result<Vec<u8>> {
    let sizes = level_sizes(hd.fmt, hd.w, hd.h, hd.mips);
    if sizes.iter().sum::<usize>() != body.len() {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, format!("mip chain is {} bytes, the header says {}", body.len(), sizes.iter().sum::<usize>())));
    }
    let mut out = hd.bytes(MAGIC_V2);
    out.extend_from_slice(&(sizes.len() as u32).to_le_bytes());
    let mut chunks = Vec::with_capacity(sizes.len());
    let mut at = 0;
    for s in &sizes {
        chunks.push(zstd::bulk::compress(&body[at..at + s], level)?);
        at += s;
    }
    for c in &chunks {
        out.extend_from_slice(&(c.len() as u32).to_le_bytes());
    }
    for c in chunks {
        out.extend_from_slice(&c);
    }
    Ok(out)
}

/// A link file to `target` (its path in the pack, '/' separators).
pub fn link_bytes(target: &str) -> Vec<u8> {
    let mut b = MAGIC_LINK.to_vec();
    b.extend_from_slice(&(target.len() as u32).to_le_bytes());
    b.extend_from_slice(target.as_bytes());
    b
}

/// The target of a link file's bytes.
pub fn link_target(b: &[u8]) -> Option<String> {
    if b.get(..8) != Some(MAGIC_LINK.as_slice()) || b.len() < 12 {
        return None;
    }
    let n = u32::from_le_bytes(b[8..12].try_into().unwrap()) as usize;
    let t = std::str::from_utf8(b.get(12..12 + n)?).ok()?;
    // a pack-relative path, never out of the pack: plain '/'-separated
    // names only, so no "..", no absolute, drive or UNC (`\\host\share`,
    // which Windows would open over the network) path
    let plain = |s: &str| !s.is_empty() && s != "." && s != ".." && !s.contains(['\\', ':', '\0']);
    t.split('/').all(plain).then(|| t.to_string())
}

/// Largest side a pack texture may have.
pub const MAX_SIZE: usize = 16384;

/// A header the pack could have written: a known format, a sane size and
/// at most a full mip chain (a damaged or hostile file must not make
/// `read` allocate gigabytes).
fn header_ok(hd: &Header) -> bool {
    let known = block_bytes(hd.fmt).is_some() || hd.fmt == 5; // 5: RGBA8
    let full_chain = (usize::BITS - hd.w.max(hd.h).leading_zeros()) as usize;
    known && (1..=MAX_SIZE).contains(&hd.w) && (1..=MAX_SIZE).contains(&hd.h) && (1..=full_chain).contains(&hd.mips)
}

/// Reads `rel` (a path in the pack at `root`), following one link, and
/// keeps the levels up to `max_size`. None: missing or not a pack file.
pub fn read(root: &Path, rel: &str, max_size: usize) -> Option<Texture> {
    let mut f = std::fs::File::open(root.join(rel)).ok()?;
    let mut head = [0u8; HEADER];
    let n = f.read(&mut head).ok()?;
    if head[..8] == *MAGIC_LINK {
        let mut b = head[..n].to_vec();
        f.read_to_end(&mut b).ok()?;
        let t = link_target(&b)?;
        let mut f = std::fs::File::open(root.join(&t)).ok()?;
        let mut head = [0u8; HEADER];
        f.read_exact(&mut head).ok()?;
        return read_open(&mut f, &head, max_size);
    }
    if n < HEADER {
        return None;
    }
    read_open(&mut f, &head, max_size)
}

/// A v1 / v2 file after its header.
fn read_open(f: &mut std::fs::File, head: &[u8; HEADER], max_size: usize) -> Option<Texture> {
    let hd = Header::parse(head)?;
    if !header_ok(&hd) {
        return None;
    }
    let sizes = level_sizes(hd.fmt, hd.w, hd.h, hd.mips);
    let skip = levels_to_skip(&hd, max_size);
    let (mut w, mut h) = (hd.w, hd.h);
    for _ in 0..skip {
        w = (w / 2).max(1);
        h = (h / 2).max(1);
    }
    let levels = sizes.len() - skip;
    let total: usize = sizes[skip..].iter().sum();
    let mut data = Vec::with_capacity(total);
    if head[..8] == *MAGIC_V1 {
        f.seek(SeekFrom::Current(sizes[..skip].iter().sum::<usize>() as i64)).ok()?;
        data.resize(total, 0);
        f.read_exact(&mut data).ok()?;
    } else if head[..8] == *MAGIC_V2 {
        let mut nb = [0u8; 4];
        f.read_exact(&mut nb).ok()?;
        let n = u32::from_le_bytes(nb) as usize;
        if n != sizes.len() {
            return None;
        }
        let mut table = vec![0u8; 4 * n];
        f.read_exact(&mut table).ok()?;
        let csize: Vec<usize> = table.chunks(4).map(|c| u32::from_le_bytes(c.try_into().unwrap()) as usize).collect();
        // zstd never grows a level by more than this (ZSTD_compressBound)
        if csize.iter().zip(&sizes).any(|(&c, &s)| c > s + s / 128 + 1024) {
            return None;
        }
        f.seek(SeekFrom::Current(csize[..skip].iter().sum::<usize>() as i64)).ok()?;
        for i in skip..n {
            let mut c = vec![0u8; csize[i]];
            f.read_exact(&mut c).ok()?;
            let d = zstd::bulk::decompress(&c, sizes[i]).ok()?;
            if d.len() != sizes[i] {
                return None;
            }
            data.extend_from_slice(&d);
        }
    } else {
        return None;
    }
    Some(Texture { header: hd, w, h, levels, data })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v1(w: usize, h: usize, fmt: u32, mips: usize) -> Vec<u8> {
        let hd = Header { w, h, fmt, mips, alpha: false, mean: [0.25, 0.5, 0.75] };
        let mut b = hd.bytes(MAGIC_V1);
        let n: usize = level_sizes(fmt, w, h, mips).iter().sum();
        // smooth, compressible data with some noise
        let mut x = 12345u32;
        for i in 0..n {
            x = x.wrapping_mul(1103515245).wrapping_add(12345);
            b.push(((i / 64) as u8).wrapping_add((x >> 28) as u8));
        }
        b
    }

    fn tmp(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("trem_hdpack_test_{}_{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("textures")).unwrap();
        d
    }

    #[test]
    fn sizes() {
        assert_eq!(level_bytes(FORMAT_DXT1, 4096, 4096), 8 * 1024 * 1024);
        assert_eq!(level_bytes(FORMAT_DXT5, 2, 1), 16);
        assert_eq!(level_sizes(FORMAT_DXT1, 8, 4, 4), vec![16, 8, 8, 8]);
        assert_eq!(level_bytes(5, 3, 2), 24);
    }

    #[test]
    fn v2_round_trips_every_level_exactly() {
        let d = tmp("v2");
        let a = v1(256, 128, FORMAT_DXT5, 9);
        let b = encode_v2(&a, 3).unwrap();
        assert!(b.len() < a.len(), "{} < {}", b.len(), a.len());
        std::fs::write(d.join("textures/a.hdtex"), &a).unwrap();
        std::fs::write(d.join("textures/b.hdtex"), &b).unwrap();
        for max in [4096, 128, 64, 1] {
            let ra = read(&d, "textures/a.hdtex", max).unwrap();
            let rb = read(&d, "textures/b.hdtex", max).unwrap();
            assert_eq!(ra, rb, "max {max}");
        }
        let full = read(&d, "textures/b.hdtex", 4096).unwrap();
        assert_eq!(full.data, a[HEADER..]);
        assert_eq!((full.w, full.h, full.levels), (256, 128, 9));
        let half = read(&d, "textures/b.hdtex", 128).unwrap();
        assert_eq!((half.w, half.h, half.levels), (128, 64, 8));
        assert_eq!(half.header.mean, [0.25, 0.5, 0.75]);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn links_follow_once_and_stay_in_the_pack() {
        let d = tmp("link");
        let a = v1(64, 64, FORMAT_DXT1, 7);
        std::fs::write(d.join("textures/a.hdtex"), encode_v2(&a, 3).unwrap()).unwrap();
        std::fs::write(d.join("textures/dup.hdtex"), link_bytes("textures/a.hdtex")).unwrap();
        std::fs::write(d.join("textures/loop.hdtex"), link_bytes("textures/dup.hdtex")).unwrap();
        assert_eq!(read(&d, "textures/dup.hdtex", 4096).unwrap().data, a[HEADER..]);
        assert!(read(&d, "textures/loop.hdtex", 4096).is_none(), "a link to a link is not followed");
        assert_eq!(link_target(&link_bytes("../x")), None);
        assert_eq!(link_target(&link_bytes("C:/x")), None);
        assert_eq!(link_target(&link_bytes("\\\\host\\share\\x.hdtex")), None);
        assert_eq!(link_target(&link_bytes("textures\\..\\..\\x")), None);
        assert_eq!(link_target(&link_bytes("/x")), None);
        assert_eq!(link_target(&link_bytes("textures//x")), None);
        assert_eq!(link_target(&link_bytes("textures/a.hdtex")).as_deref(), Some("textures/a.hdtex"));
        assert!(read(&d, "textures/missing.hdtex", 4096).is_none());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn bad_input() {
        assert!(encode_v2(b"TREMHD02xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx", 3).is_err());
        let mut short = v1(16, 16, FORMAT_DXT1, 5);
        short.pop();
        assert!(encode_v2(&short, 3).is_err());
    }

    #[test]
    fn hostile_headers_are_refused_before_allocating() {
        let d = tmp("hostile");
        let file = |w, h, fmt, mips| Header { w, h, fmt, mips, alpha: false, mean: [0.0; 3] }.bytes(MAGIC_V1);
        for (name, b) in [
            ("mips", file(16, 16, FORMAT_DXT1, u32::MAX as usize)),
            ("huge", file(u32::MAX as usize, u32::MAX as usize, 5, 1)),
            ("zero", file(0, 16, FORMAT_DXT1, 1)),
            ("fmt", file(16, 16, 99, 1)),
        ] {
            std::fs::write(d.join(format!("textures/{name}.hdtex")), &b).unwrap();
            assert!(read(&d, &format!("textures/{name}.hdtex"), 4096).is_none(), "{name}");
        }
        // a v2 level table claiming a 4 GB compressed level
        let mut b = Header { w: 16, h: 16, fmt: FORMAT_DXT1, mips: 1, alpha: false, mean: [0.0; 3] }.bytes(MAGIC_V2);
        b.extend_from_slice(&1u32.to_le_bytes());
        b.extend_from_slice(&u32::MAX.to_le_bytes());
        std::fs::write(d.join("textures/csize.hdtex"), &b).unwrap();
        assert!(read(&d, "textures/csize.hdtex", 4096).is_none());
        let _ = std::fs::remove_dir_all(&d);
    }
}

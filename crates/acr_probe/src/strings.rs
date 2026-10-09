//! `strings`: ASCII and UTF-16LE substring search over readable memory.

use crate::mem::{chunk_jobs, read_segments, MemRegion, ReadMem};
use rayon::prelude::*;
use serde::Serialize;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Encoding {
    Ascii,
    Utf16,
}

#[derive(Clone, Debug, Serialize)]
pub struct StrHit {
    pub addr: usize,
    pub encoding: Encoding,
    /// The whole printable string around the match.
    pub text: String,
}

/// Lower-cases ASCII letters (a no-op unless `fold`).
fn fold_byte(b: u8, fold: bool) -> u8 {
    if fold {
        b.to_ascii_lowercase()
    } else {
        b
    }
}

fn needles(pattern: &str, fold: bool) -> Vec<(Encoding, Vec<u8>)> {
    let ascii: Vec<u8> = pattern.bytes().map(|b| fold_byte(b, fold)).collect();
    let utf16: Vec<u8> = pattern
        .encode_utf16()
        .flat_map(|u| {
            let u = if fold && u < 0x80 { (u as u8).to_ascii_lowercase() as u16 } else { u };
            u.to_le_bytes()
        })
        .collect();
    vec![(Encoding::Ascii, ascii), (Encoding::Utf16, utf16)]
}

fn printable(b: u8) -> bool {
    (0x20..0x7f).contains(&b) || b == b'\t'
}

/// Expands a hit at `at` to the surrounding printable run (max 256 chars each side).
fn expand(buf: &[u8], at: usize, len: usize, enc: Encoding) -> (usize, String) {
    match enc {
        Encoding::Ascii => {
            let mut s = at;
            while s > 0 && at - s < 256 && printable(buf[s - 1]) {
                s -= 1;
            }
            let mut e = at + len;
            while e < buf.len() && e - at < 256 + len && printable(buf[e]) {
                e += 1;
            }
            (s, String::from_utf8_lossy(&buf[s..e]).into_owned())
        }
        Encoding::Utf16 => {
            let ch =
                |i: usize| -> Option<u16> { (i + 1 < buf.len()).then(|| u16::from_le_bytes([buf[i], buf[i + 1]])) };
            let ok = |c: u16| c < 0x80 && printable(c as u8);
            let mut s = at;
            while s >= 2 && at - s < 512 && ch(s - 2).is_some_and(ok) {
                s -= 2;
            }
            let mut e = at + len;
            while e - at < 512 + len && ch(e).is_some_and(ok) {
                e += 2;
            }
            let units: Vec<u16> = (s..e).step_by(2).filter_map(ch).collect();
            (s, String::from_utf16_lossy(&units))
        }
    }
}

/// Searches `buf` (located at `base`); only hits starting before `owned_end` are reported.
pub fn search_buf(
    buf: &[u8],
    base: usize,
    owned_end: usize,
    pattern: &str,
    fold: bool,
    out: &mut Vec<StrHit>,
    max: usize,
) {
    let hay_folded;
    let hay: &[u8] = if fold {
        hay_folded = buf.iter().map(|b| b.to_ascii_lowercase()).collect::<Vec<u8>>();
        &hay_folded
    } else {
        buf
    };
    for (enc, n) in needles(pattern, fold) {
        if n.is_empty() {
            continue;
        }
        for at in memchr::memmem::find_iter(hay, &n) {
            let addr = base + at;
            if addr >= owned_end || out.len() >= max {
                break;
            }
            if enc == Encoding::Utf16 && at % 2 != 0 {
                continue;
            }
            let (s, text) = expand(buf, at, n.len(), enc);
            let start = base + s;
            if out.iter().any(|h| h.addr == start && h.encoding == enc) {
                continue;
            }
            out.push(StrHit { addr: start, encoding: enc, text });
        }
    }
}

pub fn search<R: ReadMem>(mem: &R, regions: &[MemRegion], pattern: &str, fold: bool, max: usize) -> Vec<StrHit> {
    let overlap = pattern.len() * 2 + 1024;
    let jobs = chunk_jobs(regions, 4 << 20, overlap);
    let mut hits: Vec<StrHit> = jobs
        .par_iter()
        .flat_map_iter(|j| {
            let mut buf = vec![0u8; j.read_len];
            let segs = read_segments(mem, j.base, &mut buf);
            let mut out = Vec::new();
            for (s, e) in segs {
                let owned_end = (j.base + j.len).min(j.base + e);
                search_buf(&buf[s..e], j.base + s, owned_end, pattern, fold, &mut out, max);
            }
            out
        })
        .collect();
    hits.sort_by_key(|h| (h.addr, h.encoding == Encoding::Utf16));
    hits.dedup_by_key(|h| (h.addr, h.encoding));
    hits.truncate(max);
    hits
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mem::{FakeMem, RegionKind};

    #[test]
    fn finds_ascii_and_utf16_case_insensitive() {
        let mut data = vec![0u8; 0x3000];
        data[0x100..0x100 + 22].copy_from_slice(b"BP_RallyCarPawn_C\0abc\0");
        let w = acr_ue::pe::utf16_bytes("Rally.SimCar");
        data[0x2ffe - w.len()..0x2ffe].copy_from_slice(&w);
        let mut m = FakeMem::new();
        m.add(0x10_0000, data, RegionKind::Image, false);
        let regions = m.regions();
        let hits = search(&m, &regions, "rally", true, 100);
        assert_eq!(hits.len(), 2, "{hits:?}");
        assert_eq!(hits[0].addr, 0x10_0100);
        assert_eq!(hits[0].text, "BP_RallyCarPawn_C");
        assert_eq!(hits[1].encoding, Encoding::Utf16);
        assert_eq!(hits[1].text, "Rally.SimCar");
        assert!(search(&m, &regions, "rally", false, 100).is_empty());
    }

    #[test]
    fn matches_across_chunk_boundaries_once() {
        let mut data = vec![0u8; (4 << 20) + 0x1000];
        let at = (4 << 20) - 3;
        data[at..at + 7].copy_from_slice(b"Leaderb");
        data[at + 7..at + 11].copy_from_slice(b"oard");
        let mut m = FakeMem::new();
        m.add(0x4000_0000, data, RegionKind::Private, true);
        let hits = search(&m, &m.regions(), "Leaderboard", false, 10);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].addr, 0x4000_0000 + at);
    }
}

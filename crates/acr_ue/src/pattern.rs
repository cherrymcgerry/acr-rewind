//! Byte patterns with `??` wildcards and resolution of pattern candidates to addresses.

use crate::mem::Memory;
use crate::sigs::{Candidate, CandidateKind};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pattern {
    bytes: Vec<Option<u8>>,
}

impl Pattern {
    /// Parses `"48 8B 05 ?? ?? ?? ??"`. `?` and `??` are wildcards.
    pub fn parse(s: &str) -> Result<Self, String> {
        let bytes = s
            .split_whitespace()
            .map(|tok| match tok {
                "?" | "??" => Ok(None),
                t if t.len() == 2 => u8::from_str_radix(t, 16).map(Some).map_err(|_| format!("bad byte '{t}'")),
                t => Err(format!("bad token '{t}'")),
            })
            .collect::<Result<Vec<_>, _>>()?;
        if bytes.is_empty() {
            return Err("empty pattern".into());
        }
        if bytes.iter().all(Option::is_none) {
            return Err("pattern is all wildcards".into());
        }
        Ok(Self { bytes })
    }

    pub fn from_bytes(bytes: &[Option<u8>]) -> Self {
        Self { bytes: bytes.to_vec() }
    }

    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    pub fn matches_at(&self, hay: &[u8], at: usize) -> bool {
        hay.get(at..at + self.bytes.len())
            .is_some_and(|w| w.iter().zip(&self.bytes).all(|(b, p)| p.is_none_or(|p| p == *b)))
    }

    /// Offsets of every match in `hay`.
    pub fn find_iter<'a>(&'a self, hay: &'a [u8]) -> impl Iterator<Item = usize> + 'a {
        let anchor = self.bytes.iter().position(Option::is_some).unwrap_or(0);
        let first = self.bytes[anchor];
        let last_start = hay.len().checked_sub(self.bytes.len());
        (0..last_start.map_or(0, |l| l + 1))
            .filter(move |&i| first.is_none_or(|f| hay[i + anchor] == f) && self.matches_at(hay, i))
    }

    pub fn find_nth(&self, hay: &[u8], n: usize) -> Option<usize> {
        self.find_iter(hay).nth(n)
    }
}

/// A scannable byte range mapped at `base` in the target address space.
#[derive(Clone, Copy, Debug)]
pub struct Region<'a> {
    pub base: usize,
    pub bytes: &'a [u8],
}

impl Region<'_> {
    pub fn contains(&self, addr: usize) -> bool {
        addr >= self.base && addr < self.base + self.bytes.len()
    }

    pub fn read_i32(&self, addr: usize) -> Option<i32> {
        let off = addr.checked_sub(self.base)?;
        let b = self.bytes.get(off..off + 4)?;
        Some(i32::from_le_bytes(b.try_into().ok()?))
    }

    pub fn slice_from(&self, addr: usize, len: usize) -> Option<&[u8]> {
        let off = addr.checked_sub(self.base)?;
        let end = off.checked_add(len)?.min(self.bytes.len());
        self.bytes.get(off..end)
    }
}

/// `target = instr + instr_len + rel32(instr + disp_offset)`.
pub fn rip_target(region: &Region<'_>, instr: usize, disp_offset: usize, instr_len: usize) -> Option<usize> {
    let rel = region.read_i32(instr + disp_offset)?;
    (instr + instr_len).checked_add_signed(rel as isize)
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ResolveError {
    #[error("pattern '{0}' not found")]
    NotFound(String),
    #[error("pattern '{0}' matched, but occurrence {1} does not exist")]
    Occurrence(String, usize),
    #[error("pattern '{0}' displacement out of range")]
    Displacement(String),
    #[error("pattern '{0}' deref at {1:#x} failed")]
    Deref(String, usize),
    #[error("invalid pattern '{0}': {1}")]
    BadPattern(String, String),
    #[error("no candidates configured")]
    NoCandidates,
}

/// Resolves one candidate against `regions` (searched in order; occurrences count across all).
pub fn resolve_candidate(c: &Candidate, regions: &[Region<'_>], mem: &dyn Memory) -> Result<usize, ResolveError> {
    let pat = Pattern::parse(&c.pattern).map_err(|e| ResolveError::BadPattern(c.pattern.clone(), e))?;
    let mut seen = 0usize;
    let mut hit = None;
    'outer: for r in regions {
        for off in pat.find_iter(r.bytes) {
            if seen == c.occurrence {
                hit = Some((r, r.base + off));
                break 'outer;
            }
            seen += 1;
        }
    }
    let Some((region, at)) = hit else {
        return Err(if seen == 0 {
            ResolveError::NotFound(c.pattern.clone())
        } else {
            ResolveError::Occurrence(c.pattern.clone(), c.occurrence)
        });
    };
    let target = match c.kind {
        CandidateKind::Direct => at,
        CandidateKind::RipRelative | CandidateKind::CallRel32 => rip_target(region, at, c.disp_offset, c.instr_len)
            .ok_or_else(|| ResolveError::Displacement(c.pattern.clone()))?,
    };
    let target =
        target.checked_add_signed(c.post_add as isize).ok_or_else(|| ResolveError::Displacement(c.pattern.clone()))?;
    if c.deref {
        mem.read_ptr(target).filter(|&p| p != 0).ok_or(ResolveError::Deref(c.pattern.clone(), target))
    } else {
        Ok(target)
    }
}

/// Tries every candidate; returns the first success, or every failure.
pub fn resolve_first(
    candidates: &[Candidate],
    regions: &[Region<'_>],
    mem: &dyn Memory,
) -> Result<usize, Vec<ResolveError>> {
    if candidates.is_empty() {
        return Err(vec![ResolveError::NoCandidates]);
    }
    let mut errors = Vec::new();
    for c in candidates {
        match resolve_candidate(c, regions, mem) {
            Ok(a) => return Ok(a),
            Err(e) => errors.push(e),
        }
    }
    Err(errors)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mem::SliceMemory;

    #[test]
    fn parse_patterns() {
        let p = Pattern::parse("48 8B 05 ?? ? ff").unwrap();
        assert_eq!(p.len(), 6);
        assert!(Pattern::parse("").is_err());
        assert!(Pattern::parse("?? ??").is_err());
        assert!(Pattern::parse("4").is_err());
        assert!(Pattern::parse("GG").is_err());
        assert!(Pattern::parse("123").is_err());
    }

    #[test]
    fn find_with_wildcards() {
        let hay = [0x00, 0x48, 0x8B, 0x05, 0x11, 0x22, 0x48, 0x8B, 0x05, 0x33];
        let p = Pattern::parse("48 8B 05 ??").unwrap();
        assert_eq!(p.find_iter(&hay).collect::<Vec<_>>(), vec![1, 6]);
        assert_eq!(p.find_nth(&hay, 1), Some(6));
        assert_eq!(p.find_nth(&hay, 2), None);
        let lead = Pattern::parse("?? 8B").unwrap();
        assert_eq!(lead.find_iter(&hay).collect::<Vec<_>>(), vec![1, 6]);
        let long = Pattern::parse("48 8B 05 33 44").unwrap();
        assert_eq!(long.find_iter(&hay).count(), 0);
        assert_eq!(p.find_iter(&[]).count(), 0);
    }

    fn cand(pattern: &str, kind: CandidateKind) -> Candidate {
        Candidate { pattern: pattern.into(), kind, disp_offset: 3, instr_len: 7, ..Default::default() }
    }

    #[test]
    fn rip_relative_resolution() {
        // mov rax, [rip+0x100] at base+0x10 -> target = base + 0x10 + 7 + 0x100
        let base = 0x1400_0000usize;
        let mut code = vec![0xCCu8; 0x40];
        code[0x10..0x17].copy_from_slice(&[0x48, 0x8B, 0x05, 0x00, 0x01, 0x00, 0x00]);
        let mem = SliceMemory::new(base, code.clone());
        let regions = [Region { base, bytes: &code }];
        let c = cand("48 8B 05 ?? ?? ?? ??", CandidateKind::RipRelative);
        assert_eq!(resolve_candidate(&c, &regions, &mem), Ok(base + 0x117));

        let mut c2 = c.clone();
        c2.post_add = -0x10;
        assert_eq!(resolve_candidate(&c2, &regions, &mem), Ok(base + 0x107));
    }

    #[test]
    fn negative_displacement_and_call() {
        let base = 0x1000usize;
        let mut code = vec![0x90u8; 0x100];
        // call rel32 at +0x80 with rel = -0x80 -> target = 0x1000 + 0x85 - 0x80 = 0x1005
        code[0x80] = 0xE8;
        code[0x81..0x85].copy_from_slice(&(-0x80i32).to_le_bytes());
        let mem = SliceMemory::new(base, code.clone());
        let regions = [Region { base, bytes: &code }];
        let c = Candidate {
            pattern: "E8 ?? ?? ?? ??".into(),
            kind: CandidateKind::CallRel32,
            disp_offset: 1,
            instr_len: 5,
            ..Default::default()
        };
        assert_eq!(resolve_candidate(&c, &regions, &mem), Ok(0x1005));
    }

    #[test]
    fn direct_occurrence_and_deref() {
        let base = 0x2000usize;
        let mut buf = vec![0u8; 0x100];
        buf[0x10..0x13].copy_from_slice(&[0xAA, 0xBB, 0xCC]);
        buf[0x40..0x43].copy_from_slice(&[0xAA, 0xBB, 0xCC]);
        buf[0x80..0x88].copy_from_slice(&0xDEAD_BEEFu64.to_le_bytes());
        let mem = SliceMemory::new(base, buf.clone());
        let regions = [Region { base, bytes: &buf }];
        let mut c = cand("AA BB CC", CandidateKind::Direct);
        assert_eq!(resolve_candidate(&c, &regions, &mem), Ok(0x2010));
        c.occurrence = 1;
        assert_eq!(resolve_candidate(&c, &regions, &mem), Ok(0x2040));
        c.occurrence = 2;
        assert!(matches!(resolve_candidate(&c, &regions, &mem), Err(ResolveError::Occurrence(_, 2))));
        // deref: post_add from match to the pointer slot
        let d = Candidate { occurrence: 0, post_add: 0x70, deref: true, ..cand("AA BB CC", CandidateKind::Direct) };
        assert_eq!(resolve_candidate(&d, &regions, &mem), Ok(0xDEAD_BEEF));
        let null = Candidate { post_add: 0x20, ..d };
        assert!(matches!(resolve_candidate(&null, &regions, &mem), Err(ResolveError::Deref(..))));
    }

    #[test]
    fn resolve_first_collects_errors() {
        let base = 0x10usize;
        let buf = vec![0u8; 16];
        let mem = SliceMemory::new(base, buf.clone());
        let regions = [Region { base, bytes: &buf }];
        let errs =
            resolve_first(&[cand("11 22", CandidateKind::Direct), cand("33", CandidateKind::Direct)], &regions, &mem)
                .unwrap_err();
        assert_eq!(errs.len(), 2);
        assert_eq!(resolve_first(&[], &regions, &mem).unwrap_err(), vec![ResolveError::NoCandidates]);
    }
}

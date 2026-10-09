//! Static pointer-chain discovery (`pointer-scan`) and re-validation (`pointer-verify`).
//!
//! Reverse breadth-first search: level 0 is the target; level `d` holds every aligned qword
//! whose value `v` satisfies `node - v <= max_offset` for some node of level `d-1`. Nodes that
//! lie in a root range (the game module's `.data`/`.rdata`, or a pawn object) end a chain.
//! Each level is one parallel pass over memory, so RAM use is bounded by `max_nodes`.
//!
//! Chains use the `[raw_offsets.sim_car]` convention: `addr = root; for off in chain { addr =
//! *(addr + off) }` yields the struct base, and the target is `base + field_offset`.

use crate::mem::{chunk_jobs, read_segments, MemRegion, ReadMem};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::time::Instant;

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Chain {
    /// Every step, starting with the root slot's offset from the root base.
    #[serde(with = "hex_vec")]
    pub chain: Vec<u64>,
    /// Offset of the target inside the resolved struct.
    #[serde(with = "hex")]
    pub field_offset: u64,
}

impl Chain {
    pub fn toml(&self, root: &str) -> String {
        let steps: Vec<String> = self.chain.iter().map(|o| format!("{o:#x}")).collect();
        format!(
            "chain_root = \"{root}\"\nchain = [{}]\n# target field offset inside the resolved struct: {:#x}",
            steps.join(", "),
            self.field_offset
        )
    }
}

mod hex {
    use serde::{Deserialize, Deserializer, Serializer};
    pub fn serialize<S: Serializer>(v: &u64, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&format!("{v:#x}"))
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<u64, D::Error> {
        let s = String::deserialize(d)?;
        crate::parse_u64(&s).map_err(serde::de::Error::custom)
    }
}

mod hex_vec {
    use serde::{Deserialize, Deserializer, Serializer};
    pub fn serialize<S: Serializer>(v: &[u64], s: S) -> Result<S::Ok, S::Error> {
        s.collect_seq(v.iter().map(|x| format!("{x:#x}")))
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u64>, D::Error> {
        let v = Vec::<String>::deserialize(d)?;
        v.iter().map(|s| crate::parse_u64(s).map_err(serde::de::Error::custom)).collect()
    }
}

pub struct PtrScanParams {
    pub target: usize,
    pub max_depth: usize,
    pub max_offset: usize,
    pub max_nodes: usize,
    pub max_results: usize,
    /// Address ranges `[start, end)` that terminate a chain.
    pub roots: Vec<(usize, usize)>,
    /// Chain step 0 is `root_slot - root_base`.
    pub root_base: usize,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct LevelStats {
    pub depth: usize,
    pub found: usize,
    pub roots: usize,
    pub kept: usize,
    pub truncated: bool,
    pub millis: u128,
}

#[derive(Clone, Copy, Debug)]
struct Node {
    addr: usize,
    parent: u32,
    off: u32,
}

fn in_roots(roots: &[(usize, usize)], a: usize) -> bool {
    roots.iter().any(|&(s, e)| a >= s && a < e)
}

/// Every aligned qword in `regions` pointing at or below a node by at most `max_offset`.
fn level_pass<R: ReadMem>(mem: &R, regions: &[MemRegion], nodes: &[Node], max_offset: usize) -> Vec<(usize, u32, u32)> {
    let mut sorted: Vec<(usize, u32)> = nodes.iter().enumerate().map(|(i, n)| (n.addr, i as u32)).collect();
    sorted.sort_unstable();
    let addrs: Vec<usize> = sorted.iter().map(|s| s.0).collect();
    let lo = addrs[0].saturating_sub(max_offset);
    let hi = *addrs.last().unwrap();
    let jobs = chunk_jobs(regions, 4 << 20, 0);
    jobs.par_iter()
        .map_init(Vec::new, |buf: &mut Vec<u8>, job| {
            buf.resize(job.read_len, 0);
            let mut out = Vec::new();
            for (s, e) in read_segments(mem, job.base, &mut buf[..job.read_len]) {
                let base = job.base + s;
                let first = (8 - base % 8) % 8;
                let seg = &buf[s..e];
                let mut i = first;
                while i + 8 <= seg.len() {
                    let v = u64::from_le_bytes(seg[i..i + 8].try_into().unwrap()) as usize;
                    if v >= lo && v <= hi {
                        let k = addrs.partition_point(|&t| t < v);
                        if k < addrs.len() && addrs[k] - v <= max_offset {
                            out.push((base + i, sorted[k].1, (addrs[k] - v) as u32));
                        }
                    }
                    i += 8;
                }
            }
            out
        })
        .flatten()
        .collect()
}

/// Runs the reverse search. Returns chains (shortest first) and per-level statistics.
pub fn pointer_scan<R: ReadMem>(mem: &R, regions: &[MemRegion], p: &PtrScanParams) -> (Vec<Chain>, Vec<LevelStats>) {
    let mut levels: Vec<Vec<Node>> = vec![vec![Node { addr: p.target, parent: u32::MAX, off: 0 }]];
    let mut visited: HashSet<usize> = HashSet::from([p.target]);
    let mut chains: Vec<Chain> = Vec::new();
    let mut seen: HashSet<Chain> = HashSet::new();
    let mut stats = Vec::new();
    for depth in 1..=p.max_depth {
        let t0 = Instant::now();
        let prev = levels.last().unwrap();
        if prev.is_empty() {
            break;
        }
        let found = level_pass(mem, regions, prev, p.max_offset);
        let n_found = found.len();
        // Dedupe by address (smallest offset wins), skip already visited addresses.
        let mut best: HashMap<usize, (u32, u32)> = HashMap::new();
        for (a, parent, off) in found {
            if visited.contains(&a) {
                continue;
            }
            best.entry(a)
                .and_modify(|e| {
                    if off < e.1 {
                        *e = (parent, off)
                    }
                })
                .or_insert((parent, off));
        }
        let mut next: Vec<Node> = Vec::new();
        let mut n_roots = 0;
        for (&addr, &(parent, off)) in &best {
            let node = Node { addr, parent, off };
            if in_roots(&p.roots, addr) {
                n_roots += 1;
                let c = build_chain(&levels, node, p.root_base);
                if seen.insert(c.clone()) {
                    chains.push(c);
                }
            } else {
                next.push(node);
            }
        }
        next.sort_by_key(|n| (n.off, n.addr));
        let truncated = next.len() > p.max_nodes;
        next.truncate(p.max_nodes);
        for n in &next {
            visited.insert(n.addr);
        }
        stats.push(LevelStats {
            depth,
            found: n_found,
            roots: n_roots,
            kept: next.len(),
            truncated,
            millis: t0.elapsed().as_millis(),
        });
        levels.push(next);
        if chains.len() >= p.max_results {
            break;
        }
    }
    chains.sort_by_key(|c| (c.chain.len(), c.chain.iter().skip(1).sum::<u64>() + c.field_offset, c.chain[0]));
    chains.truncate(p.max_results);
    (chains, stats)
}

/// `top` is a root node of level `levels.len()`. With path `N_k (top) .. N_1` (N_1 holds the
/// struct base), the chain is `[N_k - root_base, off(N_k), .., off(N_2)]` and the field offset
/// is `off(N_1)`.
fn build_chain(levels: &[Vec<Node>], top: Node, root_base: usize) -> Chain {
    let mut path = vec![top];
    let mut cur = top;
    for lvl in (2..=levels.len()).rev() {
        cur = levels[lvl - 1][cur.parent as usize];
        path.push(cur);
    }
    let mut chain = vec![(top.addr - root_base) as u64];
    chain.extend(path[..path.len() - 1].iter().map(|n| n.off as u64));
    Chain { chain, field_offset: path.last().unwrap().off as u64 }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mem::{FakeMem, RegionKind};
    use acr_ue::sim_car::resolve_chain;

    const IMG: usize = 0x1_4000_0000;

    fn world() -> (FakeMem, usize) {
        let mut m = FakeMem::new();
        m.add(IMG, vec![0u8; 0x3000], RegionKind::Image, true);
        m.add(0x2000_0000, vec![0u8; 0x4000], RegionKind::Private, true);
        m.add(0x3000_0000, vec![0u8; 0x1000], RegionKind::Private, true);
        let a = 0x2000_0000usize;
        let b = 0x2000_1000usize;
        let c = 0x3000_0000usize;
        let target = b + 0x40;
        // depth 3: IMG+0x1100 -> c ; c+0x20 -> a ; a+0x18 -> b ; target = b+0x40
        m.put_u64(IMG + 0x1100, c as u64);
        m.put_u64(c + 0x20, a as u64);
        m.put_u64(a + 0x18, b as u64);
        // depth 1: IMG+0x1200 -> b
        m.put_u64(IMG + 0x1200, b as u64);
        // pointer outside root range (heap) -> a, must not be reported as a chain
        m.put_u64(0x2000_3000, a as u64);
        // pointer too far below every node (offset > max) must be ignored
        m.put_u64(IMG + 0x1300, (target - 0x3000) as u64);
        (m, target)
    }

    #[test]
    fn finds_static_chains_and_they_resolve() {
        let (m, target) = world();
        let regions = m.regions();
        let p = PtrScanParams {
            target,
            max_depth: 4,
            max_offset: 0x1000,
            max_nodes: 1000,
            max_results: 50,
            roots: vec![(IMG + 0x1000, IMG + 0x2000)],
            root_base: IMG,
        };
        let (chains, stats) = pointer_scan(&m, &regions, &p);
        assert_eq!(chains[0], Chain { chain: vec![0x1200], field_offset: 0x40 });
        assert!(chains.contains(&Chain { chain: vec![0x1100, 0x20, 0x18], field_offset: 0x40 }), "{chains:?}");
        assert!(stats[0].roots >= 1);
        let mem = crate::mem::PageCache::new(&m, 64);
        for c in &chains {
            let base = resolve_chain(&mem, IMG, &c.chain).expect("chain resolves");
            assert_eq!(base + c.field_offset as usize, target, "{c:?}");
        }
        assert!(!chains.iter().any(|c| c.chain[0] == 0x1300));
        let toml = chains[0].toml("module");
        assert!(toml.contains("chain = [0x1200]"));
        let json = serde_json::to_string(&chains[1]).unwrap();
        let back: Chain = serde_json::from_str(&json).unwrap();
        assert_eq!(back, chains[1]);
    }

    #[test]
    fn depth_limit_and_node_cap() {
        let (m, target) = world();
        let regions = m.regions();
        let p = PtrScanParams {
            target,
            max_depth: 1,
            max_offset: 0x1000,
            max_nodes: 0,
            max_results: 50,
            roots: vec![(IMG + 0x1000, IMG + 0x2000)],
            root_base: IMG,
        };
        let (chains, stats) = pointer_scan(&m, &regions, &p);
        assert_eq!(chains, vec![Chain { chain: vec![0x1200], field_offset: 0x40 }]);
        assert!(stats[0].truncated);
    }

    #[cfg(windows)]
    #[test]
    fn finds_chain_in_own_process() {
        use crate::mem::{ReadMem, RegionFilter};
        use std::sync::atomic::{AtomicUsize, Ordering};
        static ROOT: AtomicUsize = AtomicUsize::new(0);
        let car: Box<[u64; 32]> = Box::new([0x5EED_5EED; 32]);
        let mut holder: Box<[usize; 8]> = Box::new([0; 8]);
        holder[3] = car.as_ptr() as usize;
        ROOT.store(holder.as_ptr() as usize, Ordering::SeqCst);
        let target = car.as_ptr() as usize + 0x50;
        let me = crate::process::RemoteProcess::open(std::process::id()).unwrap();
        let (base, _, _) = acr_ue::pe::find_module("definitely-not-loaded.dll").unwrap();
        let module = acr_ue::pe::ModuleInfo::parse(&acr_ue::mem::ProcessMemory, base).unwrap();
        let roots: Vec<(usize, usize)> = module
            .sections
            .iter()
            .filter(|s| s.name != ".text")
            .map(|s| (base + s.rva, base + s.rva + s.size))
            .collect();
        let root_addr = &ROOT as *const AtomicUsize as usize;
        assert!(in_roots(&roots, root_addr), "static lives in a data section");
        let regions = RegionFilter::DATA.apply(&me.regions());
        let p = PtrScanParams {
            target,
            max_depth: 2,
            max_offset: 0x100,
            max_nodes: 100_000,
            max_results: 1000,
            roots,
            root_base: base,
        };
        let (chains, _) = pointer_scan(&me, &regions, &p);
        let want = Chain { chain: vec![(root_addr - base) as u64, 0x18], field_offset: 0x50 };
        assert!(chains.contains(&want), "{chains:?}");
        std::hint::black_box((&car, &holder));
    }
}

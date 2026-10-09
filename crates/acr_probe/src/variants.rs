//! Coordinate-convention variants between shared memory and game memory.
//!
//! AC shared memory is metres / m·s⁻¹ in the Kunos frame (Y up); the game may store the same
//! quantity with permuted / flipped axes and in other units (UE: Z up, centimetres). A
//! [`Variant`] maps shm values to the expected in-memory values:
//! `mem[k] = scale * signs[k] * shm[perm[k]]`.

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Variant {
    pub perm: Vec<u8>,
    pub signs: Vec<i8>,
    pub scale: f64,
}

const PERMS: [[u8; 3]; 6] = [[0, 1, 2], [0, 2, 1], [1, 0, 2], [1, 2, 0], [2, 0, 1], [2, 1, 0]];

impl Variant {
    pub fn identity(arity: usize, scale: f64) -> Self {
        Self { perm: (0..arity as u8).collect(), signs: vec![1; arity], scale }
    }

    pub fn arity(&self) -> usize {
        self.perm.len()
    }

    pub fn apply(&self, shm: &[f64]) -> Vec<f64> {
        (0..self.arity()).map(|k| self.scale * f64::from(self.signs[k]) * shm[self.perm[k] as usize]).collect()
    }

    /// Inverse of [`apply`](Self::apply): memory values back to shm space.
    pub fn invert(&self, mem: &[f64]) -> Vec<f64> {
        let mut out = vec![0.0; self.arity()];
        for k in 0..self.arity() {
            out[self.perm[k] as usize] = mem[k] / (self.scale * f64::from(self.signs[k]));
        }
        out
    }

    /// e.g. `mem = 100 * (+x, -z, +y)`.
    pub fn describe(&self) -> String {
        const AXES: [&str; 3] = ["x", "y", "z"];
        let parts: Vec<String> = (0..self.arity())
            .map(|k| {
                let s = if self.signs[k] < 0 { '-' } else { '+' };
                if self.arity() == 1 {
                    format!("{s}v")
                } else {
                    format!("{s}{}", AXES[self.perm[k] as usize])
                }
            })
            .collect();
        format!("mem = {} * ({})", fmt_scale(self.scale), parts.join(", "))
    }
}

fn fmt_scale(s: f64) -> String {
    if (s - s.round()).abs() < 1e-12 {
        format!("{}", s as i64)
    } else {
        format!("{s:.6}")
    }
}

/// Every axis permutation × sign combination × scale (or identity orientation only).
pub fn vec3_variants(scales: &[f64], permute: bool) -> Vec<Variant> {
    let mut out = Vec::new();
    for &scale in scales {
        let perms: &[[u8; 3]] = if permute { &PERMS } else { &PERMS[..1] };
        for p in perms {
            let sign_sets: Vec<[i8; 3]> = if permute {
                (0..8u8).map(|m| std::array::from_fn(|k| if m >> k & 1 == 1 { -1 } else { 1 })).collect()
            } else {
                vec![[1, 1, 1]]
            };
            for s in sign_sets {
                out.push(Variant { perm: p.to_vec(), signs: s.to_vec(), scale });
            }
        }
    }
    out
}

pub fn scalar_variants(scales: &[f64], signed: bool) -> Vec<Variant> {
    let mut out = Vec::new();
    for &scale in scales {
        out.push(Variant::identity(1, scale));
        if signed {
            out.push(Variant { perm: vec![0], signs: vec![-1], scale });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts() {
        assert_eq!(vec3_variants(&[1.0, 100.0], true).len(), 96);
        assert_eq!(vec3_variants(&[1.0], false).len(), 1);
        assert_eq!(scalar_variants(&[1.0, 2.0], true).len(), 4);
        let all = vec3_variants(&[1.0], true);
        for (i, a) in all.iter().enumerate() {
            assert!(all[i + 1..].iter().all(|b| b != a), "duplicate variant");
        }
    }

    #[test]
    fn apply_invert_roundtrip() {
        let v = Variant { perm: vec![2, 0, 1], signs: vec![1, -1, 1], scale: 100.0 };
        let shm = [1.5, -2.0, 3.25];
        let mem = v.apply(&shm);
        assert_eq!(mem, vec![325.0, -150.0, -200.0]);
        assert_eq!(v.invert(&mem), shm.to_vec());
        assert_eq!(v.describe(), "mem = 100 * (+z, -x, +y)");
        let s = Variant::identity(1, std::f64::consts::TAU / 60.0);
        assert!(s.describe().starts_with("mem = 0.104720"));
    }
}

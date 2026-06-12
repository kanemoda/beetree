//! Deterministic workload machinery for the M3.2 benchmarks: a tiny
//! PRNG, key distributions (uniform, sequential, YCSB-style scrambled
//! Zipfian), and named op mixes. Everything is seeded — identical seed,
//! identical op sequence — so every benchmark is reproducible and
//! engine-level I/O is comparable across runs (SPEC "Observability").
//!
//! This module is a leaf: nothing in the engines depends on it.

/// SplitMix64: the classic 64-bit mixer (public-domain constants), as a
/// dependency-free deterministic PRNG.
#[derive(Debug, Clone)]
pub struct SplitMix64(pub u64);

impl SplitMix64 {
    /// The next pseudo-random u64.
    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// A uniform draw in `[0, n)`.
    pub fn below(&mut self, n: u64) -> u64 {
        self.next_u64() % n
    }

    /// A uniform f64 in `[0, 1)`.
    pub fn unit_f64(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }
}

/// One stateless hash step (the SplitMix64 mixer), used to scramble
/// Zipfian ranks over the keyspace.
fn mix64(mut z: u64) -> u64 {
    z = z.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// YCSB-style scrambled Zipfian over `[0, keyspace)` with θ = 0.99: rank
/// popularity follows the Zipfian law, and ranks are scrambled across
/// the keyspace by a stateless hash so the hot set is not contiguous.
#[derive(Debug, Clone)]
pub struct ScrambledZipfian {
    keyspace: u64,
    theta: f64,
    alpha: f64,
    zetan: f64,
    eta: f64,
    rng: SplitMix64,
}

impl ScrambledZipfian {
    /// θ = 0.99, the YCSB default.
    pub const THETA: f64 = 0.99;

    /// A generator over `[0, keyspace)`, seeded.
    pub fn new(keyspace: u64, seed: u64) -> ScrambledZipfian {
        assert!(keyspace >= 2, "a Zipfian needs at least two keys");
        let theta = Self::THETA;
        let zetan = zeta(keyspace, theta);
        let zeta2 = zeta(2, theta);
        ScrambledZipfian {
            keyspace,
            theta,
            alpha: 1.0 / (1.0 - theta),
            zetan,
            eta: (1.0 - (2.0 / keyspace as f64).powf(1.0 - theta)) / (1.0 - zeta2 / zetan),
            rng: SplitMix64(seed),
        }
    }

    /// The analytical probability of the hottest rank (rank 0): 1/ζ(n,θ).
    /// Exposed so the validation test can derive its expected band.
    pub fn hottest_probability(&self) -> f64 {
        1.0 / self.zetan
    }

    /// The next key index in `[0, keyspace)`.
    pub fn next_key(&mut self) -> u64 {
        let u = self.rng.unit_f64();
        let uz = u * self.zetan;
        let rank = if uz < 1.0 {
            0
        } else if uz < 1.0 + 0.5f64.powf(self.theta) {
            1
        } else {
            ((self.keyspace as f64) * (self.eta * u - self.eta + 1.0).powf(self.alpha)) as u64
        };
        let rank = rank.min(self.keyspace - 1);
        // Scramble: rank → keyspace slot by stateless hash (collisions
        // fold a cold key onto a hotter slot; acceptable for a benchmark
        // generator and standard in YCSB).
        mix64(rank) % self.keyspace
    }
}

/// ζ(n, θ) = Σ_{i=1..n} 1/i^θ.
fn zeta(n: u64, theta: f64) -> f64 {
    (1..=n).map(|i| 1.0 / (i as f64).powf(theta)).sum()
}

/// How point-op keys are drawn from the keyspace.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyDist {
    /// Uniform over the keyspace.
    Uniform,
    /// 0, 1, 2, … (wrapping at the keyspace).
    Sequential,
    /// YCSB-style scrambled Zipfian, θ = 0.99.
    Zipfian,
}

/// The named, fixed op mixes of the M3.2 suite (SPEC "Observability").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mix {
    /// 100% insert.
    Load,
    /// 100% point read.
    PointRead,
    /// 50% read / 50% update (YCSB A).
    YcsbA,
    /// 95% read / 5% update (YCSB B).
    YcsbB,
    /// 100% read (YCSB C).
    YcsbC,
    /// 90% blind `Add` upsert / 10% read.
    UpsertHeavy,
    /// 95% point read / 5% short scans of 10–100 keys.
    ScanMix,
}

impl Mix {
    /// The mix's stable CLI/CSV name.
    pub fn name(self) -> &'static str {
        match self {
            Mix::Load => "load",
            Mix::PointRead => "point-read",
            Mix::YcsbA => "ycsb-a",
            Mix::YcsbB => "ycsb-b",
            Mix::YcsbC => "ycsb-c",
            Mix::UpsertHeavy => "upsert-heavy",
            Mix::ScanMix => "scan-mix",
        }
    }

    /// Every named mix, in suite order.
    pub fn all() -> [Mix; 7] {
        [
            Mix::Load,
            Mix::PointRead,
            Mix::YcsbA,
            Mix::YcsbB,
            Mix::YcsbC,
            Mix::UpsertHeavy,
            Mix::ScanMix,
        ]
    }

    /// Parse a CLI name.
    pub fn parse(name: &str) -> Option<Mix> {
        Mix::all().into_iter().find(|m| m.name() == name)
    }
}

/// One benchmark operation, in key-index space (the harness encodes
/// indices to byte keys).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkOp {
    /// Insert/overwrite `key` with a value derived from the op counter.
    Insert(u64),
    /// Point read.
    Read(u64),
    /// Blind `Add(delta)`.
    UpsertAdd(u64, i64),
    /// Scan `count` keys starting at index `start`.
    Scan(u64, u64),
}

/// A seeded stream of `n` ops for `mix` over `[0, keyspace)`, point keys
/// drawn per `dist`. Identical arguments ⇒ identical sequence.
pub struct OpStream {
    mix: Mix,
    dist: KeyDist,
    keyspace: u64,
    remaining: u64,
    sequential_next: u64,
    rng: SplitMix64,
    zipf: Option<ScrambledZipfian>,
}

impl OpStream {
    /// Build the stream; `seed` controls every random choice.
    pub fn new(mix: Mix, dist: KeyDist, keyspace: u64, n: u64, seed: u64) -> OpStream {
        OpStream {
            mix,
            dist,
            keyspace,
            remaining: n,
            sequential_next: 0,
            rng: SplitMix64(seed),
            zipf: match dist {
                KeyDist::Zipfian => Some(ScrambledZipfian::new(keyspace, seed ^ 0x5EED)),
                _ => None,
            },
        }
    }

    fn key(&mut self) -> u64 {
        match self.dist {
            KeyDist::Uniform => self.rng.below(self.keyspace),
            KeyDist::Sequential => {
                let k = self.sequential_next;
                self.sequential_next = (self.sequential_next + 1) % self.keyspace;
                k
            }
            KeyDist::Zipfian => self.zipf.as_mut().expect("zipfian generator").next_key(),
        }
    }
}

impl Iterator for OpStream {
    type Item = WorkOp;

    fn next(&mut self) -> Option<WorkOp> {
        if self.remaining == 0 {
            return None;
        }
        self.remaining -= 1;
        let roll = self.rng.below(100);
        let key = self.key();
        Some(match self.mix {
            Mix::Load => WorkOp::Insert(key),
            Mix::PointRead | Mix::YcsbC => WorkOp::Read(key),
            Mix::YcsbA => {
                if roll < 50 {
                    WorkOp::Read(key)
                } else {
                    WorkOp::Insert(key)
                }
            }
            Mix::YcsbB => {
                if roll < 95 {
                    WorkOp::Read(key)
                } else {
                    WorkOp::Insert(key)
                }
            }
            Mix::UpsertHeavy => {
                if roll < 90 {
                    WorkOp::UpsertAdd(key, (self.rng.below(100) + 1) as i64)
                } else {
                    WorkOp::Read(key)
                }
            }
            Mix::ScanMix => {
                if roll < 95 {
                    WorkOp::Read(key)
                } else {
                    WorkOp::Scan(key, 10 + self.rng.below(91))
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Identical seed ⇒ identical op sequence, for every mix and dist.
    #[test]
    fn streams_are_deterministic_under_seed() {
        for mix in Mix::all() {
            for dist in [KeyDist::Uniform, KeyDist::Sequential, KeyDist::Zipfian] {
                let a: Vec<WorkOp> = OpStream::new(mix, dist, 1000, 500, 42).collect();
                let b: Vec<WorkOp> = OpStream::new(mix, dist, 1000, 500, 42).collect();
                let c: Vec<WorkOp> = OpStream::new(mix, dist, 1000, 500, 43).collect();
                assert_eq!(a, b, "{mix:?}/{dist:?} must be seed-deterministic");
                // Combos that consume randomness must actually vary by
                // seed; sequential keys with a fixed op kind legitimately
                // do not (the stream ignores its rng there).
                let randomness_free = dist == KeyDist::Sequential
                    && matches!(mix, Mix::Load | Mix::PointRead | Mix::YcsbC);
                if randomness_free {
                    assert_eq!(a, c, "{mix:?}/{dist:?} is fully determined by its inputs");
                } else {
                    assert_ne!(a, c, "{mix:?}/{dist:?} must actually vary by seed");
                }
            }
        }
    }

    /// The Zipfian's hottest key lands inside the analytical band for
    /// θ = 0.99: with 1M draws over a 10,000-key space, the most frequent
    /// key's share must be within ±10% of 1/ζ(n, θ) (sampling error at
    /// this volume is far below that; the seed is fixed, so no flake).
    #[test]
    fn zipfian_hottest_key_is_in_the_expected_band() {
        const KEYSPACE: u64 = 10_000;
        const DRAWS: usize = 1_000_000;
        let mut zipf = ScrambledZipfian::new(KEYSPACE, 7);
        let expected = zipf.hottest_probability();
        let mut counts = vec![0u32; KEYSPACE as usize];
        for _ in 0..DRAWS {
            counts[zipf.next_key() as usize] += 1;
        }
        let hottest = *counts.iter().max().expect("non-empty") as f64 / DRAWS as f64;
        assert!(
            (expected * 0.9..=expected * 1.1).contains(&hottest),
            "hottest key share {hottest:.4} outside ±10% of analytical {expected:.4}"
        );
    }

    /// Zipfian draws are deterministic under the seed.
    #[test]
    fn zipfian_is_deterministic_under_seed() {
        let a: Vec<u64> = {
            let mut g = ScrambledZipfian::new(1000, 9);
            (0..200).map(|_| g.next_key()).collect()
        };
        let b: Vec<u64> = {
            let mut g = ScrambledZipfian::new(1000, 9);
            (0..200).map(|_| g.next_key()).collect()
        };
        assert_eq!(a, b);
    }
}

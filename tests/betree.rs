// Integration tests for the M0.2 BeTree.
//
// The frozen generic harness (tests/harness.rs, byte-identical since the
// Step-0 API freeze) is mounted as a module and instantiated for BeTree —
// that one `instantiate_harness!` line is the M0.2 acceptance gate.

#[macro_use]
#[path = "harness.rs"]
mod harness;

use beetree::{BeTree, KvEngine, Params, TraceEvent};
use harness::*;
use proptest::prelude::*;

instantiate_harness!(betree_engine, beetree::BeTree);

/// Dependency-free deterministic PRNG (xorshift64) for random workloads.
struct XorShift(u64);

impl XorShift {
    fn new(seed: u64) -> Self {
        XorShift(seed.max(1))
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    fn byte(&mut self) -> u8 {
        (self.next_u64() >> 32) as u8
    }
}

fn flush_decision_count(tree: &BeTree) -> usize {
    tree.trace()
        .iter()
        .filter(|event| matches!(event, TraceEvent::FlushDecision { .. }))
        .count()
}

/// 2000 sequential single-byte-key inserts under default params must build
/// a genuinely deep tree and exercise the flush path — proof that the
/// harness runs against a real Bε-tree, not a leaf that never splits.
#[test]
fn structural_deep_tree() {
    let mut tree = BeTree::new(Params::default());
    for i in 0..2000u32 {
        tree.insert(
            vec![(i % 256) as u8],
            vec![(i / 256) as u8, (i % 256) as u8],
        );
    }
    tree.check_invariants()
        .expect("invariants green after 2000 inserts");
    assert!(
        tree.height() >= 3,
        "expected height >= 3, got {}",
        tree.height()
    );
    let flushes = flush_decision_count(&tree);
    assert!(
        flushes > 1,
        "expected > 1 FlushDecision events, got {flushes}"
    );
}

/// A 1000-op random workload checked against a shadow oracle (interleaved
/// gets like P1, full-domain sweep like P2, invariants after every op like
/// P3) under parameter extremes the default harness never sees.
#[test]
fn param_matrix_random_oracle() {
    let matrix = [
        Params {
            fanout: 2,
            buffer_capacity: 8,
            leaf_capacity: 1,
        },
        Params {
            fanout: 16,
            buffer_capacity: 1,
            leaf_capacity: 4,
        },
        Params {
            fanout: 4,
            buffer_capacity: 8,
            leaf_capacity: 8,
        },
    ];
    for (i, params) in matrix.into_iter().enumerate() {
        let mut rng = XorShift::new(0x5eed + i as u64);
        let mut engine = BeTree::new(params);
        let mut oracle = std::collections::BTreeMap::new();
        for op in 0..1000 {
            let key = vec![rng.byte()];
            let len = (rng.next_u64() % 9) as usize;
            let value: Vec<u8> = (0..len).map(|_| rng.byte()).collect();
            engine.insert(key.clone(), value.clone());
            oracle.insert(key.clone(), value);
            assert_eq!(
                engine.get(&key),
                oracle.get(&key).cloned(),
                "interleaved get diverged at op {op} under {params:?}"
            );
            engine
                .check_invariants()
                .unwrap_or_else(|violation| panic!("op {op} under {params:?}: {violation}"));
        }
        for key in full_domain() {
            assert_eq!(
                engine.get(&key),
                oracle.get(&key).cloned(),
                "final sweep diverged under {params:?}"
            );
        }
    }
}

/// Pivot-convention edges, observably: pivots always equal real inserted
/// keys (SPEC), so overwriting EVERY key after the tree is deep exercises
/// insert- and get-routing at `key == pivot` on every boundary, plus the
/// smallest ([0]) and largest ([255]) keys of the domain. A wrong-side
/// route would put a message outside its node's owned range, which the
/// independent I1 bounds check flags after every op.
#[test]
fn pivot_edges_full_domain_overwrite() {
    let mut tree = BeTree::new(Params::default());
    for b in 0..=u8::MAX {
        tree.insert(vec![b], vec![b, 0]);
    }
    tree.check_invariants()
        .expect("invariants green after first pass");
    assert!(
        tree.height() >= 2,
        "256 distinct keys must split the root leaf"
    );

    for b in 0..=u8::MAX {
        tree.insert(vec![b], vec![b, 1]);
        tree.check_invariants()
            .expect("invariants green during overwrite pass");
    }
    for b in 0..=u8::MAX {
        assert_eq!(tree.get(&[b]), Some(vec![b, 1]), "stale read for key {b}");
    }
    assert_eq!(tree.get(&[0]), Some(vec![0, 1]));
    assert_eq!(tree.get(&[255]), Some(vec![255, 1]));
}

/// Regression (M0.2 adversarial review): F=2 is legal but degenerate —
/// with split-only rebalancing every internal split of a 3-child node must
/// produce a fanout-1 piece, so strictly ascending inserts grow the tree
/// Θ(n) tall (height ≈ n/2 at F=2/B=1/L=1). The original recursive flush
/// cascade and invariant walk recursed once per level and aborted the
/// process (stack overflow, SIGABRT) around 2,500 sequential inserts.
/// The engine must SURVIVE legal-but-degenerate parameters; performance
/// under them is explicitly not promised (SPEC, "Structure parameters").
#[test]
fn regression_f2_ascending_inserts_survive_linear_height() {
    let params = Params {
        fanout: 2,
        buffer_capacity: 1,
        leaf_capacity: 1,
    };
    let mut tree = BeTree::new(params);
    for i in 0..3000u32 {
        tree.insert(i.to_be_bytes().to_vec(), vec![1]);
        // Checking every op is quadratic on a tree whose node count is
        // itself quadratic; every 64 ops still pins the invariants all the
        // way up the degenerate spine. The stack-depth guard lives in the
        // insert cascade itself (explicit frame stack, ADR-0005) and is not
        // weakened by checking less often.
        if i % 64 == 63 {
            tree.check_invariants()
                .expect("invariants green during the degenerate build");
        }
    }
    tree.check_invariants()
        .expect("invariants green on the degenerate tall tree");
    assert!(
        tree.height() > 1000,
        "the F=2 ascending workload must produce a degenerate tall tree (got height {})",
        tree.height()
    );
    assert_eq!(tree.get(&0u32.to_be_bytes()), Some(vec![1]));
    assert_eq!(tree.get(&1499u32.to_be_bytes()), Some(vec![1]));
    assert_eq!(tree.get(&2999u32.to_be_bytes()), Some(vec![1]));
}

/// Reporting helper, not a correctness gate:
/// `cargo test --test betree stats_10k -- --ignored --nocapture`
#[test]
#[ignore = "reporting helper; run explicitly with --ignored --nocapture"]
fn stats_10k_random_inserts() {
    let mut rng = XorShift::new(0xbee7ee);
    let mut tree = BeTree::new(Params::default());
    for _ in 0..10_000 {
        let key = vec![rng.byte(), rng.byte()];
        let value = vec![rng.byte(), rng.byte(), rng.byte(), rng.byte()];
        tree.insert(key, value);
    }
    tree.check_invariants()
        .expect("invariants green after 10k inserts");
    println!(
        "10k random 2-byte-key inserts under default params: height={} node_count={} flush_decisions={}",
        tree.height(),
        tree.node_count(),
        flush_decision_count(&tree),
    );
}

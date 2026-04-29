//! Microbenchmark for `Ring::preference_list`.
//!
//! Target: < 1µs for a 3-node ring with 256 vnodes/node.

use criterion::{Criterion, black_box, criterion_group, criterion_main};
use grumpydb_ring::{Ring, RingConfig, RoutingKey};

fn build_ring(n: usize) -> Ring<String> {
    let mut r = Ring::new(RingConfig::default());
    for i in 0..n {
        r.add_node(format!("node-{i}"));
    }
    r
}

fn bench_preference_list(c: &mut Criterion) {
    let mut group = c.benchmark_group("preference_list");

    for &nodes in &[3usize, 10, 50] {
        let ring = build_ring(nodes);
        let key = RoutingKey {
            database: "users",
            collection: "profiles",
            key_bytes: b"alice@example.com",
        };
        group.bench_function(format!("{nodes}-nodes_n=1"), |b| {
            b.iter(|| {
                let pl = ring.preference_list(black_box(&key), black_box(1));
                black_box(pl);
            });
        });
        group.bench_function(format!("{nodes}-nodes_n=3"), |b| {
            b.iter(|| {
                let pl = ring.preference_list(black_box(&key), black_box(3));
                black_box(pl);
            });
        });
    }

    group.finish();
}

criterion_group!(benches, bench_preference_list);
criterion_main!(benches);

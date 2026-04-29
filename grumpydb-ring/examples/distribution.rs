use grumpydb_ring::{Ring, RingConfig, RoutingKey};
use std::collections::HashMap;

fn main() {
    let nodes = ["n1", "n2", "n3", "n4", "n5", "n6", "n7", "n8", "n9", "n10"];
    let mut r: Ring<&str> = Ring::new(RingConfig::default());
    for n in &nodes {
        r.add_node(*n);
    }
    let mut counts: HashMap<&str, u64> = HashMap::new();
    let n = 1_000_000u64;
    for i in 0..n {
        let s = format!("key-{i}");
        let k = RoutingKey {
            database: "db",
            collection: "coll",
            key_bytes: s.as_bytes(),
        };
        let pl = r.preference_list(&k, 1);
        *counts.entry(pl[0]).or_insert(0) += 1;
    }
    let mean = n as f64 / nodes.len() as f64;
    let mut entries: Vec<_> = counts.into_iter().collect();
    entries.sort();
    for (n, c) in entries {
        let dev = (c as f64 - mean) / mean * 100.0;
        println!("{n:>4}: {c:>7} ({dev:+.2}%)");
    }
}

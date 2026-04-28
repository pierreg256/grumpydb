//! Criterion benchmarks for the GrumpyDB wire protocol.
//!
//! Measures parser throughput on common command shapes and serializer throughput
//! on representative responses. Run with
//! `cargo bench --bench protocol` (full mode) or
//! `cargo bench --bench protocol -- --quick` (smoke run).

use criterion::{Criterion, Throughput, black_box, criterion_group, criterion_main};
use grumpydb_protocol::{Response, parse_command};

fn bench_parse_simple_commands(c: &mut Criterion) {
    // Mix of small, frequently-used commands. The parser is line-oriented and
    // case-insensitive on the verb.
    let commands: Vec<&str> = vec![
        "PING",
        "GET users 550e8400-e29b-41d4-a716-446655440000",
        "INSERT users 550e8400-e29b-41d4-a716-446655440000 {\"name\":\"alice\"}",
        "GET tasks 550e8400-e29b-41d4-a716-446655440001",
        "PING",
        "GET tasks 550e8400-e29b-41d4-a716-446655440002",
        "DELETE tasks 550e8400-e29b-41d4-a716-446655440003",
        "COUNT tasks",
        "FLUSH",
        "PING",
    ];
    // Repeat to reach 1 000 commands per iteration.
    let mut batch: Vec<&str> = Vec::with_capacity(1_000);
    for _ in 0..(1_000 / commands.len()) {
        batch.extend_from_slice(&commands);
    }
    while batch.len() < 1_000 {
        batch.push("PING");
    }

    let mut group = c.benchmark_group("parse_simple_commands");
    group.throughput(Throughput::Elements(1_000));
    group.bench_function("1000_commands", |b| {
        b.iter(|| {
            for line in &batch {
                let cmd = parse_command(black_box(line)).expect("parse");
                black_box(cmd);
            }
        });
    });
    group.finish();
}

fn bench_parse_complex_command(c: &mut Criterion) {
    // INSERT with a ~1 KB JSON value.
    let payload = format!(
        "{{\"name\":\"alice\",\"bio\":\"{}\",\"age\":42,\"active\":true}}",
        "x".repeat(950)
    );
    let line = format!("INSERT users 550e8400-e29b-41d4-a716-446655440000 {payload}");
    assert!(line.len() >= 1_000, "expected ~1KB command line");

    let mut group = c.benchmark_group("parse_complex_command");
    group.throughput(Throughput::Bytes(line.len() as u64));
    group.bench_function("1kb_insert", |b| {
        b.iter(|| {
            let cmd = parse_command(black_box(&line)).expect("parse");
            black_box(cmd);
        });
    });
    group.finish();
}

fn bench_serialize_response(c: &mut Criterion) {
    // Response::Array containing 100 Bulk(Some(...)) entries — typical SCAN
    // result shape.
    let response = Response::Array(
        (0..100)
            .map(|i| Response::Bulk(Some(format!("doc_{i}_payload"))))
            .collect(),
    );

    let mut group = c.benchmark_group("serialize_response");
    group.throughput(Throughput::Elements(100));
    group.bench_function("array_100_bulks", |b| {
        b.iter(|| {
            let s = black_box(&response).serialize();
            black_box(s);
        });
    });
    group.finish();
}

criterion_group!(
    protocol_benches,
    bench_parse_simple_commands,
    bench_parse_complex_command,
    bench_serialize_response,
);
criterion_main!(protocol_benches);

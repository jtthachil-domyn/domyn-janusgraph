use criterion::{Criterion, criterion_group, criterion_main};
use nexus_bench::benchmarks;
use nexus_bench::graph_builder::build_bench_graph;

fn bench_b5_one_hop(c: &mut Criterion) {
    let bg = build_bench_graph(100, 5, 2, 10, 20);
    c.bench_function("B5: 1-hop traversal (100 companies)", |b| {
        b.iter(|| benchmarks::b5_one_hop_traversal(&bg))
    });
}

fn bench_b6_two_hop(c: &mut Criterion) {
    let bg = build_bench_graph(100, 5, 2, 10, 20);
    c.bench_function("B6: 2-hop traversal (100 companies)", |b| {
        b.iter(|| benchmarks::b6_two_hop_traversal(&bg))
    });
}

fn bench_b7_variable_length(c: &mut Criterion) {
    let bg = build_bench_graph(100, 5, 2, 10, 20);
    c.bench_function("B7: Variable-length path (3 hops)", |b| {
        b.iter(|| benchmarks::b7_variable_length_path(&bg))
    });
}

fn bench_b8_aggregation(c: &mut Criterion) {
    let bg = build_bench_graph(100, 5, 2, 10, 20);
    c.bench_function("B8: Aggregation (count by label)", |b| {
        b.iter(|| benchmarks::b8_aggregation(&bg))
    });
}

fn bench_b3_point_lookup(c: &mut Criterion) {
    let bg = build_bench_graph(100, 5, 2, 10, 20);
    c.bench_function("B3: Point lookup", |b| {
        b.iter(|| benchmarks::b3_point_lookup(&bg))
    });
}

criterion_group!(
    benches,
    bench_b3_point_lookup,
    bench_b5_one_hop,
    bench_b6_two_hop,
    bench_b7_variable_length,
    bench_b8_aggregation
);
criterion_main!(benches);

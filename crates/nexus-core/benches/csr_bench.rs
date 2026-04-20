use criterion::{Criterion, criterion_group, criterion_main};
use nexus_core::csr::CsrBuilder;

fn bench_csr_neighbor_lookup(c: &mut Criterion) {
    let n = 10_000;
    let mut builder = CsrBuilder::new(n);
    for i in 0..n as u64 {
        for j in 1..=5 {
            builder.add_edge(i, (i + j) % n as u64, i * 5 + j);
        }
    }
    let csr = builder.build();

    c.bench_function("csr_neighbor_lookup", |b| {
        b.iter(|| {
            for i in 0..100u64 {
                std::hint::black_box(csr.neighbors_of(i));
            }
        })
    });
}

criterion_group!(benches, bench_csr_neighbor_lookup);
criterion_main!(benches);

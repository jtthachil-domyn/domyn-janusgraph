use criterion::{Criterion, criterion_group, criterion_main};
use nexus_algebra::semiring::BooleanSemiring;
use nexus_algebra::sparse_vector::SparseVector;
use nexus_algebra::spmv;
use nexus_core::csr::CsrBuilder;

fn bench_spmv_reachability(c: &mut Criterion) {
    let n = 10_000;
    let mut builder = CsrBuilder::new(n);
    for i in 0..n as u64 {
        for j in 1..=5 {
            builder.add_edge(i, (i + j) % n as u64, i * 5 + j);
        }
    }
    let csr = builder.build();
    let sr = BooleanSemiring;
    let input = SparseVector::singleton(n, 0, true);

    c.bench_function("spmv_1hop_10k_vertices", |b| {
        b.iter(|| {
            std::hint::black_box(spmv::spmv(&csr, &input, &sr));
        })
    });
}

criterion_group!(benches, bench_spmv_reachability);
criterion_main!(benches);

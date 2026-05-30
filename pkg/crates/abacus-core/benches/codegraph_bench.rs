// placeholder for codegraph benchmarks
use criterion::{criterion_group, criterion_main, Criterion};

fn codegraph_bench(_c: &mut Criterion) {}

criterion_group!(benches, codegraph_bench);
criterion_main!(benches);

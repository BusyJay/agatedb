use agatedb::format::{get_ts, key_with_ts};
use criterion::{criterion_group, criterion_main, Criterion};

#[cfg(not(target_env = "msvc"))]
use tikv_jemallocator::Jemalloc;

#[cfg(not(target_env = "msvc"))]
#[global_allocator]
static GLOBAL: Jemalloc = Jemalloc;

fn criterion_benchmark(c: &mut Criterion) {
    c.bench_function("format make key with ts", |b| {
        b.iter(|| key_with_ts("aaabbbcccddd", 233))
    });
    let key = key_with_ts("aaabbbcccddd", 233);
    c.bench_function("format get ts", |b| b.iter(|| get_ts(&key)));
}

criterion_group!(benches, criterion_benchmark);
criterion_main!(benches);

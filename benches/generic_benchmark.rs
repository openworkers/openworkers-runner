use criterion::{Criterion, criterion_group, criterion_main};

#[cfg(feature = "v8")]
use openworkers_runtime_v8::Worker;

#[cfg(feature = "v8")]
openworkers_runner::generate_worker_benchmarks!(Worker);

#[cfg(feature = "v8")]
criterion_group!(benches, worker_benchmarks);

#[cfg(feature = "v8")]
criterion_main!(benches);

// Fallback when no runtime is enabled
#[cfg(not(feature = "v8"))]
fn main() {
    eprintln!("No runtime feature enabled. Run with --features v8");
}

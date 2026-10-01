//! E3 for the Rust-hosted engines: per-call latency of an allocation
//! cycle, p50/p99/max + spike count. The Node driver does the same for
//! wasm-vs-V8. Usage: cargo run --example latency [calls]

use std::time::Instant;

const SRC: &str = include_str!("../bench/kernels/alloc-frame.js");

fn stats(mut samples: Vec<f64>) -> (f64, f64, f64, usize) {
    samples.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let p50 = samples[samples.len() / 2];
    let p99 = samples[(samples.len() as f64 * 0.99) as usize];
    let max = samples[samples.len() - 1];
    let spikes = samples.iter().filter(|x| **x > p50 * 10.0).count();
    (p50, p99, max, spikes)
}

fn main() {
    let n: usize = std::env::args()
        .nth(1)
        .and_then(|a| a.parse().ok())
        .unwrap_or(50_000);

    for (name, mut engine) in [
        (
            "tree-walker",
            Box::new(|out: &mut Vec<u8>| {
                linjs::run(SRC, out).expect("tree-walker");
            }) as Box<dyn FnMut(&mut Vec<u8>)>,
        ),
        (
            "vm",
            Box::new(|out: &mut Vec<u8>| {
                linjs::run_vm(SRC, out).expect("vm");
            }) as Box<dyn FnMut(&mut Vec<u8>)>,
        ),
    ] {
        // Warmup.
        for _ in 0..200 {
            let mut out = Vec::new();
            engine(&mut out);
        }
        let mut samples = Vec::with_capacity(n);
        for _ in 0..n {
            let mut out = Vec::new();
            let start = Instant::now();
            engine(&mut out);
            samples.push(start.elapsed().as_secs_f64() * 1000.0);
        }
        let (p50, p99, max, spikes) = stats(samples);
        println!(
            "[E3 {name}] p50={p50:.4}ms p99={p99:.4}ms max={max:.4}ms spikes(>10x p50)={spikes} over {n} calls"
        );
    }
}

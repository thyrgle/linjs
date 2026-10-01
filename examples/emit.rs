//! Emits per-kernel artifacts for the Node benchmark driver:
//! `<kernel>.wasm`, `<kernel>.js` (transpiled), `<kernel>.expected`
//! (the interpreter's output, the checksum gate).
//!
//! Usage: cargo run --example emit [kernel ...]   (default: all)

use std::fs;
use std::path::Path;

const KERNELS: &[&str] = &[
    "fib",
    "loop-arith",
    "array-sum",
    "array-sum-i32",
    "bit-arith",
    "string-build",
    "calls",
    "alloc-frame",
    "dbg",
];

fn main() {
    let out_dir = Path::new("bench/out");
    fs::create_dir_all(out_dir).expect("create bench/out");
    let wanted: Vec<String> = std::env::args().skip(1).collect();

    for kernel in KERNELS {
        if !wanted.is_empty() && !wanted.iter().any(|w| w == kernel) {
            continue;
        }
        let src = fs::read_to_string(format!("bench/kernels/{kernel}.js"))
            .unwrap_or_else(|e| panic!("read kernel {kernel}: {e}"));
        let (items, errors) = linjs::parse_program(&src).expect("parse");
        assert!(errors.is_empty(), "{kernel}: parse errors {errors:?}");

        // Interpreter output = the checksum every engine must match.
        let mut expected = Vec::new();
        linjs::Interp::new(&mut expected)
            .run(&items)
            .expect("interpreter run");

        // WASM module (strict dialect).
        let wasm = linjs::compile_to_wasm(&src)
            .unwrap_or_else(|e| panic!("{kernel}: wasm compile: {}", e.message));
        fs::write(out_dir.join(format!("{kernel}.wasm")), &wasm).unwrap();

        // Transpiled JS for V8.
        let js = linjs::transpile(&items);
        fs::write(out_dir.join(format!("{kernel}.js")), js).unwrap();

        fs::write(out_dir.join(format!("{kernel}.expected")), expected).unwrap();
        println!("emitted {kernel} ({} wasm bytes)", wasm.len());
    }
}

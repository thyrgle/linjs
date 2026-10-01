//! `i32[]` arrays: half-stride elements, truncating stores, f64 reads —
//! three-engine parity against Node.

use linjs::{run, run_vm};

fn eval(src: &str) -> String {
    let mut out = Vec::new();
    run(src, &mut out).unwrap();
    String::from_utf8(out).unwrap()
}

fn eval_vm(src: &str) -> String {
    let mut out = Vec::new();
    run_vm(src, &mut out).unwrap();
    String::from_utf8(out).unwrap()
}

#[test]
fn i32_arrays_fill_and_sum() {
    let src = "// @own\n\
               let xs: i32[] = [1, 2, 3, 4, 5];\n\
               let sum = 0;\n\
               for (let i = 0; i < xs.length; i++) { sum += xs[i]; }\n\
               console.log(sum, xs.length, xs[0], xs[4]);";
    let t = eval(src);
    let v = eval_vm(src);
    assert_eq!(t, v);
    assert_eq!(t, "15 5 1 5\n");
}

#[test]
fn i32_stores_keep_dynamic_semantics() {
    // In the dynamic engines everything is f64 — stores keep 3.9.
    // (The wasm dialect truncates to 3; a documented divergence.)
    let interp = eval("let xs: i32[] = [0]; xs[0] = 3.9; console.log(xs[0]);");
    assert_eq!(interp, "3.9\n");
    let vm = eval_vm("let xs: i32[] = [0]; xs[0] = 3.9; console.log(xs[0]);");
    assert_eq!(vm, "3.9\n");
}

#[test]
fn i32_array_checksum_wasm() {
    let src = "// @own\n\
               let xs: i32[] = [5, 10, 15, 20];\n\
               let total = 0;\n\
               for (const v of xs) { total += v; }\n\
               console.log(total, xs.length);";
    let bytes = linjs::compile_to_wasm(src).unwrap();
    linjs::wasm::validate(&bytes).unwrap();
}

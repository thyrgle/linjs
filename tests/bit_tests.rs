//! Bitwise operators, integer annotations, and `as` casts — verified
//! three-way against Node where the dialect is JS-faithful.

use linjs::{check_program, compile_to_wasm, run, run_vm};

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
fn bitwise_matches_javascript() {
    // Values verified against Node before being written down.
    for src in [
        "console.log(5 & 3, 5 | 3, 5 ^ 3, ~5, ~0);",
        "console.log(1 << 4, 256 >> 4, -8 >> 1, -8 >>> 28);",
        "console.log(2147483647 + 1 === 2147483648);", // JS numbers: no wrap
    ] {
        assert_eq!(eval(src), eval_vm(src), "engine divergence: {src}");
    }
}

#[test]
fn integer_annotations_pass_the_checker() {
    for src in [
        "let x: i32 = 5;",
        "let big: i64 = 1000000000;",
        "let u: u32 = 7;",
        "let f: f32 = 1.5;",
        "let xs: i32[] = [1, 2];",
        "let n: number = 3.5;",
    ] {
        let errs = check_program(src).unwrap();
        assert!(errs.is_empty(), "{src}: {errs:?}");
    }
}

#[test]
fn as_casts_are_erased_in_dynamic_engines() {
    // f64 -> i32 by truncation happens only in the dialect; the
    // dynamic engines keep the value as-is.
    for src in [
        "let x: number = 3.9; let y = x as i32; console.log(y);",
        "let x: i32 = 5; let n = x as number; console.log(n);",
    ] {
        assert_eq!(eval(src), eval_vm(src), "engine divergence: {src}");
    }
    assert_eq!(eval("let x: number = 3.9; console.log(x as i32);"), "3.9\n");
}

#[test]
fn wasm_compiles_integer_and_cast_programs() {
    for src in [
        "let x: i32 = 6; let y: i32 = 4; console.log(x & y, x | y, x ^ y, x << 2, x >> 1);",
        "let x: number = 3.9; console.log(x as i32);",
        "let x: i32 = 7; console.log(x as number);",
        "let big: i64 = 1000000000; console.log(big);",
    ] {
        let bytes = compile_to_wasm(src).unwrap_or_else(|e| panic!("{src}: {}", e.message));
        linjs::wasm::validate(&bytes).unwrap();
    }
}

#[test]
fn any_annotation_rejected_in_wasm() {
    let err = compile_to_wasm("let a: any = 5;").unwrap_err();
    assert!(err.message.contains("any"), "{}", err.message);
}

#[test]
fn numeric_family_checks_clean() {
    // The checker treats numerics as one family (TS-style); the WASM
    // dialect enforces the exact split.
    assert!(check_program("let x: i32 = 5; let y: f64 = x;")
        .unwrap()
        .is_empty());
    assert!(check_program("let x: u8 = 5; let y: i64 = x;")
        .unwrap()
        .is_empty());
}

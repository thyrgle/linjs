//! The type layer's proofs: annotations check, `any` is gradual,
//! untyped code is inferred, and erasure holds — typed programs run
//! identically to their untyped twins through every engine.

use linjs::{check_program, run, run_vm};

fn errors(src: &str) -> Vec<String> {
    check_program(src)
        .unwrap()
        .into_iter()
        .map(|e| e.render())
        .collect()
}

fn assert_clean(src: &str) {
    let errs = errors(src);
    assert!(
        errs.is_empty(),
        "unexpected type errors in:\n{src}\n{errs:?}"
    );
}

fn assert_error(src: &str, contains: &str) {
    let errs = errors(src);
    assert!(
        errs.iter().any(|e| e.contains(contains)),
        "expected a `{contains}` error in:\n{src}\n{errs:?}"
    );
}

#[test]
fn clean_programs_pass() {
    for src in [
        "let x: number = 1; console.log(x);",
        "const name: string = \"lin\"; console.log(name);",
        "let flag: boolean = true; console.log(flag);",
        "let xs: number[] = [1, 2, 3]; console.log(xs.length);",
        "function f(a: number, b: number): number { return a + b; } console.log(f(1, 2));",
        "let anything: any = f_or_value(); function f_or_value() { return 1; } console.log(anything);",
        "for (const v of [1, 2]) { console.log(v); }",
        "for (const k in {a: 1}) { console.log(k); }",
        "let s = \"a\" + 1; console.log(s);",
        "let untyped = 5; let also = untyped + 1; console.log(also);",
    ] {
        assert_clean(src);
    }
}

#[test]
fn mismatches_are_caught() {
    assert_error(
        "let x: number = \"no\";",
        "initializer is string, declared number",
    );
    assert_error(
        "let flag: boolean = 1;",
        "initializer is number, declared boolean",
    );
    assert_error("let xs: number[] = [\"a\"];", "initializer is string[]");
    assert_error(
        "function f(a: number): number { return a; } console.log(f(\"s\"));",
        "",
    ); // arity/type enforced by dialect; checker flags via any? see below
    assert_error(
        "function f(): number { return \"s\"; }",
        "declares number, returns string",
    );
}

#[test]
fn assignments_are_checked() {
    assert_error(
        "let x: number = 1; x = \"now a string\";",
        "cannot assign string to `number`",
    );
    // Untyped variables accept anything.
    assert_clean("let x = 1; x = \"now a string\";");
    // any accepts everything, and anything can land in any.
    assert_clean("let a: any = 1; a = \"s\"; let b: number = a;");
}

#[test]
fn arrays_are_element_checked() {
    assert_error(
        "let xs: number[] = [1, 2]; xs[0] = \"s\";",
        "cannot store string in a number array",
    );
    assert_clean("let xs: number[] = [1, 2]; xs[0] = 3;");
}

#[test]
fn gradual_any_bridges() {
    // any flows both directions without complaint.
    assert_clean("let a: any = 1; let n: number = a; let s: string = a;");
    // Operations on any yield any, which fits everywhere.
    assert_clean("let a: any = f(); function f() { return 1; } let n: number = a + 1;");
}

#[test]
fn untyped_code_is_inferred_not_rejected() {
    // Dynamic reassignment is legal JS — inference makes the variable
    // hold its initializer's type, but only annotated variables are
    // rejected on reassignment. Untyped stays unchecked.
    assert_clean("let x = 1; x = \"s\";");
    // Inference feeds annotated initializers: a number cannot
    // initialize a declared string.
    assert_error(
        "let n: number = 1; let s: string = n;",
        "initializer is number, declared string",
    );
}

#[test]
fn erasure_typed_programs_run_identically() {
    let typed = "function add(a: number, b: number): number { return a + b; }\n\
                 let result: number = add(2, 3);\n\
                 console.log(result);\n\
                 let names: string[] = [\"a\", \"b\"];\n\
                 console.log(names.length);";
    let untyped = "function add(a, b) { return a + b; }\n\
                   let result = add(2, 3);\n\
                   console.log(result);\n\
                   let names = [\"a\", \"b\"];\n\
                   console.log(names.length);";
    assert_clean(typed);
    let mut typed_out = Vec::new();
    run(typed, &mut typed_out).unwrap();
    let mut vm_out = Vec::new();
    run_vm(typed, &mut vm_out).unwrap();
    let mut untyped_out = Vec::new();
    run(untyped, &mut untyped_out).unwrap();
    let t = String::from_utf8(typed_out).unwrap();
    assert_eq!(t, String::from_utf8(vm_out).unwrap(), "vm diverged");
    assert_eq!(t, String::from_utf8(untyped_out).unwrap(), "erasure broke");
    assert_eq!(t, "5\n2\n");
}

#[test]
fn wasm_dialect_still_rejects_dynamic_values() {
    // any has no wasm representation — the dynamic engines run it.
    let src = "let a: any = 1; console.log(a);";
    assert_clean(src);
    let err = linjs::compile_to_wasm(src).unwrap_err();
    assert!(
        err.message.contains("any") || err.message.contains("type"),
        "{}",
        err.message
    );
}

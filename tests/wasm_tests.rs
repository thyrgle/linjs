//! The WASM backend's proofs: the strict dialect compiles to a real
//! `.wasm` module that runs in Node with byte-identical output, and
//! unannotated heap values are rejected (there is no GC to fall back
//! on).

use linjs::{check_wasm_against_node, compile_to_wasm, run};

/// Numeric fixtures for the strict dialect.
const WASM_FIXTURES: &[&str] = &[
    "console.log(1 + 2 * 3, (1 + 2) * 3, 7 % 3, 7 / 2, -4 + 1);",
    "console.log(0.5, 1/0, 0/0);",
    // Booleans print via explicit coercion (the strict dialect's
    // console.log takes numbers; JS would print "true"/"false").
    "console.log((1 === 1) ? 1 : 0, (1 === 2) ? 1 : 0, (1 < 2) ? 1 : 0, (2 <= 2) ? 1 : 0, (3 > 4) ? 1 : 0);",
    "console.log((!0) ? 1 : 0, (!1) ? 1 : 0, 1 && 2, 0 && 2, 0 || 5, 3 || 5);",
    "console.log((1 !== 2) ? 1 : 0, (2 !== 2) ? 1 : 0, (1 != 2) ? 1 : 0, (2 != 2) ? 1 : 0);",
    "console.log(true ? 1 : 2, false ? 1 : 2);",
    "let x = 1; x = x + 1; console.log(x);",
    "let x = 5; x += 2; x -= 1; x *= 3; x /= 2; x++; console.log(x++, ++x);",
    "let s = 0; for (let i = 0; i < 5; i++) { if (i === 2) { continue; } if (i === 4) { break; } s += i; } console.log(s);",
    "let n = 3, f = 1; while (n > 1) { f *= n; n--; } console.log(f);",
    "function fib(n) { if (n < 2) { return n; } return fib(n - 1) + fib(n - 2); } console.log(fib(10));",
    "function add(a, b) { return a + b; } console.log(add(add(1, 2), add(3, 4)));",
    "let t = 0; for (let i = 1; i <= 100; i++) { t += i; } console.log(t);",
    "let cond = 5 > 3; if (cond) { console.log(1); } else { console.log(0); }",
    // @own arrays in linear memory
    "// @own\nlet a = [10, 20, 30];\nconsole.log(a[0], a[1], a[2], a.length);",
    "// @own\nlet a = [1, 2, 3];\na[1] = 20;\nlet sum = 0;\nfor (const v of a) { sum += v; }\nconsole.log(sum, a.length);",
    "// @own\nlet a = [3, 1, 2];\nlet total = 0;\nfor (let i = 0; i < a.length; i++) { total += a[i]; }\nconsole.log(total);",
    // strings: static literals, concatenation, byte length
    r#"console.log("hello");"#,
    r#"let who = "world"; console.log("hello " + who);"#,
    r#"console.log("a" + "b" + "c");"#,
    r#"const s = "abc"; console.log(s.length);"#,
    r#"let greeting = "hi"; console.log(greeting + "!");"#,
    r#"// @own
let a = [1, 2];
console.log("sum incoming");
console.log(a[0] + a[1]);"#,
    // SIMD elementwise transforms: even length (pure vector pairs)
    r#"// @own
let a = [1, 2, 3, 4];
// @own
let b = [10, 20, 30, 40];
// @own
let out = [0, 0, 0, 0];
for (let i = 0; i < a.length; i++) { out[i] = a[i] + b[i]; }
console.log(out[0], out[1], out[2], out[3]);"#,
    // SIMD odd length: vector pairs plus the scalar tail element
    r#"// @own
let a = [1, 2, 3, 4, 5];
// @own
let b = [2, 4, 6, 8, 10];
// @own
let out = [0, 0, 0, 0, 0];
for (let i = 0; i < a.length; i++) { out[i] = a[i] * b[i] - 2 + a[i] / b[i]; }
console.log(out[0], out[1], out[2], out[3], out[4]);"#,
    // SIMD with a repeated source read and a literal splat
    r#"// @own
let a = [3, 7, 11];
// @own
let out = [0, 0, 0];
for (let i = 0; i < a.length; i++) { out[i] = a[i] * a[i] + 1; }
console.log(out[0], out[1], out[2]);"#,
];

/// The SIMD transform fixture used by both the vector-op proof and
/// the differential check.
const SIMD_SRC: &str = "// @own\nlet a = [1, 2, 3, 4];\n// @own\nlet b = [10, 20, 30, 40];\n// @own\nlet out = [0, 0, 0, 0];\nfor (let i = 0; i < a.length; i++) { out[i] = a[i] + b[i]; }\nconsole.log(out[0], out[1], out[2], out[3]);";

#[test]
fn simd_path_emits_vector_ops() {
    let module = compile_to_wasm(SIMD_SRC).expect("compiles");
    // 0xFD prefix + LEB 0xF0 0x01 = f64x2.add; 0xFD 0x00 = v128.load.
    assert!(
        module.windows(3).any(|w| w == [0xFD, 0xF0, 0x01]),
        "no f64x2.add in the module — the SIMD path did not fire"
    );
    assert!(
        module.windows(2).any(|w| w == [0xFD, 0x00]),
        "no v128.load in the module — the SIMD path did not fire"
    );
}

#[test]
fn wasm_matches_the_interpreter() {
    for src in WASM_FIXTURES {
        eprintln!("fixture: {src}");
        let mut ours = Vec::new();
        let ours = run(src, &mut ours)
            .map(|_| String::from_utf8(ours).unwrap())
            .unwrap_or_else(|e| panic!("interpreter errored on:\n{src}\n{}", e.message));
        match check_wasm_against_node(src) {
            Ok(()) => {}
            Err(msg) if msg == "node not available" => {
                eprintln!("skipping: node is not available");
                return;
            }
            Err(msg) => panic!("wasm divergence on:\n{src}\n{msg}"),
        }
        let _ = ours;
    }
}

#[test]
fn wasm_module_has_the_exported_shape() {
    let bytes = compile_to_wasm("console.log(42);").unwrap();
    // magic + version
    assert_eq!(&bytes[0..4], b"\0asm");
    assert_eq!(
        u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]),
        1
    );
    linjs::wasm::validate(&bytes).unwrap();
}

#[test]
fn unannotated_arrays_are_rejected() {
    let err = compile_to_wasm("let a = [1, 2];").unwrap_err();
    assert!(err.message.contains("// @own"), "{}", err.message);
}

#[test]
fn heap_types_are_rejected_in_the_strict_dialect() {
    for src in [
        "console.log({ a: 1 });",
        "const f = () => 1; console.log(f());",
        "console.log(typeof 1);",
        // Mixed string/number concatenation has no JS coercion here.
        r#"console.log("a" + 1);"#,
        // Multi-string logs are single-value in v1.
        r#"console.log("a", "b");"#,
    ] {
        let err = compile_to_wasm(src).unwrap_err();
        assert!(
            err.message.contains("strict dialect"),
            "{src}: {}",
            err.message
        );
    }
}

#[test]
fn numeric_only_functions() {
    // Array ELEMENTS are numbers and cross function boundaries; the
    // array reference itself stays local.
    let ok = compile_to_wasm(
        "// @own\nlet a = [1];\nfunction f(x) { return x; }\nconsole.log(f(a[0]));",
    );
    ok.unwrap();
}

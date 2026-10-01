//! The Node differential: every fixture program runs in the linjs
//! interpreter and, transpiled, in Node — the outputs must match byte
//! for byte. Skips gracefully where Node is unavailable.

use linjs::check_against_node;

/// Fixture programs whose JavaScript semantics are unambiguous. The
/// linjs interpreter and Node must agree on every line of output.
const FIXTURES: &[&str] = &[
    // arithmetic, precedence, numbers
    "console.log(1 + 2 * 3, (1 + 2) * 3, 7 % 3, 7 / 2, -4 + 1);",
    "console.log(0.5, 1/0, -1/0, 0/0, NaN, Infinity);",
    // strings
    r#"console.log("a" + "b" + 1, "abc".length, "héllo"[1]);"#,
    r#"console.log("tab\there", "line\nbreak".length);"#,
    // equality, truthiness, typeof
    r#"console.log(1 === 1, 1 === "1", 1 == "1", null == undefined, null === undefined);"#,
    r#"console.log(!0, !"", !"x", !null, NaN === NaN, typeof 1, typeof "a", typeof undefined, typeof null);"#,
    "console.log(0 && 99, 1 && 99, 0 || 5, 2 || 5);",
    "console.log(true ? 1 : 2, false ? 1 : 2);",
    // variables and scoping
    "let x = 1; x = x + 1; let y; console.log(x, y);",
    "let x = 1; { let x = 2; console.log(x); } console.log(x);",
    "let a = 1, b = 2, c; console.log(a, b, c);",
    // control flow
    "let s = 0; for (let i = 0; i < 5; i++) { if (i === 2) { continue; } if (i === 4) { break; } s += i; } console.log(s);",
    "let n = 3, f = 1; while (n > 1) { f *= n; n--; } console.log(f);",
    r#"for (const c of "abc") { console.log(c); }"#,
    "const arr = [10, 20, 30]; let sum = 0; for (const v of arr) { sum += v; } console.log(sum);",
    // functions, recursion, closures
    "function fib(n) { if (n < 2) { return n; } return fib(n - 1) + fib(n - 2); } console.log(fib(10));",
    "console.log(early()); function early() { return 42; }",
    "function nothing() {} console.log(nothing());",
    "function counter() { let n = 0; return () => { n += 1; return n; }; } const next = counter(); next(); console.log(next(), next());",
    "const add = (a, b) => a + b; console.log(add(2, 3));",
    "const make = x => y => x + y; console.log(make(10)(5));",
    "let base = 10; function addBase(n) { return base + n; } console.log(addBase(5));",
    // arrays
    "const a = [1, 2, 3]; a.push(4); console.log(a.length, a[0], a[3], a[99]);",
    "const doubled = [1, 2, 3].map(x => x * 2); console.log(doubled[0], doubled[1], doubled[2], doubled.length);",
    "const evens = [1, 2, 3, 4].filter(n => n % 2 === 0); console.log(evens[0], evens[1]);",
    "const a = [1, 2]; console.log(a.pop(), a.length, a.pop(), a.length, a.pop());",
    "const a = [1, 2, 3]; let s = 0; for (const v of a.map(x => x * x)) { s += v; } console.log(s);",
    // updates and compound assignment
    "let x = 5; x += 2; x -= 1; x *= 3; x /= 2; x++; console.log(x++, ++x);",
    "let a = [1]; a[0]++; a[0] += 9; console.log(a[0]);",
    // strings via addition
    r#"console.log("n=" + 1 + 2);"#,
    // ---- M2: objects ----
    "const o = { a: 1, b: \"two\" }; o.c = 3; console.log(o.a, o.b, o.c, o.missing);",
    r#"const a = 1; const o = { a, nested: { x: [1, 2] } }; console.log(o);"#,
    r#"console.log({ a: 1, b: "x" });"#,
    "console.log([]);",
    "console.log({});",
    "console.log([1, 2]);",
    r#"console.log(["x", "it\u0027s"]);"#,
    "console.log([1, [2, [3]]]);",
    r#"console.log({ outer: { inner: 1 }, list: [1, "two", null, true] });"#,
    r#"const counts = { one: 1, two: 2 }; let total = 0; for (const k in counts) { total += counts[k]; } console.log(total);"#,
    r#"console.log({ 1: "one", "with space": 2, ok: 3 });"#,
    "const rows = [{ n: 1 }, { n: 2 }]; console.log(rows.map(r => r.n));",
    "function mutate(o) { o.x = 99; } const o = { x: 1 }; mutate(o); console.log(o.x);",
    "console.log(typeof {}, typeof [], typeof null);",
    // ---- M2: var semantics ----
    "function f() { { var x = 1; } return x; } console.log(f());",
    "function f() { console.log(later); var later = 5; } f();",
    "const fns = []; for (var i = 0; i < 3; i++) { fns.push(() => i); } console.log(fns[0](), fns[1](), fns[2]());",
    "const fns2 = []; for (let j = 0; j < 3; j++) { fns2.push(() => j); } console.log(fns2[0](), fns2[1](), fns2[2]());",
    "let seen = []; for (let i = 0; i < 5; i++) { seen.push(i); i += 1; } console.log(seen.length, seen[0], seen[1], seen[2]);",
    // ---- M2: for-in ----
    r#"const o = { a: 1, b: 2 }; let keys = ""; for (const k in o) { keys += k; } console.log(keys);"#,
    "const arr = [10, 20]; for (const i in arr) { console.log(typeof i, arr[i]); }",
    r#"for (const k in 42) { console.log(k); } console.log("done");"#,
];

/// Programs whose fresh locals qualify for inference — the linjs side
/// runs with ownership applied, and must still match Node byte for
/// byte. This is the inference soundness oracle.
const INFERRED_FIXTURES: &[&str] = &[
    "let buf = [1, 2, 3]; buf[0] = 10; let sum = 0; for (const v of buf) { sum += v; } console.log(sum, buf.length);",
    "let a = [1, 2]; let b = a; b.push(3); console.log(b[0], b[2]);",
    "let a = [1]; let b = a; b[0] = 2; console.log(a[0], b[0]);",
    "const point = { x: 1, y: 2 }; point.x = 5; console.log(point.x, point.y, point.z);",
    "function make() { let fresh = [7, 8]; return fresh.length; } console.log(make());",
    "function total(items) { let sum = 0; for (const v of items) { sum += v; } return sum; } let buf = [1, 2, 3]; console.log(total(buf), buf.length);",
    "let nested = { outer: [1, 2] }; nested.outer.push(3); console.log(nested.outer.length, nested.outer[2]);",
];

#[test]
fn inferred_interpreter_matches_node() {
    let node_missing = match std::process::Command::new("node").arg("--version").output() {
        Ok(out) => !out.status.success(),
        Err(_) => true,
    };
    if node_missing {
        eprintln!("skipping: node is not available");
        return;
    }
    for (i, src) in INFERRED_FIXTURES.iter().enumerate() {
        match linjs::check_inferred_against_node(src) {
            Ok(()) => {}
            Err(msg) if msg == "node not available" => {
                eprintln!("skipping: node is not available");
                return;
            }
            Err(msg) => panic!("inferred fixture {i} diverged: {msg}\nsource: {src}"),
        }
    }
}

#[test]
fn interpreter_matches_node() {
    let node_missing = match std::process::Command::new("node").arg("--version").output() {
        Ok(out) => !out.status.success(),
        Err(_) => true,
    };
    if node_missing {
        eprintln!("skipping: node is not available");
        return;
    }
    for (i, src) in FIXTURES.iter().enumerate() {
        match check_against_node(src) {
            Ok(()) => {}
            Err(msg) if msg == "node not available" => {
                eprintln!("skipping: node is not available");
                return;
            }
            Err(msg) => panic!("fixture {i} diverged: {msg}\nsource: {src}"),
        }
    }
}

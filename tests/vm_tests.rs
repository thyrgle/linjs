//! The bytecode VM's proofs: it must agree with the tree-walking
//! interpreter everywhere (outputs identical), enforce the same memory
//! semantics, and match Node byte for byte.

use linjs::{run, run_vm};

fn both(src: &str) -> (String, String) {
    let mut tree_out = Vec::new();
    let tree = run(src, &mut tree_out).map(|_| String::from_utf8(tree_out).unwrap());
    let mut vm_out = Vec::new();
    let vm = run_vm(src, &mut vm_out).map(|_| String::from_utf8(vm_out).unwrap());
    match (tree, vm) {
        (Ok(a), Ok(b)) => {
            assert_eq!(
                a, b,
                "engines diverged on:\n{src}\n-- tree --\n{a}\n-- vm --\n{b}"
            );
            (a, b)
        }
        (Err(e), other) => panic!(
            "tree-walker errored on:\n{src}\n{e:?}\n(vm: {:?})",
            other.map(|_| ()).err().map(|e| e.message)
        ),
        (_, Err(e)) => panic!("vm errored on:\n{src}\n{}", e.message),
    }
}

/// Programs the VM must run identically to the interpreter.
const PARITY_FIXTURES: &[&str] = &[
    // arithmetic, precedence, numbers, strings
    "console.log(1 + 2 * 3, (1 + 2) * 3, 7 % 3, 7 / 2, -4 + 1);",
    "console.log(0.5, 1/0, -1/0, 0/0, NaN, Infinity);",
    r#"console.log("a" + "b" + 1, "abc".length, "héllo"[1]);"#,
    // equality, truthiness, typeof, logical short-circuit values
    r#"console.log(1 === 1, 1 === "1", 1 == "1", null == undefined, null === undefined);"#,
    r#"console.log(!0, !"", !"x", !null, NaN === NaN, typeof 1, typeof null);"#,
    "console.log(0 && 99, 1 && 99, 0 || 5, 2 || 5);",
    "console.log(true ? 1 : 2, false ? 1 : 2);",
    // variables, scoping, hoisting
    "let x = 1; x = x + 1; let y; console.log(x, y);",
    "let a = 1, b = 2, c; console.log(a, b, c);",
    "function f() { { var x = 1; } return x; } console.log(f());",
    "function f() { console.log(later); var later = 5; } f();",
    // control flow, including break/continue through nested ifs
    "let s = 0; for (let i = 0; i < 5; i++) { if (i === 2) { continue; } if (i === 4) { break; } s += i; } console.log(s);",
    "let n = 3, f = 1; while (n > 1) { f *= n; n--; } console.log(f);",
    r#"for (const c of "abc") { console.log(c); }"#,
    "const arr = [10, 20, 30]; let sum = 0; for (const v of arr) { sum += v; } console.log(sum);",
    r#"const counts = { one: 1, two: 2 }; let t = 0; for (const k in counts) { t += counts[k]; } console.log(t);"#,
    // functions, recursion, closures, per-iteration bindings
    "function fib(n) { if (n < 2) { return n; } return fib(n - 1) + fib(n - 2); } console.log(fib(10));",
    "console.log(early()); function early() { return 42; }",
    "function counter() { let n = 0; return () => { n += 1; return n; }; } const next = counter(); next(); console.log(next(), next());",
    "const add = (a, b) => a + b; console.log(add(2, 3));",
    "const make = x => y => x + y; console.log(make(10)(5));",
    "let base = 10; function addBase(n) { return base + n; } console.log(addBase(5));",
    "const fns = []; for (let i = 0; i < 3; i++) { fns.push(() => i); } console.log(fns[0](), fns[1](), fns[2]());",
    "const fns2 = []; for (var j = 0; j < 3; j++) { fns2.push(() => j); } console.log(fns2[0](), fns2[1](), fns2[2]());",
    "let seen = []; for (let i = 0; i < 5; i++) { seen.push(i); i += 1; } console.log(seen.length, seen[0], seen[1], seen[2]);",
    // arrays and methods
    "const a = [1, 2, 3]; a.push(4); console.log(a.length, a[0], a[3], a[99]);",
    "const doubled = [1, 2, 3].map(x => x * 2); console.log(doubled[0], doubled[1], doubled[2], doubled.length);",
    "const evens = [1, 2, 3, 4].filter(n => n % 2 === 0); console.log(evens[0], evens[1]);",
    "const a = [1, 2]; console.log(a.pop(), a.length, a.pop(), a.length, a.pop());",
    // objects
    r#"const o = { a: 1, b: "two", nested: { x: [1, 2] } };"#,
    r#"const a = 1; const o = { a, nested: { x: [1, 2] } }; console.log(o.a, o.nested.x[1], o.missing);"#,
    "function mutate(o) { o.x = 99; } const o = { x: 1 }; mutate(o); console.log(o.x);",
    // updates and compound assignment
    "let x = 5; x += 2; x -= 1; x *= 3; x /= 2; x++; console.log(x++, ++x);",
    "let a = [1]; a[0]++; a[0] += 9; console.log(a[0]);",
    // memory layer
    "// @own\nlet buf = [1, 2, 3];\nbuf.push(4);\nlet sum = 0;\nfor (const v of buf) { sum += v; }\nconsole.log(sum, buf.length);",
    "// @own\nconst point = { x: 1 };\n// @ref\nconst view = point;\nconsole.log(view.x);",
    "// @own\nlet buf = [1, 2, 3];\n// @own\nlet taken = buf;\nconsole.log(taken[2]);",
    "function make() { return [7, 8]; }\n// @own\nlet owned = make();\nconsole.log(owned[0], owned[1]);",
];

#[test]
fn vm_matches_the_tree_walker_everywhere() {
    for src in PARITY_FIXTURES {
        both(src);
    }
}

#[test]
fn vm_enforces_memory_semantics() {
    // Use after move.
    let mut out = Vec::new();
    let err = run_vm(
        "// @own\nlet buf = [1];\n// @own\nlet taken = buf;\nconsole.log(buf[0]);",
        &mut out,
    )
    .unwrap_err();
    assert_eq!(err.message, "use after move of `buf`");

    // Refs are read-only.
    let err = run_vm(
        "// @own\nlet cfg = { rows: 1 };\n// @ref\nlet view = cfg;\nview.rows = 2;",
        &mut out,
    )
    .unwrap_err();
    assert_eq!(err.message, "@ref is read-only");

    // Own values cannot escape.
    let err = run_vm(
        "// @own\nlet buf = [1];\nfunction leak() { return buf; }\nleak();",
        &mut out,
    )
    .unwrap_err();
    assert!(err.message.contains("cannot escape"), "{}", err.message);

    // Arenas drop with their frames.
    let mut interp_out = Vec::new();
    let mut vm = linjs::vm::Vm::new(&mut interp_out);
    let (items, _) = linjs::parse_program(
        "function make() { // @own\n let x = [1, 2, 3]; return x.length; }\n\
         let s = 0;\n\
         for (let i = 0; i < 100; i++) { s += make(); }\n\
         console.log(s);",
    )
    .unwrap();
    vm.run_items(&items).unwrap();
    assert_eq!(vm.arena_count(), 1, "every call arena was dropped");
}

#[test]
fn vm_reports_runtime_errors_like_the_interpreter() {
    for src in ["nope + 1;", "let x = 1; x();", "const c = 1; c = 2;"] {
        let mut a = Vec::new();
        let mut b = Vec::new();
        let tree = run(src, &mut a).err().map(|e| e.message);
        let vm = run_vm(src, &mut b).err().map(|e| e.message);
        assert_eq!(tree, vm, "error divergence on {src}");
    }
}

#[test]
fn vm_matches_node() {
    let node_missing = match std::process::Command::new("node").arg("--version").output() {
        Ok(out) => !out.status.success(),
        Err(_) => true,
    };
    if node_missing {
        eprintln!("skipping: node is not available");
        return;
    }
    for (i, src) in PARITY_FIXTURES.iter().enumerate() {
        match linjs::check_vm_against_node(src) {
            Ok(()) => {}
            Err(msg) if msg == "node not available" => {
                eprintln!("skipping: node is not available");
                return;
            }
            Err(msg) => panic!("vm fixture {i} diverged from node: {msg}\nsource: {src}"),
        }
    }
}

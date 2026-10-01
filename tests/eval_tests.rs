//! End-to-end evaluation tests: programs run, output matches.

use memjs::run;

fn eval(src: &str) -> String {
    let mut out = Vec::new();
    match run(src, &mut out) {
        Ok(errors) => {
            assert!(errors.is_empty(), "unexpected diagnostics: {errors:?}");
        }
        Err(e) => panic!("runtime error: {}", e.message),
    }
    String::from_utf8(out).unwrap()
}

fn eval_err(src: &str) -> String {
    let mut out = Vec::new();
    let err = run(src, &mut out).expect_err("expected a runtime error");
    err.message
}

#[test]
fn arithmetic_and_precedence() {
    assert_eq!(eval("console.log(1 + 2 * 3);"), "7\n");
    assert_eq!(eval("console.log((1 + 2) * 3);"), "9\n");
    assert_eq!(eval("console.log(7 % 3, 7 / 2, -4 + 1);"), "1 3.5 -3\n");
}

#[test]
fn strings_and_numbers() {
    assert_eq!(eval(r#"console.log("a" + "b" + 1);"#), "ab1\n");
    assert_eq!(eval(r#"console.log("abc".length);"#), "3\n");
    assert_eq!(eval(r#"console.log("héllo"[1]);"#), "é\n");
    assert_eq!(
        eval("console.log(0.5, 1/0, -1/0, 0/0);"),
        "0.5 Infinity -Infinity NaN\n"
    );
}

#[test]
fn variables_and_const() {
    assert_eq!(eval("let x = 1; x = x + 1; console.log(x);"), "2\n");
    assert_eq!(eval("let x; console.log(x);"), "undefined\n");
    assert_eq!(
        eval_err("const c = 1; c = 2;"),
        "assignment to constant variable `c`"
    );
}

#[test]
fn control_flow() {
    assert_eq!(
        eval(
            "let s = 0;
             for (let i = 0; i < 5; i++) {
               if (i === 2) { continue; }
               if (i === 4) { break; }
               s += i;
             }
             console.log(s);"
        ),
        "4\n"
    );
    assert_eq!(
        eval(
            "let n = 3, f = 1;
             while (n > 1) { f *= n; n--; }
             console.log(f);"
        ),
        "6\n"
    );
    assert_eq!(
        eval("for (const c of \"abc\") { console.log(c); }"),
        "a\nb\nc\n"
    );
}

#[test]
fn functions_and_recursion() {
    assert_eq!(
        eval(
            "function fib(n) {
               if (n < 2) { return n; }
               return fib(n - 1) + fib(n - 2);
             }
             console.log(fib(10));"
        ),
        "55\n"
    );
    // Declaration hoisting: call before the declaration.
    assert_eq!(
        eval("console.log(early()); function early() { return 42; }"),
        "42\n"
    );
    // No explicit return is `undefined`.
    assert_eq!(
        eval("function nothing() {} console.log(nothing());"),
        "undefined\n"
    );
}

#[test]
fn closures_and_arrows() {
    assert_eq!(
        eval(
            "function counter() {
               let n = 0;
               return () => { n += 1; return n; };
             }
             const next = counter();
             next();
             console.log(next(), next());"
        ),
        "2 3\n"
    );
    assert_eq!(
        eval(
            "const add = (a, b) => a + b;
             console.log(add(2, 3));"
        ),
        "5\n"
    );
    assert_eq!(
        eval(
            "const make = x => y => x + y;
             console.log(make(10)(5));"
        ),
        "15\n"
    );
    // Function declarations inside blocks hoist within the block.
    assert_eq!(
        eval(
            "function outer() {
               helper();
               function helper() { console.log(\"hoisted\"); }
             }
             outer();"
        ),
        "hoisted\n"
    );
}

#[test]
fn arrays_and_methods() {
    assert_eq!(
        eval(
            "const a = [1, 2, 3];
             a.push(4);
             console.log(a.length, a[0], a[3], a[99]);
             a[1] = 20;
             console.log(a[1]);"
        ),
        "4 1 4 undefined\n20\n"
    );
    assert_eq!(
        eval(
            "const doubled = [1, 2, 3].map(x => x * 2);
             console.log(doubled[0], doubled[1], doubled[2], doubled.length);"
        ),
        "2 4 6 3\n"
    );
    assert_eq!(
        eval(
            "const evens = [1, 2, 3, 4].filter(n => n % 2 === 0);
             console.log(evens[0], evens[1]);"
        ),
        "2 4\n"
    );
    assert_eq!(
        eval(
            "const a = [1, 2];
             console.log(a.pop(), a.length, a.pop(), a.length, a.pop());"
        ),
        "2 1 1 0 undefined\n"
    );
}

#[test]
fn equality_and_truthiness() {
    assert_eq!(
        eval(
            r#"console.log(1 === 1, 1 === "1", 1 == "1", null == undefined, null === undefined);"#
        ),
        "true false true true false\n"
    );
    assert_eq!(
        eval(
            r#"console.log(!0, !"", !"x", !null, NaN === NaN, typeof 1, typeof "a", typeof undefined, typeof null);"#
        ),
        "true true false true false number string undefined object\n"
    );
    assert_eq!(
        eval("let n = 0; n && 99; console.log(n); n = \"\" || 5; console.log(n);"),
        "0\n5\n"
    );
    assert_eq!(eval("console.log(true ? \"y\" : \"n\");"), "y\n");
}

#[test]
fn ternary_update_compound() {
    assert_eq!(
        eval("let x = 5; x += 2; x -= 1; x *= 3; x /= 2; x++; console.log(x++, ++x);"),
        "10 12\n"
    );
}

#[test]
fn scoping_shadowing() {
    assert_eq!(
        eval(
            "let x = 1;
             function f() {
               let x = 2;
               function g() { return x; }
               return g();
             }
             console.log(f(), x);"
        ),
        "2 1\n"
    );
}

#[test]
fn runtime_errors() {
    assert_eq!(eval_err("nope + 1;"), "nope is not defined");
    assert_eq!(eval_err("let x = 1; x();"), "1 is not a function");
    assert_eq!(
        eval_err("const a = [1]; a.push2(1);"),
        "[object Array].push2 is not a function"
    );
}

#[test]
fn objects() {
    assert_eq!(
        eval(
            r#"const o = { a: 1, b: "two" };
               console.log(o.a, o.b, o.missing);
               o.c = 3;
               console.log(o.c, o["a"]);"#
        ),
        "1 two undefined\n3 1\n"
    );
    assert_eq!(
        eval(
            r#"const o = {};
               o["computed key"] = 1;
               o[2] = "two";
               console.log(o["computed key"], o[2], o.gone);"#
        ),
        "1 two undefined\n"
    );
    // Shorthand, mixed keys, nesting.
    assert_eq!(
        eval(
            r#"const a = 1;
               const o = { a, b: { c: [1, 2] } };
               console.log(o.a, o.b.c[1]);"#
        ),
        "1 2\n"
    );
    // Objects are reference values.
    assert_eq!(
        eval(
            "function mutate(o) { o.x = 99; }
             const o = { x: 1 };
             mutate(o);
             console.log(o.x);"
        ),
        "99\n"
    );
    // Objects passed to methods survive as callback results.
    assert_eq!(
        eval(
            r#"const rows = [{ n: 1 }, { n: 2 }];
               const names = rows.map(r => r.n);
               console.log(names[0], names[1]);"#
        ),
        "1 2\n"
    );
}

#[test]
fn var_hoisting_and_function_scope() {
    // var is function-scoped: visible after its block.
    assert_eq!(
        eval(
            "function f() {
               { var x = 1; }
               return x;
             }
             console.log(f());"
        ),
        "1\n"
    );
    // var hoists: usable (undefined) before its statement runs.
    assert_eq!(
        eval(
            "function f() {
               console.log(later);
               var later = 5;
             }
             f();"
        ),
        "undefined\n"
    );
    // let stays block-scoped.
    assert_eq!(
        eval_err(
            "function f() {
               { let y = 1; }
               return y;
             }
             f();"
        ),
        "y is not defined"
    );
    // Closures over a shared var binding.
    assert_eq!(
        eval(
            "const fns = [];
             for (var i = 0; i < 3; i++) { fns.push(() => i); }
             console.log(fns[0](), fns[1](), fns[2]());"
        ),
        "3 3 3\n"
    );
    // Closures over per-iteration let bindings.
    assert_eq!(
        eval(
            "const fns = [];
             for (let i = 0; i < 3; i++) { fns.push(() => i); }
             console.log(fns[0](), fns[1](), fns[2]());"
        ),
        "0 1 2\n"
    );
    // Body mutations of a let loop variable affect the loop.
    assert_eq!(
        eval(
            "let seen = [];
             for (let i = 0; i < 5; i++) { seen.push(i); i += 1; }
             console.log(seen.length, seen[0], seen[1], seen[2]);"
        ),
        "3 0 2 4\n"
    );
}

#[test]
fn for_in() {
    assert_eq!(
        eval(
            r#"const o = { a: 1, b: 2 };
               let keys = "";
               for (const k in o) { keys += k; }
               console.log(keys);"#
        ),
        "ab\n"
    );
    // Arrays iterate index strings.
    assert_eq!(
        eval(
            "const a = [10, 20];
             for (const i in a) { console.log(typeof i, a[i]); }"
        ),
        "string 10\nstring 20\n"
    );
    // Non-objects iterate nothing, without error.
    assert_eq!(eval("for (const k in 42) { console.log(k); }"), "");
    assert_eq!(eval("for (const k in null) { console.log(k); }"), "");
}

#[test]
fn parse_diagnostics_are_reported() {
    let mut out = Vec::new();
    let errors = memjs::run("let ok = 1; console.log(ok); let = 2;", &mut out).unwrap();
    assert_eq!(errors.len(), 1, "one broken item");
    // The good items still ran.
    assert_eq!(String::from_utf8(out).unwrap(), "1\n");
}

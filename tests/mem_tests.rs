//! The memory-mode proofs: `@own` allocates into activation arenas,
//! moves are enforced, `@ref` is a read-only borrow, and `@own` values
//! cannot escape — while unannotated programs behave exactly as before.

use memjs::run;

fn eval(src: &str) -> String {
    let mut out = Vec::new();
    match run(src, &mut out) {
        Ok(errors) => assert!(errors.is_empty(), "unexpected diagnostics: {errors:?}"),
        Err(e) => panic!("runtime error: {}", e.message),
    }
    String::from_utf8(out).unwrap()
}

fn eval_err(src: &str) -> String {
    let mut out = Vec::new();
    run(src, &mut out)
        .expect_err("expected a runtime error")
        .message
}

#[test]
fn own_values_behave_like_their_gc_twins() {
    assert_eq!(
        eval(
            "// @own
             let buf = [1, 2, 3];
             buf.push(4);
             let sum = 0;
             for (const v of buf) { sum += v; }
             console.log(sum, buf.length, buf[3], buf[0]);"
        ),
        "10 4 4 1\n"
    );
    assert_eq!(
        eval(
            "// @own
             const point = { x: 1, y: 2 };
             console.log(point.x, point.y, point.z, typeof point);"
        ),
        "1 2 undefined object\n"
    );
    // Function results can be owned: the fresh return value moves in.
    assert_eq!(
        eval(
            "function make() { return [7, 8]; }
             // @own
             let owned = make();
             console.log(owned[0], owned[1]);"
        ),
        "7 8\n"
    );
}

#[test]
fn moves_move() {
    assert_eq!(
        eval(
            "// @own
             let buf = [1, 2, 3];
             // @own
             let taken = buf;
             console.log(taken[2]);"
        ),
        "3\n"
    );
    assert_eq!(
        eval_err(
            "// @own
             let buf = [1, 2, 3];
             // @own
             let taken = buf;
             console.log(buf[0]);"
        ),
        "use after move of `buf`"
    );
    // Reads of a moved binding are errors even in dead expressions.
    assert_eq!(
        eval_err(
            "// @own
             let a = [1];
             // @own
             let b = a;
             const c = a;"
        ),
        "use after move of `a`"
    );
}

#[test]
fn refs_are_read_only_borrows() {
    assert_eq!(
        eval(
            "// @own
             let config = { rows: 10, name: \"grid\" };
             // @ref
             let view = config;
             console.log(view.rows, view.name);"
        ),
        "10 grid\n"
    );
    assert_eq!(
        eval_err(
            "// @own
             let config = { rows: 10 };
             // @ref
             let view = config;
             view.rows = 5;"
        ),
        "@ref is read-only"
    );
    assert_eq!(
        eval_err(
            "// @own
             let buf = [1];
             // @ref
             let view = buf;
             view.push(2);"
        ),
        "@ref is read-only"
    );
    // Refs do not move: the source stays usable.
    assert_eq!(
        eval(
            "// @own
             let buf = [1, 2];
             // @ref
             let view = buf;
             console.log(buf[0], view[1]);"
        ),
        "1 2\n"
    );
    // Writes go through the owner only.
    assert_eq!(
        eval(
            "// @own
             let buf = [1];
             // @ref
             let view = buf;
             buf.push(2);
             console.log(view.length);"
        ),
        "2\n"
    );
}

#[test]
fn own_values_cannot_escape() {
    assert_eq!(
        eval_err(
            "// @own
             let buf = [1];
             function leak() { return buf; }
             leak();"
        ),
        "an @own value cannot escape its activation — return a copy or restructure"
    );
    assert_eq!(
        eval_err(
            "const gc = [];
             // @own
             let buf = [1];
             gc.push(buf);"
        ),
        "cannot store an @own value in a garbage-collected container"
    );
    assert_eq!(
        eval_err(
            "// @own
             let buf = [1];
             let copy = buf;"
        ),
        "cannot assign an @own value to unannotated `copy` — declare it with // @own or // @ref"
    );
    assert_eq!(
        eval_err("// @own\nlet n = 5;"),
        "@own applies to arrays and objects, not 5"
    );
    assert_eq!(
        eval_err(
            "const plain = [1];
             // @ref
             let r = plain;"
        ),
        "@ref requires an @own value, not [object Array]"
    );
}

#[test]
fn arguments_borrow_own_values() {
    // Reads through parameters work; the caller keeps ownership.
    assert_eq!(
        eval(
            "function total(items) {
               let sum = 0;
               for (const v of items) { sum += v; }
               return sum;
             }
             // @own
             let buf = [1, 2, 3];
             console.log(total(buf), buf.length);"
        ),
        "6 3\n"
    );
    // Writes through parameters are rejected (parameters are borrows).
    assert_eq!(
        eval_err(
            "function sneak(items) { items.push(99); }
             // @own
             let buf = [1];
             sneak(buf);"
        ),
        "@ref is read-only"
    );
}

#[test]
fn arenas_are_dropped_with_their_activation() {
    // 100 calls push and pop 100 arenas; only the global one remains.
    let mut out = Vec::new();
    let mut interp = memjs::Interp::new(&mut out);
    let (items, _) = memjs::parse_program(
        "function make() { // @own\n let x = [1, 2, 3]; return x.length; }\n\
         let s = 0;\n\
         for (let i = 0; i < 100; i++) { s += make(); }\n\
         console.log(s);",
    )
    .unwrap();
    interp.run(&items).unwrap();
    assert_eq!(interp.arena_count(), 1, "every call arena was dropped");
    assert_eq!(String::from_utf8(out).unwrap(), "300\n");
}

#[test]
fn annotated_programs_stay_valid_javascript() {
    // The annotations are comments: the transpiled output of an
    // annotated program is unchanged and runs in Node. (Byte-identity
    // with Node is asserted for these programs in node_diff_tests.)
    let src = "// @own\nlet buf = [1, 2];\n// @ref\nlet view = buf;\nconsole.log(view.length);\n";
    let (items, _) = memjs::parse_program(src).unwrap();
    let js = memjs::transpile(&items);
    assert!(
        !js.contains("@own") && !js.contains("@ref"),
        "annotations must not leak into codegen:\n{js}"
    );
    assert!(js.contains("let buf = [1, 2];"), "transpiled:\n{js}");
}

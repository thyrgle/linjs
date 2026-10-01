//! The incremental proofs: edits re-parse only what changed, untouched
//! items keep their parsed ASTs pointer-identical, and evaluation after
//! any edit sequence equals a cold parse of the same text.

use increparse::{CancelToken, Engine, SerialExecutor, Session};
use linjs::passes::{program_from_tree, Ctx, ItemsPass, Settle};
use linjs::{transpile, Interp, Item};

fn make_engine() -> Engine<Ctx> {
    Engine::with((ItemsPass::new(), Settle))
}

/// Runs a program's items, returning stdout.
fn eval_items(items: &[Item]) -> String {
    let mut out = Vec::new();
    Interp::new(&mut out).run(items).expect("runs");
    String::from_utf8(out).unwrap()
}

/// The `Arc` pointer of the function named `name`, read straight from
/// the tree so pointer identity is observable.
fn fn_ptr(tree: &increparse::ParseTree<Ctx>, name: &str) -> Option<*const linjs::ast::FnDef> {
    for id in tree.children(tree.root()) {
        if let Ctx::Fn(def) = tree.ctx(*id) {
            if def.name == name {
                return Some(std::sync::Arc::as_ptr(def));
            }
        }
    }
    None
}

/// The minimal byte-range edit turning `old` into `new` (char-boundary
/// safe). Test-side twin of the same helper in increparse-lsp.
fn diff_edit(old: &str, new: &str) -> increparse::Edit {
    let ob = old.as_bytes();
    let nb = new.as_bytes();
    let mut start = ob.iter().zip(nb).take_while(|(a, b)| a == b).count();
    while start > 0 && (!old.is_char_boundary(start) || !new.is_char_boundary(start)) {
        start -= 1;
    }
    let max_suffix = (ob.len() - start).min(nb.len() - start);
    let mut suffix = ob[ob.len() - max_suffix..]
        .iter()
        .rev()
        .zip(nb[nb.len() - max_suffix..].iter().rev())
        .take_while(|(a, b)| a == b)
        .count();
    while suffix > 0
        && (!old.is_char_boundary(old.len() - suffix) || !new.is_char_boundary(new.len() - suffix))
    {
        suffix -= 1;
    }
    increparse::Edit::replace(start, old.len() - suffix, new.len() - suffix)
}

#[test]
fn edit_one_function_reparses_only_it() {
    let src = "function a() { return 1; }\nfunction b() { return 2; }\nconsole.log(a() + b());\n";
    let engine = make_engine();
    let mut session = Session::from_source(src, 0, Ctx::Root);
    session.run(&engine, src, &SerialExecutor, &CancelToken::new());

    let a_before = fn_ptr(session.tree(), "a").expect("a parsed");

    // Edit b's body: `return 2` -> `return 20`. The edit lands inside b;
    // a's bytes are untouched.
    let at = src.find("return 2;").unwrap();
    let edited = format!(
        "{}0{}",
        &src[..at + "return 2".len()],
        &src[at + "return 2".len()..]
    );
    let edit = diff_edit(src, &edited);
    session.edit(edit);
    let report = session.run(&engine, &edited, &SerialExecutor, &CancelToken::new());

    // A handful of nodes: the root's rounds plus whatever items the
    // re-segmentation could not reuse. Not the whole world.
    assert!(
        report.nodes_processed <= 6,
        "an edit must not re-process the world: {}",
        report.nodes_processed
    );
    // Untouched function a kept its exact parsed AST.
    let a_after = fn_ptr(session.tree(), "a").expect("a survives");
    assert_eq!(a_before, a_after, "function a must keep its parsed AST");

    // And the program still evaluates to the new answer.
    let (items, errors) = program_from_tree(session.tree());
    assert!(errors.is_empty(), "diagnostics: {errors:?}");
    assert_eq!(eval_items(&items), "21\n");
}

#[test]
fn unchanged_files_fully_reuse() {
    let src = "function a() { return 1; }\nconsole.log(a());\n";
    let engine = make_engine();
    let mut session = Session::from_source(src, 0, Ctx::Root);
    let first = session.run(&engine, src, &SerialExecutor, &CancelToken::new());
    assert!(first.reached_fixpoint, "every run settles to a fixpoint");

    // A no-op run: nothing pending, nothing processed.
    let second = session.run(&engine, src, &SerialExecutor, &CancelToken::new());
    assert_eq!(second.nodes_processed, 0, "an unchanged file does no work");
    assert!(second.reached_fixpoint);
}

#[test]
fn evaluation_matches_cold_after_edits() {
    // Four explicit snapshots of one program; between each pair the
    // incremental session takes the diff edit and must evaluate exactly
    // like a cold parse of the same text.
    let t0 = "function f(n) { return n * 2; }\nconsole.log(f(3));\n";
    let t1 = "function f(n) { return n * 4; }\nconsole.log(f(3));\n";
    let t2 = "function f(n) { return n * 4; }\nconsole.log(f(30));\n";
    let t3 = "function f(n) { return n * 4 + 1; }\nconsole.log(f(30), f(1));\n";

    let engine = make_engine();
    let mut source = t0.to_string();
    let mut session = Session::from_source(&source, 0, Ctx::Root);
    session.run(&engine, &source, &SerialExecutor, &CancelToken::new());

    assert_eq!(eval_items(&program_from_tree(session.tree()).0), "6\n");

    for next in [t1, t2, t3] {
        let edit = diff_edit(&source, next);
        source = next.to_string();
        session.edit(edit);
        session.run(&engine, &source, &SerialExecutor, &CancelToken::new());

        let (items, errors) = program_from_tree(session.tree());
        assert!(errors.is_empty(), "no diagnostics after an edit");
        let incremental = eval_items(&items);

        let (cold_items, _) = linjs::parse_program(&source).unwrap();
        let cold = eval_items(&cold_items);

        assert_eq!(incremental, cold, "diverged on:\n{source}");
    }
    // The edits actually changed behavior.
    assert_eq!(eval_items(&program_from_tree(session.tree()).0), "121 5\n");
}

#[test]
fn transpiled_programs_are_stable() {
    let src = "function f(n) { return n * 2; }\nconsole.log(f(3));\n";
    let (items, _) = linjs::parse_program(src).unwrap();
    let js = transpile(&items);
    assert!(js.contains("function f(n)"));
    assert!(js.contains("console.log(f(3));"), "transpiled:\n{js}");
}

//! memjs — a JavaScript subset with incremental bones.
//!
//! A pilot built on [`increparse`] to prove the compiler pattern: the
//! engine segments a source file into top-level items and parses each
//! item exactly where it stands; the parsed ASTs ride along in the tree
//! as contexts; edits re-segment and re-parse only what changed
//! (content-keyed caches keep untouched items pointer-identical); and a
//! tree-walking interpreter executes the settled tree. A transpiler
//! prints the same AST back to JavaScript so every memjs program can be
//! checked against Node.
//!
//! # Examples
//!
//! ```
//! use memjs::run;
//!
//! let mut out = Vec::new();
//! run("console.log(1 + 2);", &mut out).unwrap();
//! assert_eq!(String::from_utf8(out).unwrap(), "3\n");
//! ```
//!
//! # The M1+M2 subset, plus the M3 memory layer
//!
//! `let` / `const` / `var` (with JavaScript's scoping and hoisting
//! semantics: `var` is function-scoped, `let` loop variables get
//! per-iteration bindings), assignment (including compound and
//! `++` / `--`), `if` / `else`, `while`, `for`, `for..of`, `for..in`,
//! `break` / `continue` / `return`, function declarations, arrow
//! functions and closures, calls, member and index access, object
//! literals with shorthand, ternaries, `typeof`, arrays with `length`,
//! `push`, `pop`, `map`, and `filter`, strings, f64 numbers,
//! `undefined`, `null`, truthiness, `===`, and `==` with coercion.
//! `console.log` formats containers the way Node does, and the test
//! suite verifies every fixture against Node byte for byte.
//!
//! On top sits the gradual-memory layer ([`mem`]): `// @own` comments
//! allocate arrays and objects into the current activation's arena —
//! dropped deterministically on return, never on the reference-counted
//! heap — with enforced moves and read-only `// @ref` borrows. Annotated
//! programs are still 100% valid JavaScript, and the Node differential
//! covers them too.
//!
//! And most code needs no annotations at all: [`infer`] classifies
//! unannotated fresh-value declarations (own-able, or garbage-collected
//! with the reason), and [`run_inferred`] applies the verdicts — the
//! Node differential verifies inferred programs byte for byte.
//!
//! Divergences from JavaScript are documented in [`interp`], [`mem`],
//! and [`infer`].

pub mod ast;
pub mod infer;
pub mod interp;
pub mod lexer;
pub mod mem;
pub mod parser;
pub mod passes;
pub mod transpile;
pub mod value;

pub use ast::{Expr, Stmt};
pub use interp::{Interp, InterpError, Item};
pub use passes::{parse_program, program_from_tree, Ctx, ItemsPass, Settle};
pub use transpile::transpile;

/// Classifies the program's unannotated fresh-value declarations
/// without running anything: `(name, verdict)` in declaration order.
/// See [`infer`] for the rules.
pub fn infer_report(source: &str) -> Result<infer::Inference, InterpError> {
    let (mut items, _) = parse_program(source).map_err(|e| InterpError { message: e })?;
    Ok(infer::infer(&mut items, false))
}

/// Parses, applies ownership inference, then runs: unannotated
/// declarations that qualify allocate into arenas, exactly as if they
/// carried `// @own`. Sound inference means the output matches
/// [`run`] — and Node — byte for byte.
pub fn run_inferred(
    source: &str,
    out: &mut dyn std::io::Write,
) -> Result<Vec<(usize, String)>, InterpError> {
    let (mut items, errors) = parse_program(source).map_err(|e| InterpError { message: e })?;
    infer::infer(&mut items, true);
    let mut interp = Interp::new(out);
    interp.run(&items)?;
    Ok(errors)
}

/// Like [`check_against_node`], but the memjs side runs with ownership
/// inference applied — the soundness oracle: inferred programs must
/// produce identical output to Node.
pub fn check_inferred_against_node(source: &str) -> Result<(), String> {
    let node = which_node().ok_or("node not available")?;
    let mut ours = Vec::new();
    run_inferred(source, &mut ours).map_err(|e| format!("memjs error: {}", e.message))?;
    let ours = String::from_utf8(ours).map_err(|e| e.to_string())?;

    let (items, _) = parse_program(source)?;
    let js = transpile(&items);
    let output = std::process::Command::new(node)
        .arg("-e")
        .arg(js)
        .output()
        .map_err(|e| format!("node failed to run: {e}"))?;
    let theirs = String::from_utf8_lossy(&output.stdout).to_string();
    if ours != theirs {
        return Err(format!(
            "outputs diverge (inferred):\n-- memjs --\n{ours}\n-- node --\n{theirs}"
        ));
    }
    Ok(())
}

/// Parses and runs `source`, writing `console.log` output to `out`.
///
/// Returns the parse diagnostics (offset + message) if any item failed
/// to parse; the runnable items still execute.
pub fn run(
    source: &str,
    out: &mut dyn std::io::Write,
) -> Result<Vec<(usize, String)>, InterpError> {
    let (items, errors) = parse_program(source).map_err(|e| InterpError { message: e })?;
    let mut interp = Interp::new(out);
    interp.run(&items)?;
    Ok(errors)
}

/// Runs `source` through the interpreter and its transpiled form through
/// `node`, comparing the outputs. Returns an explanation when they
/// disagree or `node` is unavailable.
pub fn check_against_node(source: &str) -> Result<(), String> {
    let node = which_node().ok_or("node not available")?;
    let mut ours = Vec::new();
    run(source, &mut ours).map_err(|e| format!("memjs error: {}", e.message))?;
    let ours = String::from_utf8(ours).map_err(|e| e.to_string())?;

    let (items, _) = parse_program(source)?;
    let js = transpile(&items);
    let output = std::process::Command::new(node)
        .arg("-e")
        .arg(js)
        .output()
        .map_err(|e| format!("node failed to run: {e}"))?;
    let theirs = String::from_utf8_lossy(&output.stdout).to_string();
    if ours != theirs {
        return Err(format!(
            "outputs diverge:\n-- memjs --\n{ours}\n-- node --\n{theirs}"
        ));
    }
    Ok(())
}

fn which_node() -> Option<std::path::PathBuf> {
    let out = std::process::Command::new("node")
        .arg("--version")
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| std::path::PathBuf::from("node"))
}

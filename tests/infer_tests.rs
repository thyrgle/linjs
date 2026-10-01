//! Ownership-inference proofs: verdicts match the rules, inferred
//! programs behave identically to their garbage-collected versions,
//! and explicit annotations are never touched.

use linjs::infer::{Inference, Reason, Verdict};
use linjs::{infer_report, run, run_inferred};

fn verdicts(src: &str) -> Vec<(String, Verdict)> {
    let Inference { verdicts } = infer_report(src).unwrap();
    verdicts
}

fn eval_with(src: &str, inferred: bool) -> Result<String, String> {
    let mut out = Vec::new();
    let result = if inferred {
        run_inferred(src, &mut out)
    } else {
        run(src, &mut out)
    };
    match result {
        Ok(_) => Ok(String::from_utf8(out).unwrap()),
        Err(e) => Err(e.message),
    }
}

#[test]
fn fresh_locals_are_own_able() {
    assert_eq!(
        verdicts("let buf = [1, 2, 3];"),
        vec![("buf".into(), Verdict::OwnAble)]
    );
    assert_eq!(
        verdicts("const point = { x: 1 };"),
        vec![("point".into(), Verdict::OwnAble)]
    );
}

#[test]
fn returns_disqualify() {
    assert_eq!(
        verdicts("function make() { let fresh = [1]; return fresh; }"),
        vec![("fresh".into(), Verdict::Gc(Reason::EscapesViaReturn))]
    );
}

#[test]
fn container_stores_disqualify() {
    assert_eq!(
        verdicts("const gc = [];\nlet buf = [1];\ngc.push(buf);"),
        vec![
            ("gc".into(), Verdict::OwnAble), // fresh literal, never escapes itself
            ("buf".into(), Verdict::Gc(Reason::StoredIntoContainer)),
        ]
    );
}

#[test]
fn closure_captures_disqualify() {
    assert_eq!(
        verdicts("let buf = [1];\nconst read = () => buf[0];"),
        vec![("buf".into(), Verdict::Gc(Reason::CapturedByClosure))]
    );
}

#[test]
fn shared_aliasing_disqualifies_both() {
    // `b = a` with `a` used afterwards: the value must keep sharing.
    assert_eq!(
        verdicts("let a = [1];\nlet b = a;\nconsole.log(a[0], b[0]);"),
        vec![
            ("a".into(), Verdict::Gc(Reason::SharedAliasing)),
            ("b".into(), Verdict::Gc(Reason::NotAFreshValue)),
        ]
    );
}

#[test]
fn move_chains_qualify_when_the_source_dies() {
    assert_eq!(
        verdicts("let a = [1, 2];\nlet b = a;\nconsole.log(b.length);"),
        vec![
            ("a".into(), Verdict::OwnAble),
            ("b".into(), Verdict::OwnAble)
        ]
    );
}

#[test]
fn moves_from_non_candidates_stay_gc() {
    // A call result is fresh at runtime, but v1 inference only reports
    // fresh-literal and identifier initializers — a call result is
    // simply not a candidate.
    assert_eq!(
        verdicts("function make() { return [1]; }\nlet x = make();"),
        vec![]
    );
    // Parameters are not candidates.
    assert_eq!(
        verdicts("function f(p) { let local = p; return local[0]; }"),
        vec![("local".into(), Verdict::Gc(Reason::NotAFreshValue))]
    );
}

#[test]
fn explicit_annotations_are_untouched() {
    // The @own declaration is not a candidate; the alias through @ref
    // is not either.
    let report =
        verdicts("// @own\nlet buf = [1];\n// @ref\nlet view = buf;\nconsole.log(view[0]);");
    assert!(report.is_empty(), "no unannotated candidates: {report:?}");
}

#[test]
fn inference_preserves_behavior() {
    // The same program with and without inference must produce the
    // same output — soundness is behavior preservation.
    let programs = [
        // fresh locals with reads and mutation
        "let buf = [1, 2, 3];\nbuf[0] = 10;\nlet sum = 0;\nfor (const v of buf) { sum += v; }\nconsole.log(sum, buf.length);",
        // move chain: source never used again
        "let a = [1, 2];\nlet b = a;\nb.push(3);\nconsole.log(b[0], b[2]);",
        // shared aliasing stays garbage-collected: both bindings work
        "let a = [1];\nlet b = a;\nb[0] = 2;\nconsole.log(a[0], b[0]);",
        // mixed with explicit annotations
        "// @own\nlet kept = [5];\nlet local = [1, 2];\nconsole.log(kept[0], local[1]);",
        // objects
        "const point = { x: 1, y: 2 };\npoint.x = 5;\nconsole.log(point.x, point.y, point.z);",
        // functions with fresh locals that escape via return
        "function make() { let fresh = [7, 8]; return fresh; }\nconsole.log(make()[1]);",
    ];
    for src in programs {
        let plain = eval_with(src, false).unwrap_or_else(|e| panic!("{src}\nplain errored: {e}"));
        let inferred =
            eval_with(src, true).unwrap_or_else(|e| panic!("{src}\ninferred errored: {e}"));
        assert_eq!(plain, inferred, "inference changed behavior on:\n{src}");
    }
}

#[test]
fn inference_flips_qualifying_declarations_to_own() {
    // White-box: after apply, the AST carries Mem::Own exactly where
    // the verdicts said OwnAble.
    use linjs::passes::parse_program;

    let src = "let a = [1, 2];\nlet b = a;\nconsole.log(b[0]);";
    let (mut items, _) = parse_program(src).unwrap();
    linjs::infer::infer(&mut items, true);
    let modes: Vec<String> = items
        .iter()
        .filter_map(|item| match item {
            linjs::Item::Stmt(linjs::Stmt::Let { decls, mem, .. }) => {
                Some(format!("{}:{mem:?}", decls[0].0))
            }
            _ => None,
        })
        .collect();
    assert_eq!(modes, vec!["a:Own".to_string(), "b:Own".to_string()]);
}

#[test]
fn functions_get_their_own_analysis() {
    // A name used at top level does not disqualify the local in a
    // function body, and vice versa.
    let report = verdicts(
        "function make() { let local = [1]; return local.length; }\nlet top = [2];\nconsole.log(top[0]);",
    );
    assert_eq!(
        report,
        vec![
            ("local".into(), Verdict::OwnAble),
            ("top".into(), Verdict::OwnAble)
        ]
    );
}

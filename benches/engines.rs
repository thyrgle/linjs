//! E1 continued: the Rust-hosted engines on the same kernel programs
//! the Node driver runs — tree-walker, bytecode VM, and Boa (a
//! pure-Rust JS engine, the peer for our tree-walker).
//!
//! Run with: cargo bench --bench engines

use criterion::{criterion_group, criterion_main, Criterion};
use std::hint::black_box;

const FIB: &str = include_str!("../bench/kernels/fib.js");
const LOOP_ARITH: &str = include_str!("../bench/kernels/loop-arith.js");
const ARRAY_SUM: &str = include_str!("../bench/kernels/array-sum.js");
const CALLS: &str = include_str!("../bench/kernels/calls.js");
const STRING_BUILD: &str = include_str!("../bench/kernels/string-build.js");

/// Runs a kernel through an engine and returns the checksum output —
/// the same bytes every engine must produce.
fn checksum(engine: impl FnOnce(&str, &mut Vec<u8>), src: &str) -> Vec<u8> {
    let mut out = Vec::new();
    engine(src, &mut out);
    out
}

fn linjs_tree(src: &str, out: &mut Vec<u8>) {
    linjs::run(src, out).expect("tree-walker run");
}

fn linjs_vm(src: &str, out: &mut Vec<u8>) {
    linjs::run_vm(src, out).expect("vm run");
}

fn bench_engine(c: &mut Criterion, group_name: &str, engine: impl Fn(&str, &mut Vec<u8>)) {
    let mut group = c.benchmark_group(group_name);
    for (name, src) in [
        ("fib", FIB),
        ("loop-arith", LOOP_ARITH),
        ("array-sum", ARRAY_SUM),
        ("calls", CALLS),
        ("string-build", STRING_BUILD),
    ] {
        // Correctness gate: the checksum must match the interpreter's.
        let expected = checksum(linjs_tree, src);
        let actual = checksum(&engine, src);
        assert_eq!(expected, actual, "{group_name}/{name} diverged");

        group.bench_function(name, |b| {
            b.iter(|| {
                let mut out = Vec::new();
                engine(black_box(src), &mut out);
                black_box(out);
            })
        });
    }
    group.finish();
}

fn bench_tree_walker(c: &mut Criterion) {
    bench_engine(c, "tree-walker", linjs_tree);
}

fn bench_vm(c: &mut Criterion) {
    bench_engine(c, "vm", linjs_vm);
}

fn bench_boa(c: &mut Criterion) {
    use std::cell::RefCell;

    thread_local! {
        static SINK: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
    }

    fn console_log(
        _: &boa_engine::JsValue,
        args: &[boa_engine::JsValue],
        _: &mut boa_engine::Context,
    ) -> boa_engine::JsResult<boa_engine::JsValue> {
        SINK.with(|sink| {
            let line: Vec<String> = args.iter().map(|a| a.display().to_string()).collect();
            sink.borrow_mut()
                .extend_from_slice(line.join(" ").as_bytes());
            sink.borrow_mut().extend_from_slice(b"\n");
        });
        Ok(boa_engine::JsValue::undefined())
    }

    bench_engine(c, "boa", |src, out| {
        SINK.with(|sink| sink.borrow_mut().clear());
        let mut context = boa_engine::Context::default();
        let console = boa_engine::JsObject::with_object_proto(context.intrinsics());
        console
            .set(
                boa_engine::js_string!("log"),
                boa_engine::JsValue::from(
                    boa_engine::NativeFunction::from_fn_ptr(console_log)
                        .to_js_function(context.realm()),
                ),
                false,
                &mut context,
            )
            .expect("register console.log");
        context
            .register_global_property(
                boa_engine::js_string!("console"),
                console,
                boa_engine::property::Attribute::all(),
            )
            .expect("register console");
        context
            .eval(boa_engine::Source::from_bytes(src))
            .expect("boa eval");
        SINK.with(|sink| out.extend_from_slice(&sink.borrow()));
    });
}

criterion_group!(benches, bench_tree_walker, bench_vm, bench_boa);
criterion_main!(benches);

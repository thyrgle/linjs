# linjs

A JavaScript subset with **gradual memory semantics**, built on
[increparse](https://github.com/thyrgle/increparse) — the proving ground for a
simple idea: start loose and garbage-collected like JavaScript, but let memory
annotations opt values into deterministic, arena-based lifetimes. Annotated
programs stay **100% valid JavaScript**.

```js
// @own
let buf = [1, 2, 3];   // arena allocation — not the reference-counted heap
// @own
let taken = buf;       // move: `buf` is now a use-after-move error
// @ref
let view = taken;      // read-only borrow: `view.push(...)` is rejected

function leak() { return buf; }  // error: @own values cannot escape
```

## The language

A strict JavaScript subset: `let`/`const`/`var` with real scoping and hoisting
semantics (`var` function-scoped, `let` loop variables per-iteration),
assignment with compound forms and `++`/`--`, `if`/`else`, `while`, `for`,
`for..of`, `for..in`, `break`/`continue`/`return`, function declarations with
hoisting, arrow functions and closures, calls, member and index access, object
literals with shorthand, ternaries, `typeof`, arrays with `length`/`push`/
`pop`/`map`/`filter`, strings, f64 numbers, `undefined`, `null`, truthiness,
`===`, and `==` with coercion. `console.log` formats containers the way Node
does.

## How it works

One increparse pass segments a file into top-level items and parses each item
exactly where it stands; the ASTs ride in the parse tree as contexts. A
content-keyed cache keeps untouched items pointer-identical across edits, so
an edit to one function re-parses only that function — asserted by `Arc`
pointer identity in the test suite. A tree-walking interpreter executes the
settled tree, and a transpiler prints the same AST back to JavaScript.

## The memory layer

`// @own` and `// @ref` line comments annotate declarations:

* `@own` allocates the value into the current activation's arena — dropped
  deterministically when the call returns, never touching the
  reference-counted heap.
* Declarations initialized from an `@own` binding **move** it; reads of a
  moved binding are use-after-move errors.
* `@ref` borrows: a read-only handle. Writes through a borrow — including
  through function parameters, which receive `@own` arguments as borrows —
  are rejected.
* Escapes are errors: returning an `@own` value, storing one in a
  garbage-collected container, or assigning one to an unannotated binding.
* Ownership depth is one: members of owned containers are ordinary
  garbage-collected values.

## Verified against Node

linjs ships a transpiler, and the test suite runs fixture programs through
both the interpreter and Node — the outputs must match byte for byte,
including annotated programs (Node ignores the comments). Divergences from
JavaScript are documented in the crate docs.

## Running it

```sh
cargo test   # the test suite is the demo
```

```rust
use linjs::run;

let mut out = Vec::new();
run("console.log(1 + 2);", &mut out).unwrap();
assert_eq!(String::from_utf8(out).unwrap(), "3\n");
```

Requires Node for the differential tests; the suite skips them gracefully
when Node is absent.

## The CLI

```sh
cargo build --release
./target/release/linjs check program.js          # parse + type diagnostics
./target/release/linjs check --strict program.js # also gate the WASM dialect
./target/release/linjs run program.js            # the interpreter
./target/release/linjs build program.js          # program.wasm, validated
```

`check` renders parse errors with line:col and type errors with the
annotated subject, and exits 1 when anything is wrong — the editor
and CI entry point. Types are TS-style: checked statically, erased at
runtime.

## In the browser

The `wasm/` directory builds the whole compiler into a WebAssembly
module: `check`, `run`, `transpile`, and `compile` — diagnostics and a
runnable `.wasm` module, both client-side.

```sh
cd wasm
cargo build --release --target wasm32-unknown-unknown
wasm-bindgen --target web --out-dir pkg \
  target/wasm32-unknown-unknown/release/linjs_wasm.wasm
python3 -m http.server 8000   # open http://localhost:8000/demo/
```

`node wasm/bindings_test.mjs` proves it end to end under Node: the
bindings produce the same diagnostics as the CLI, and a module compiled
in-wasm runs in the host's own WebAssembly runtime with
checksum-identical output.

## Benchmarks

The three engines are measured against V8 and Boa with a
checksum-gated, methodology-first harness: kernel programs run
identically on every engine, outputs must match before any number is
reported, and V8 gets 2,000 warmup iterations so it is measured at its
optimizing best. Headline: the compiled WASM beats V8 on
call-heavy f64 code (~4x on fib), loses 1.2-3x on loop/array work,
and — the point of the memory model — allocation latency is flat after
one-time arena growth, with no GC pauses. Integer bitwise code runs at
parity with V8. Method and numbers:
[docs/benchmarks.md](docs/benchmarks.md).

## The bytecode VM

linjs compiles to bytecode and runs on an explicit stack machine
(`linjs::run_vm`) — the structural rehearsal for a WASM backend. Frames
have slot-based locals resolved at compile time; locals captured by
closures are boxed into shared cells (with per-iteration cells for
`for (let ...)` matching JavaScript exactly); every call pushes a frame
and its activation arena, and return pops both — `@own` teardown is
structural, not simulated. The VM passes the whole test suite in
lockstep with the tree-walking interpreter: identical outputs on every
fixture, identical memory semantics, and byte-identical output against
Node.

## The WASM backend

`linjs::compile_to_wasm` compiles the **strict dialect** — numbers,
booleans, control flow, functions, and `// @own` arrays — to a real
WebAssembly module. `@own` arrays and strings live in linear memory — strings as static
data-segment literals plus arena-allocated concatenations —
bump-allocated from a per-frame arena; the escape rules guarantee
frame teardown is a pointer reset, so annotated code runs with **no
garbage collector anywhere in the pipeline**. Unannotated arrays are a compile error: in
linear memory there is nothing to fall back on. The differential
harness instantiates the emitted module in Node and checks its output
against the interpreter byte for byte.

## The type layer

TypeScript-style annotations, checked statically and **erased at
runtime** — typed and untyped programs run identically through every
engine:

```js
function add(a: number, b: number): number { return a + b; }
let x: number = add(2, 3);
let anything: any = f();          // the gradual escape hatch
let xs: number[] = [1, 2];        // pairs with // @own for WASM
```

`linjs::check_program` returns every type error; the type language is
`number`, `string`, `boolean`, `any`, and `T[]`, with inference for
unannotated code and `any` compatible in both directions. Types say
*what* a value is, `@own` says *how long it lives* — and the WASM
backend consumes both: annotated concrete types decide its valtypes,
while `any` is rejected there (dynamic values need the GC engines).
That is the whole thesis in one sentence: **typed + `@own` = compiled
WASM, zero GC.**

## Rust-style numeric types

The dialect types are Rust's own: `i8`-`i64`, `u8`-`u64`, `f32`, `f64`
(`number` is the `f64` alias, so all JS-flavored code keeps working).
Integer annotations get real valtypes in the WASM dialect — wrapping
i32 arithmetic, trapping division, half-memory arrays — and Rust-style
casts (`x as i32`) emit trunc/convert/wrap there, while erasing to
identity in the dynamic engines. Bitwise and shift operators
(`& | ^ ~ << >> >>>`) carry JavaScript's i32 semantics, verified
against Node. Divergences are documented in the crate docs.

## Ownership inference

Most code should need no annotations at all. `linjs::infer_report`
classifies every unannotated fresh-value declaration:

```rust
let report = linjs::infer_report(src).unwrap();
// [("buf", OwnAble), ("data", Gc(EscapesViaReturn)), ...]
```

A declaration is own-able when no return escapes it, no container
stores it, no closure captures it, no aliasing shares it — and it is
not used in arithmetic, which JavaScript would coerce silently.
`linjs::run_inferred` applies those verdicts before running: qualifying
declarations allocate into arenas, exactly as if they carried `// @own`.
The Node differential covers inferred programs too — identical output to
Node is the soundness oracle. The one documented v1 limit: ordinary
calls are assumed to borrow only; a callee that writes through its
parameter fails loudly at runtime rather than silently diverging.

## Status

M1 (language + interpreter + transpiler), M2 (objects, `var`, `for..in`,
correct `for`-`let` closures), M3 (`@own`/`@ref` arenas with enforced
moves and borrows), M4 (ownership inference), M5 (the bytecode VM), M6
(the WASM backend for the strict dialect), M7 (strings in linear
memory), M8 (the TS-style type layer), M9 (Rust-style numeric types
with `as` casts and bitwise operators), and M10 (bounds-check
elimination, strength reduction, and f64x2/i32x4 SIMD elementwise
codegen) are complete. The `linjs` CLI and the `linjs-wasm` browser
build ship alongside. Next: widening the strict dialect (object
layouts, methods) toward full-language WASM compilation.

License: MIT OR Apache-2.0.

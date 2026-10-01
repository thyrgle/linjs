# memjs

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

memjs ships a transpiler, and the test suite runs fixture programs through
both the interpreter and Node — the outputs must match byte for byte,
including annotated programs (Node ignores the comments). Divergences from
JavaScript are documented in the crate docs.

## Running it

```sh
cargo test   # the test suite is the demo
```

```rust
use memjs::run;

let mut out = Vec::new();
run("console.log(1 + 2);", &mut out).unwrap();
assert_eq!(String::from_utf8(out).unwrap(), "3\n");
```

Requires Node for the differential tests; the suite skips them gracefully
when Node is absent.

## Ownership inference

Most code should need no annotations at all. `memjs::infer_report`
classifies every unannotated fresh-value declaration:

```rust
let report = memjs::infer_report(src).unwrap();
// [("buf", OwnAble), ("data", Gc(EscapesViaReturn)), ...]
```

A declaration is own-able when no return escapes it, no container
stores it, no closure captures it, no aliasing shares it — and it is
not used in arithmetic, which JavaScript would coerce silently.
`memjs::run_inferred` applies those verdicts before running: qualifying
declarations allocate into arenas, exactly as if they carried `// @own`.
The Node differential covers inferred programs too — identical output to
Node is the soundness oracle. The one documented v1 limit: ordinary
calls are assumed to borrow only; a callee that writes through its
parameter fails loudly at runtime rather than silently diverging.

## Status

M1 (language + interpreter + transpiler), M2 (objects, `var`, `for..in`,
correct `for`-`let` closures), M3 (`@own`/`@ref` arenas with enforced
moves and borrows), and M4 (ownership inference) are complete.

License: MIT OR Apache-2.0.

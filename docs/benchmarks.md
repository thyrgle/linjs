# Benchmarks

How linjs performs, measured with a written-down method so the numbers
mean something. Single machine (Linux, Rust release profile for
hosted engines, Node 26 for V8 + wasm), medians over repeated runs,
checksums validated before any timing is reported.

## Method

Kernel programs live in `bench/kernels/` — plain files in the strict
dialect subset, valid as both linjs and JavaScript, with the
measurement loop *inside* the program (setup → loop accumulating a
checksum → one `console.log`). Every engine runs literally the same
file; a checksum gate compares each engine's output against the
interpreter's before any number is reported.

- **Node side** (`bench/driver.mjs`): times the compiled WASM module
  and the transpiled JS on V8. Warmup is 2,000 iterations — V8 needs
  thousands of calls to tier up to its optimizing compiler, and a
  benchmark that catches V8 in its interpreter is lying.
- **Rust side** (`benches/engines.rs`, criterion): tree-walker,
  bytecode VM, and Boa (a pure-Rust JS engine — the peer for our
  tree-walker). Release profile, 100 samples.
- **E3 latency** (`bench/driver.mjs` alloc-frame, `examples/latency.rs`):
  per-call sampling, p50/p99/max + spike count (>10× p50).

What the Rust-side numbers include: `run` and `run_vm` parse and
compile per iteration — they measure *end-to-end cost of running a
program once*, not steady-state execution. The Node-side numbers
exclude compile time (reported separately, ~0.2 ms for all kernels).

## E1 — Engine vs engine (median wall time per program run)

| Kernel | tree-walker | linjs VM | Boa | V8 (JS) | linjs WASM |
|---|---|---|---|---|---|
| fib (recursion) | 256 ms | 217 ms | 200 ms | 3.7 ms | **0.91 ms** |
| loop-arith | 135 ms | 117 ms | 44 ms | 0.70 ms | 0.81 ms |
| array-sum | 991 ms | 1,009 ms | 197 ms | 0.37 ms | 1.14 ms |
| calls | 113 ms | 102 ms | 74 ms | 0.12 ms | 0.17 ms |
| string-build | 172 µs | 149 µs | 453 µs | <0.05 ms | 0.03 ms |

(Rust side via criterion, 100 samples. Node side via driver, 9 reps
after 2,000 warmups. V8 string-build is below reliable measurement at
this kernel size.)

Reads:

- **Our compiled WASM is the fastest linjs everywhere**, and beats V8
  on fib by ~4× — a small f64-heavy module goes through TurboFan
  extremely well, and skips every JS semantic V8 has to model
  (block scoping, boxing edges).
- **V8 wins loop-heavy and array-heavy workloads** (loop-arith 1.2×,
  array-sum 3.1×, calls 1.4×). Its JIT specializes hot loops; our wasm
  pays a bounds check + trunc per element access, and V8's packed-double
  arrays are legendary for a reason.
- **String building is ~9× slower in wasm** — every concat is an
  arena copy; V8 uses ropes.
- **Tree-walker vs VM vs Boa**: the VM is 8–18% faster than the
  tree-walker on most kernels — real but modest, because both include
  parse+compile per iteration and the kernels are call/loop dominated.
  Boa beats our VM on loops (its bytecode is older and better-tuned)
  and loses on fib/calls.

## E2 — The thesis benchmark (`array-sum`, @own arrays)

Our `@own` array is a contiguous f64 run in linear memory; the same
program on V8 uses a GC-heap JS array. Result: **V8 still wins
throughput (3×)** — its JIT eliminates bounds checks; we emit a check,
a trunc, and a branch per element access. That gap is compiler work
(redundant-elimination passes we don't do yet), not a memory-model
cost. The memory model shows up in E3 instead.

## E6b — Bounds-check elimination + strength reduction (M10)

Recognizing `for (let i = 0; i < a.length; i++)` as a sequential
element loop (i not captured, not reassigned, a never reassigned)
unlocks strength-reduced addressing: a running pointer local replaces
the per-access `base + 8 + i*stride` computation, and `a[i]` reads
lose their bounds check, trunc, and NaN else-path entirely. When the
body never reads `i` outside `a[i]`, the induction variable is dropped
and the loop runs purely on the address.

| Kernel | before | after | V8 |
|---|---|---|---|
| array-sum | 1.02 ms | **0.59 ms** | 0.37 ms |
| array-sum-i32 | 1.30 ms | **0.59 ms** | 0.36 ms |

1.7× faster than the checked path, within 1.6× of V8 — and the
remaining gap is the accumulator bookkeeping, not memory access.
With i32[] the payload is half the linear memory of the f64 version.
The checker-verified safety scans (no closure capture, no reassignment,
no shadowing) mean elimination is only ever applied where reads are
provably in bounds.

## E6 — Integer dialect (M9 follow-up)

With Rust-style numeric types landed, two integer kernels join the
suite — and **bit-arith reaches parity with V8 (1.0×)**: a chain of
`+ ^ << | >>>` on i32 locals maps 1:1 to wasm integer instructions,
and V8's Smi path holds no advantage on the same operations. The
remaining gaps are the known compiler taxes: `array-sum-i32` at 3.7×
(bounds check + truncate per element read, on top of the f64→i32
conversion for JS-identical reads) — bounds-check elimination is the
next compiler lever, with i32[] arrays already at half the linear
memory of f64[].

| Kernel | linjs WASM | V8 (JS) | ratio |
|---|---|---|---|
| bit-arith (i32 locals, bitwise chain) | 0.36 ms | 0.36 ms | **1.0×** |
| array-sum-i32 (i32[] elements) | 1.30 ms | 0.35 ms | 3.7× |

Checksums verified against the interpreter and Node before timing.
Integer division and casts trap in the dialect (documented
divergences); the kernels avoid div-by-zero.

## E7 — SIMD elementwise transforms (M10 phase B)

Sequential elementwise stores (`out[i] = a[i] + b[i]` shapes) compile
to a length-guarded `f64x2` vector loop: two lanes per iteration, one
`v128` load per source, one `v128` store, an odd-tail scalar element,
and a fall back to the JS-identical checked loop when lengths diverge.
On the `vector-add` kernel the compiled module runs **3.4× faster than
V8** — the loop body is four instructions per pair, no bounds checks,
no NaN checks.

| Kernel | linjs WASM | V8 (JS) | ratio |
|---|---|---|---|
| vector-add (f64 elementwise, 40 elems × 20k) | 0.21 ms | 0.80 ms | **0.3×** |
| vector-add-i32 (i32[] lanes, same shape) | 0.29 ms | 0.78 ms | **0.4×** |

`i32[]` transforms run four lanes per v128 (`i32x4` add/sub/mul, no
integer division — those shapes stay on the checked loop). Lane
arithmetic is wrapping i32, bit-identical to the checked paths for
in-range values; division and out-of-range literals fall back to the
checked loop, which keeps the per-element JS semantics.

Post-M10-phase-A refresh of the numeric kernels (current medians):

| Kernel | linjs WASM | V8 (JS) | ratio |
|---|---|---|---|
| fib | 0.81 ms | 3.63 ms | **0.2×** |
| array-sum (BCE, strength-reduced) | 0.59 ms | 0.83 ms | **0.7×** |
| array-sum-i32 | 0.63 ms | 0.40 ms | 1.6× |
| bit-arith | 0.42 ms | 0.37 ms | 1.1× |
| calls | 0.17 ms | 0.12 ms | 1.4× |
| loop-arith | 0.80 ms | 0.67 ms | 1.2× |

Bounds-check elimination + strength reduction flipped `array-sum` from
2.9× behind V8 to **faster than V8 (0.7×)**; SIMD elementwise flips the
store-shaped kernels to 3.4× ahead. The remaining behind-V8 kernels are
the known compiler taxes: reduction with i32 element reads
(`array-sum-i32` still truncates per read) and string/call-heavy shapes.

## E3 — Allocation latency (the differentiator)

One allocation cycle per call, 100,000 calls, per-call sampling:

| Engine | p50 | p99 | max | spikes (>10× p50) |
|---|---|---|---|---|
| linjs WASM (`@own` arena) | 1.9 µs | 5.3 µs | 178 µs | 36 |
| V8 (GC heap array) | 1.4 µs | 2.0 µs | 241 µs | 74 |
| linjs tree-walker* | 2.99 ms | 3.47 ms | 5.72 ms | 0 |
| linjs VM* | 3.23 ms | 3.81 ms | 5.53 ms | 0 |

\* Rust-hosted numbers include a full parse+compile per call — they
measure end-to-end program invocation, not allocation alone, so the
interesting comparison is wasm-vs-V8.

WASM's max (178 µs) is a **one-time `memory.grow`** — after the arena
reaches steady state, latency is flat with no recurring pauses. V8's
74 spikes are scavenger collections that *recur for the lifetime of
the program*; longer runs keep paying them. This is the allocation
story the language promises: deterministic teardown, no GC pauses
after warmup, and the one spike that exists is allocation *growth*,
not reclamation.

WASM compile time is negligible at this scale: 0.15–0.25 ms per
kernel module (0.4–3.4 KB).

## Caveats

- Single machine, warm environment; absolute numbers vary, ratios are
  the signal.
- V8 is a 25-year optimizing JIT with tiered compilation; linjs's
  backend was written in a week and does no optimization passes beyond
  what a correct emitter requires. Matching V8 throughput is not the
  goal — deterministic memory is.
- The tree-walker/VM numbers include parse+compile per iteration.
- Boa's console is a no-op stub registered for the checksum.

## Reproduce

```sh
cargo run --example emit                 # emit wasm/js/expected per kernel
node bench/driver.mjs fib                # checksum gate + E1 + timing
node bench/driver.mjs array-sum          # the E2 thesis kernel
node bench/driver.mjs alloc-frame        # E3 allocation latency
cargo bench --bench engines              # tree-walker vs VM vs Boa
cargo run --example latency -- 20000     # E3 for the Rust engines
```

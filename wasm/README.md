# linjs-wasm — the compiler, in your tab

The whole linjs pipeline behind four bindings: static checks, the
tree-walking interpreter, the JS transpiler, and the strict-dialect
WASM compiler. `cargo check`-quality diagnostics and a runnable
`.wasm` module, both in the browser.

## Bindings

| binding | what it does |
|---|---|
| `check(src)` | every parse + type diagnostic, one string each (`"3:9: parse error: …"`). Never throws. |
| `run(src)` | runs on the interpreter, returns captured `console.log` output. The semantic ground truth. |
| `transpile(src)` | plain JavaScript out, annotations erased. |
| `compile(src)` | raw `.wasm` bytes for the strict dialect — feed `new WebAssembly.Module(bytes)` directly. |

## Build

```sh
rustup target add wasm32-unknown-unknown
cargo install wasm-bindgen-cli   # version must match the wasm-bindgen dep

cd wasm
cargo build --release --target wasm32-unknown-unknown
wasm-bindgen --target web --out-dir pkg \
  target/wasm32-unknown-unknown/release/linjs_wasm.wasm
```

(`wasm-pack build --target web` works too.)

## Try it

```sh
# serve the demo (ES modules need http)
python3 -m http.server 8000
# open http://localhost:8000/demo/
```

## Test (Node, no browser needed)

```sh
cargo build --release --target wasm32-unknown-unknown
node bindings_test.mjs
```

The test regenerates the nodejs glue itself and runs the full matrix:
clean diagnostics, type + parse diagnostics, interpreter output,
transpile erasure, runtime-error throwing — and it instantiates a
compiled module through Node's own WebAssembly runtime, gating its
output against the interpreter exactly like the CI checksum harness.

## Layout

- `src/lib.rs` — the bindings (thin wrappers over the `linjs` crate)
- `demo/index.html` — the playground: check / run / transpile / compile-and-run
- `bindings_test.mjs` — the Node end-to-end test
- `pkg/`, `pkg-node/` — generated glue (gitignored)

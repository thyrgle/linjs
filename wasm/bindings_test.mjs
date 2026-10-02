// End-to-end test of the linjs browser bindings, run under Node.
//
// Regenerates the nodejs glue from the cdylib (wasm-bindgen CLI must
// be installed: `cargo install wasm-bindgen-cli`), then exercises
// check / run / transpile / compile — including executing a compiled
// module through Node's own WebAssembly runtime.
//
//   cd wasm
//   cargo build --release --target wasm32-unknown-unknown
//   node bindings_test.mjs

import { execSync } from 'child_process';
import { fileURLToPath } from 'url';
import { dirname, join } from 'path';

const here = dirname(fileURLToPath(import.meta.url));

execSync(
  'wasm-bindgen --target nodejs --out-dir pkg-node target/wasm32-unknown-unknown/release/linjs_wasm.wasm',
  { stdio: 'inherit', cwd: here },
);

const { check, transpile, compile, run } = await import(
  `${here}/pkg-node/linjs_wasm.js`
);

let failures = 0;
const eq = (label, got, want) => {
  const g = JSON.stringify(got);
  const w = JSON.stringify(want);
  if (g === w) {
    console.log(`ok   ${label}`);
  } else {
    failures++;
    console.log(`FAIL ${label}\n  got:  ${g}\n  want: ${w}`);
  }
};
const throws = (label, fn) => {
  try {
    fn();
    failures++;
    console.log(`FAIL ${label}: should have thrown`);
  } catch (e) {
    console.log(`ok   ${label} (threw: ${String(e.message).slice(0, 48)})`);
  }
};

// 1. Clean program: no diagnostics; the interpreter runs it; the WASM
//    path compiles it; and the compiled module runs in Node's own
//    WebAssembly runtime with checksum-identical output.
const clean =
  '// @own\nlet a = [1, 2, 3];\nlet t = 0;\nfor (let i = 0; i < a.length; i++) { t += a[i]; }\nconsole.log(t);\n';
eq('check(clean)', check(clean), []);
eq('run(clean)', run(clean), '6\n');
const bytes = compile(clean);
if (String.fromCharCode(...bytes.slice(0, 4)) !== '\0asm') {
  failures++;
  console.log('FAIL compile(clean): not wasm magic');
} else {
  console.log(`ok   compile(clean): ${bytes.length} bytes, wasm magic`);
}
{
  let got = null;
  const mod = new WebAssembly.Module(bytes);
  const inst = new WebAssembly.Instance(mod, {
    env: {
      log1: (x) => {
        got = x;
      },
      logstr: () => {},
    },
  });
  inst.exports.run();
  eq('compiled module output', got, 6);
}

// 2. Diagnostics: type errors and parse errors with line:col.
eq(
  'check(type error)',
  check('let x: number = 5;\nx = "oops";\nconsole.log(x);\n'),
  ['type error in `x`: cannot assign string to `number`'],
);
eq('check(parse error)', check('let a = 1;\nlet b = ;\n'), [
  '2:9: parse error: unexpected token Semi',
]);

// 3. Transpile erases annotations; output is plain JS.
eq(
  'transpile',
  transpile('let x: number = 1;\nconsole.log(x);\n').trim(),
  'let x = 1;\nconsole.log(x);',
);

// 4. Runtime errors throw (an unbound identifier).
throws('run throws on unbound call', () => run('missingFn();'));

// 5. Member access on null is the dialect's documented v1 leniency:
//    undefined, not an exception.
eq('run(null.x) is lenient', run('console.log(null.x);'), 'undefined\n');

// 6. Non-strict-dialect programs throw on compile.
throws('compile throws on objects', () => compile('let o = {a: 1};\nconsole.log(o.a);\n'));

console.log(failures === 0 ? '\nall bindings green' : `\n${failures} FAILURES`);
process.exit(failures === 0 ? 0 : 1);

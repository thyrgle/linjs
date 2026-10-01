// The Node-side benchmark driver: checksum gate + E1 table + E3 latency.
//
// Usage: node bench/driver.mjs <kernel>
//   E1: times the compiled WASM module and the transpiled JS on V8.
//   E3 (kernel=alloc-frame): samples per-run() call latency, p50/p99/max.
//
// Checksum gate: every engine must print the same output as the
// interpreter's `.expected` file before any number is reported.

import { readFileSync } from 'fs';
import { performance } from 'perf_hooks';

const kernel = process.argv[2] ?? 'fib';
// V8 needs thousands of iterations to tier up to its optimizing
// compiler; the warmup is deliberately generous so every number is the
// engine's *best steady state*, not its interpreter warmup.
const WARMUP = 2000;
const REPS = 9;

const expected = readFileSync(`bench/out/${kernel}.expected`, 'utf8').trim();
const wasmBytes = readFileSync(`bench/out/${kernel}.wasm`);
const jsSource = readFileSync(`bench/out/${kernel}.js`, 'utf8');

// Captured program output per engine.
let captured = '';
const captureLog = (...xs) => { captured += xs.join(' ') + '\n'; };

// ---- WASM ----
{
  const compileStart = performance.now();
  const module = new WebAssembly.Module(wasmBytes);
  const compileMs = performance.now() - compileStart;
  console.log(`[compile] ${kernel}.wasm: ${compileMs.toFixed(2)} ms (${wasmBytes.length} bytes)`);
}

function makeEnv(memRef) {
  const dec = new TextDecoder();
  return new Proxy({}, {
    get: (t, name) => {
      if (typeof name === 'string' && name === 'logstr') {
        return (p) => {
          const len = new DataView(memRef.memory.buffer).getUint32(p, true);
          const bytes = new Uint8Array(memRef.memory.buffer, p + 8, len);
          captureLog(dec.decode(bytes));
        };
      }
      if (typeof name === 'string' && name.startsWith('log')) {
        return (...xs) => captureLog(...xs);
      }
      return undefined;
    },
  });
}

const envRef = { memory: null };
const instance = new WebAssembly.Instance(new WebAssembly.Module(wasmBytes), { env: makeEnv(envRef) });
envRef.memory = instance.exports.memory;

function runWasmOnce() {
  captured = '';
  instance.exports.run();
  return captured;
}

// ---- V8 (transpiled) ----
const runV8 = new Function('console', jsSource);

const consoleShim = { log: (...xs) => { captured += xs.join(' ') + '\n'; } };

function runV8Once() {
  captured = '';
  runV8(consoleShim);
  return captured;
}

// ---- checksum gate ----
{
  const w = runWasmOnce();
  const v = runV8Once();
  if (w.trim() !== expected) {
    console.error(`CHECKSUM MISMATCH (wasm): got ${JSON.stringify(w.trim())}, want ${JSON.stringify(expected)}`);
    process.exit(1);
  }
  if (v.trim() !== expected) {
    console.error(`CHECKSUM MISMATCH (v8): got ${JSON.stringify(v.trim())}, want ${JSON.stringify(expected)}`);
    process.exit(1);
  }
  console.log(`[checksum] ${kernel}: wasm ✓ v8 ✓ (${expected.replace(/\n/g, ' | ')})`);
}

// ---- timing ----
function timeIt(runOnce) {
  for (let i = 0; i < WARMUP; i++) runOnce();
  const samples = [];
  for (let i = 0; i < REPS; i++) {
    const start = performance.now();
    runOnce();
    samples.push(performance.now() - start);
  }
  samples.sort((a, b) => a - b);
  const median = samples[Math.floor(samples.length / 2)];
  return { median, min: samples[0], max: samples[samples.length - 1] };
}

if (kernel === 'alloc-frame') {
  // E3: per-call latency sampling — every run() is one allocation cycle.
  for (let i = 0; i < 500; i++) runWasmOnce();
  const N = 100000;
  const spikeAt = (samples, p50) => samples.filter((x) => x > p50 * 10).length;
  const wasmSamples = new Array(N);
  for (let i = 0; i < N; i++) {
    const start = performance.now();
    instance.exports.run();
    wasmSamples[i] = performance.now() - start;
  }
  wasmSamples.sort((a, b) => a - b);
  const wasmP50 = wasmSamples[Math.floor(N / 2)];
  const pct = (p) => wasmSamples[Math.floor((p / 100) * N)].toFixed(4);
  console.log(
    `[E3 wasm] p50=${wasmP50.toFixed(4)}ms p99=${pct(99)}ms max=${wasmSamples[N - 1].toFixed(4)}ms ` +
    `spikes(>10x p50)=${spikeAt(wasmSamples, wasmP50)} over ${N} calls`
  );

  for (let i = 0; i < 500; i++) runV8Once();
  const v8Samples = new Array(N);
  for (let i = 0; i < N; i++) {
    const start = performance.now();
    runV8Once();
    v8Samples[i] = performance.now() - start;
  }
  v8Samples.sort((a, b) => a - b);
  const v8P50 = v8Samples[Math.floor(N / 2)];
  const vpct = (p) => v8Samples[Math.floor((p / 100) * N)].toFixed(4);
  console.log(
    `[E3 v8 ] p50=${v8P50.toFixed(4)}ms p99=${vpct(99)}ms max=${v8Samples[N - 1].toFixed(4)}ms ` +
    `spikes(>10x p50)=${spikeAt(v8Samples, v8P50)} over ${N} calls`
  );
} else {
  const w = timeIt(runWasmOnce);
  const v = timeIt(runV8Once);
  console.log(`[E1] ${kernel}:`);
  console.log(`    wasm : median ${w.median.toFixed(2)} ms  (min ${w.min.toFixed(2)}, max ${w.max.toFixed(2)}) over ${REPS}`);
  console.log(`    v8   : median ${v.median.toFixed(2)} ms  (min ${v.min.toFixed(2)}, max ${v.max.toFixed(2)}) over ${REPS}`);
  console.log(`    wasm/v8: ${(w.median / v.median).toFixed(1)}x`);
}

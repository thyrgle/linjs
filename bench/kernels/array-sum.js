// E2 kernel (the thesis): numeric array iteration. @own in linjs-wasm
// means a contiguous f64 run in linear memory; in V8 it is a plain JS
// array backed by the GC heap. Same program, same checksum.
// @own
let a = [
  1, 2, 3, 4, 5, 6, 7, 8, 9, 10,
  11, 12, 13, 14, 15, 16, 17, 18, 19, 20,
  21, 22, 23, 24, 25, 26, 27, 28, 29, 30,
  31, 32, 33, 34, 35, 36, 37, 38, 39, 40,
];
let sum = 0;
for (let p = 0; p < 20000; p++) {
  for (let i = 0; i < a.length; i++) {
    sum += a[i];
  }
}
console.log(sum);

// E7 kernel: elementwise SIMD transform — out[i] = a[i] + b[i].
// The vector loop pairs two lanes per iteration; V8 must do the same
// work through bounds-checked JS array loads/stores.
// @own
let a = [
  1, 2, 3, 4, 5, 6, 7, 8, 9, 10,
  11, 12, 13, 14, 15, 16, 17, 18, 19, 20,
  21, 22, 23, 24, 25, 26, 27, 28, 29, 30,
  31, 32, 33, 34, 35, 36, 37, 38, 39, 40,
];
// @own
let b = [
  100, 200, 300, 400, 500, 600, 700, 800, 900, 1000,
  1100, 1200, 1300, 1400, 1500, 1600, 1700, 1800, 1900, 2000,
  2100, 2200, 2300, 2400, 2500, 2600, 2700, 2800, 2900, 3000,
  3100, 3200, 3300, 3400, 3500, 3600, 3700, 3800, 3900, 4000,
];
// @own
let out = [
  0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
  0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
  0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
  0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
];
for (let p = 0; p < 20000; p++) {
  for (let i = 0; i < a.length; i++) {
    out[i] = a[i] + b[i];
  }
}
let checksum = 0;
for (let i = 0; i < out.length; i++) {
  checksum += out[i];
}
console.log(checksum);

// E7 kernel (i32 lanes): elementwise transform on i32[] arrays —
// out[i] = xs[i] + ys[i]. Four i32 lanes per v128; the running bound
// is the precomputed end address. V8 does the same work through
// bounds-checked packed-SMI array loads/stores.
// @own
let xs: i32[] = [
  1, 2, 3, 4, 5, 6, 7, 8, 9, 10,
  11, 12, 13, 14, 15, 16, 17, 18, 19, 20,
  21, 22, 23, 24, 25, 26, 27, 28, 29, 30,
  31, 32, 33, 34, 35, 36, 37, 38, 39, 40,
];
// @own
let ys: i32[] = [
  100, 200, 300, 400, 500, 600, 700, 800, 900, 1000,
  1100, 1200, 1300, 1400, 1500, 1600, 1700, 1800, 1900, 2000,
  2100, 2200, 2300, 2400, 2500, 2600, 2700, 2800, 2900, 3000,
  3100, 3200, 3300, 3400, 3500, 3600, 3700, 3800, 3900, 4000,
];
// @own
let out: i32[] = [
  0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
  0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
  0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
  0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
];
for (let p = 0; p < 20000; p++) {
  for (let i = 0; i < xs.length; i++) {
    out[i] = xs[i] + ys[i];
  }
}
let checksum = 0;
for (let i = 0; i < out.length; i++) {
  checksum += out[i];
}
console.log(checksum);

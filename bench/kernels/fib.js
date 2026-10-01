// E1 kernel: recursive function calls. Same program on every engine;
// sizes tuned so the slowest engine (tree-walker) finishes in ~1s.
function fib(n) {
  if (n < 2) {
    return n;
  }
  return fib(n - 1) + fib(n - 2);
}
let acc = 0;
for (let i = 0; i < 20; i++) {
  acc += fib(21);
}
console.log(acc);

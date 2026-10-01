// E5 kernel: function call overhead.
function add(a, b) {
  return a + b;
}
let acc = 0;
for (let i = 0; i < 200000; i++) {
  acc += add(i, 1);
}
console.log(acc);

// E1 kernel: loop + arithmetic throughput.
let acc = 0;
for (let i = 0; i < 300000; i++) {
  acc += ((i * 3) - (i % 7)) % 1000;
}
console.log(acc);

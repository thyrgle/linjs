// E6 kernel: integer arithmetic + bitwise mix on i32 locals. Mixed
// i32/number arithmetic is a dialect error, so the loop variable is
// typed too.
let acc: i32 = 0;
let key: i32 = 0x9E3779B9;
for (let i: i32 = 0; i < 500000; i++) {
  acc = (acc + i) ^ key;
  acc = (acc << 3) | (acc >>> 29);
}
console.log((acc >>> 0) % 1000);

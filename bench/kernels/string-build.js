// E1 kernel: string concatenation (arena copies in wasm).
let s = "";
for (let i = 0; i < 300; i++) {
  s = s + "x";
}
console.log(s.length);

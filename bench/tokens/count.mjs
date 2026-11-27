// Token counts with the o200k_base tokenizer (GPT-4o family). Claude's
// tokenizer is not public; o200k is a widely used, reproducible stand-in,
// and the comparison is relative.
//
//   node count.mjs file...      prints "<tokens>\t<file>" per file
import { readFileSync } from "node:fs";
import { encode } from "gpt-tokenizer/encoding/o200k_base";

for (const file of process.argv.slice(2)) {
  const text = readFileSync(file === "-" ? 0 : file, "utf8");
  console.log(`${encode(text).length}\t${file}`);
}

// Checks that the `lib.ar` code blocks of the layout-generator guide, joined
// by blank lines, are exactly examples/sky130_latch/lib.ar, which CI compiles
// and the guide walks through. Run it from docs/ with `node scripts/guide.mjs`;
// it exits 1 and names the first differing line when they disagree.
import {readFileSync} from 'node:fs';
import {join} from 'node:path';

const docs = join(import.meta.dirname, '..');
const guide = readFileSync(join(docs, 'docs', 'guides', 'layout-generator.md'), 'utf8');
const example = readFileSync(join(docs, '..', 'examples', 'sky130_latch', 'lib.ar'), 'utf8');

const blocks = [...guide.matchAll(/```argon title="lib\.ar"\n([\s\S]*?)\n```/g)].map((m) => m[1]);
const joined = `${blocks.join('\n\n')}\n`;
if (joined === example) {
  console.log(`the guide's ${blocks.length} lib.ar blocks match examples/sky130_latch/lib.ar`);
  process.exit(0);
}
const [a, b] = [joined.split('\n'), example.split('\n')];
const line = a.findIndex((text, i) => text !== b[i]);
console.error(
  `the guide's lib.ar blocks differ from examples/sky130_latch/lib.ar at line ${line + 1}:\n` +
    `  guide:   ${a[line] ?? '(end)'}\n  example: ${b[line] ?? '(end)'}`,
);
process.exit(1);

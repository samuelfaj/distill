// Check one converted module: node check.mjs lib/NAME.js
import { readFileSync } from 'node:fs';
import path from 'node:path';
import { pathToFileURL } from 'node:url';

const pkg = JSON.parse(readFileSync(new URL('./package.json', import.meta.url), 'utf8'));
if (pkg.type !== 'module') {
  console.error('package.json does not declare "type": "module" yet');
  process.exit(1);
}
const file = process.argv[2];
if (!file) {
  console.error('usage: node check.mjs lib/NAME.js');
  process.exit(2);
}
const loaded = await import(pathToFileURL(path.resolve(file)).href);
const names = Object.keys(loaded).sort();
if (!names.length) {
  console.error(`${file}: no exports`);
  process.exit(1);
}
console.log(`ok ${file}: ${names.join(', ')}`);

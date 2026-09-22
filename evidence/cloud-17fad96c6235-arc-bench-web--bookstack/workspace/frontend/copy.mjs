import { mkdirSync, copyFileSync, readdirSync, statSync, existsSync } from 'fs';
import { join, dirname } from 'path';
import { fileURLToPath } from 'url';

const here = dirname(fileURLToPath(import.meta.url));
const src = join(here, 'src');
const out = join(here, 'dist');

mkdirSync(out, { recursive: true });

function copyDir(from, to) {
  mkdirSync(to, { recursive: true });
  for (const entry of readdirSync(from)) {
    const fromPath = join(from, entry);
    const toPath = join(to, entry);
    if (statSync(fromPath).isDirectory()) {
      copyDir(fromPath, toPath);
    } else {
      copyFileSync(fromPath, toPath);
    }
  }
}

if (existsSync(src)) {
  copyDir(src, out);
  console.log('frontend build complete -> ' + out);
} else {
  console.warn('no src directory found at ' + src);
}

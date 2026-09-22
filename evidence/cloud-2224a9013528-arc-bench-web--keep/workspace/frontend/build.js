/* Tiny Node copy script: copy every file in src/ into dist/ verbatim. */
const fs = require('fs');
const path = require('path');

const srcDir = path.join(__dirname, 'src');
const distDir = path.join(__dirname, 'dist');

fs.mkdirSync(distDir, { recursive: true });

for (const name of fs.readdirSync(srcDir)) {
  const src = path.join(srcDir, name);
  const stat = fs.statSync(src);
  if (!stat.isFile()) continue;
  fs.copyFileSync(src, path.join(distDir, name));
}

console.log('Built frontend/dist from frontend/src');

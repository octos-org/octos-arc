const http = require('http'); const fs = require('fs'); const path = require('path');
const dist = path.join(__dirname, '..', 'frontend', 'dist');
const handler = (req, res) => {{ try {{
  const url = req.url.split('?')[0];
  const name = url === '/' ? 'index.html' : url.replace(/^\//, '');
  const candidates = [name, name + '.html'].map(n => path.join(dist, n));
  const file = candidates.find(f => f.startsWith(dist) && fs.existsSync(f) && fs.statSync(f).isFile());
  if (!file) {{ res.writeHead(404); return res.end('not found'); }}
  const type = file.endsWith('.js') ? 'application/javascript' : file.endsWith('.css') ? 'text/css' : 'text/html; charset=utf-8';
  res.writeHead(200, {{ 'Content-Type': type }}); res.end(fs.readFileSync(file));
}} catch (e) {{ res.writeHead(500); res.end('error'); }} }};
http.createServer(handler).listen(process.env.PORT || {port});
if (process.env.ARC_EXTRA_PORTS !== '0') for (const p of {extra_ports}) if (String(p) !== String(process.env.PORT || {port})) http.createServer(handler).listen(p);
process.on('uncaughtException', () => {{}}); process.on('unhandledRejection', () => {{}});

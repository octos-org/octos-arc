// Seed backend for the initial workspace. Zero dependencies on purpose: the
// container's route to npmjs is slow, so the template -- and every app grown
// from it -- runs on plain `node`. Implementation turns keep this
// architecture (CommonJS require, process.env.PORT, ../frontend/dist) and
// add the task's API routes and persistence on top of it.
"use strict";

const http = require("http");
const fs = require("fs");
const path = require("path");

const DIST_DIR = path.resolve(__dirname, "..", "frontend", "dist");
const PORT = Number(process.env.PORT || 3000);

const MIME_TYPES = {
  ".css": "text/css; charset=utf-8",
  ".html": "text/html; charset=utf-8",
  ".ico": "image/x-icon",
  ".js": "text/javascript; charset=utf-8",
  ".json": "application/json; charset=utf-8",
  ".png": "image/png",
  ".svg": "image/svg+xml",
};

function send(res, status, body) {
  res.writeHead(status, {
    "Content-Type": "text/plain; charset=utf-8",
    "Content-Length": Buffer.byteLength(body),
  });
  res.end(body);
}

function serveNext(req, res, rels, i) {
  if (i >= rels.length) {
    send(res, 404, "not found\n");
    return;
  }
  const file = path.normalize(path.join(DIST_DIR, rels[i]));
  if (file !== DIST_DIR && !file.startsWith(DIST_DIR + path.sep)) {
    send(res, 404, "not found\n");
    return;
  }
  fs.readFile(file, (err, data) => {
    if (err) {
      serveNext(req, res, rels, i + 1);
      return;
    }
    const type = MIME_TYPES[path.extname(file).toLowerCase()] || "application/octet-stream";
    res.writeHead(200, { "Content-Type": type, "Content-Length": data.length });
    res.end(req.method === "HEAD" ? undefined : data);
  });
}

const server = http.createServer((req, res) => {
  if (req.method !== "GET" && req.method !== "HEAD") {
    send(res, 405, "method not allowed\n");
    return;
  }
  let pathname;
  try {
    pathname = decodeURIComponent(new URL(req.url, "http://localhost").pathname);
  } catch {
    send(res, 400, "bad request\n");
    return;
  }
  // Static frontend only: "/" serves index.html, "/<name>" serves
  // <name>.html when it exists -- extensionless aliases are never created,
  // they would shadow HTML routes with a binary MIME type. Anything the
  // frontend does not contain stays the app's business (API routes) and
  // 404s until codegen adds it.
  const rel = pathname === "/" ? "index.html" : pathname.replace(/^\/+/, "");
  serveNext(req, res, path.extname(rel) ? [rel] : [rel + ".html", rel], 0);
});

server.listen(PORT, () => {
  console.log(`backend listening on port ${PORT}, serving ${DIST_DIR}`);
});

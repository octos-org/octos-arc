# Web Template

The initial workspace the platform lays out from the submission bundle and
hands to `main.py` (`ARCBENCH_TEMPLATE_DIR`). Zero-dependency by design: the
container's route to npmjs is slow, so nothing here may require
`npm install`.

- `frontend/src/`: hand-written HTML/CSS/JS sources. `npm run build`
  (frontend/package.json) copies `src/*` to `frontend/dist/` verbatim.
- `backend/`: `npm start` runs `server.js`, a CommonJS Node http server with
  no dependencies. It serves `../frontend/dist` (`index.html` for `/`,
  `<name>.html` for `/<name>`) on `process.env.PORT` and answers 404 for
  anything else.
- The two `package.json` manifests are byte-identical to the ones
  `write_codegen_manifests()` in main.py writes, so codegen turns keep them
  as-is and add the task's API routes and persistence in `backend/server.js`.

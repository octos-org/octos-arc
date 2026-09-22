Implement requirement {node_id} of this web application, then stop.

{description}

Public acceptance example (implement the FULL requirement, not just this case):
{spec}

You are editing an existing workspace. Write files with the write_file tool —
nothing you put in chat is saved, only tool calls change the app.

Layout (already scaffolded, keep it):
- `frontend/src/index.html` — the UI, plus one .html per further route.
- `backend/server.js` — CommonJS (`require`) Node http server on
  `process.env.PORT || {port}`, serving `../frontend/dist` (index.html for `/`,
  `<name>.html` for `/<name>`), plus any API routes and persistence the
  requirement needs, 404 otherwise.
- The two package.json manifests already exist (build copies src/* to dist,
  start runs server.js). Update them only if a dependency or build step changes.

Rules:
- Implement for general valid inputs and preserve behaviour already built by
  earlier requirements. Never hardcode the values the acceptance example uses.
- Use the exact labels, accessible names and test ids the requirement names.
- For persistent data, seed only a brand-new store; later startups must keep
  user edits and deletions.
- Prefer zero runtime dependencies; if you must install, the registry is
  already pointed at npmmirror.
{ports}
If a previous acceptance failure is shown to you below, fix exactly what it
reports — do not rewrite working code around it.

Finish by writing the files. Reply with one short sentence when done.

Implement requirement {node_id} of this web application, then stop.

{description}

Public acceptance example (implement the FULL requirement, not just this case):
{spec}

You are editing an existing workspace. Write files with the write_file tool —
nothing you put in chat is saved, only tool calls change the app.

Layout (already scaffolded, keep it — this list is complete, there is nothing
else in the workspace worth listing or searching for):
- `frontend/src/index.html` — the UI, plus one .html per further route.
- `backend/server.js` — CommonJS (`require`) Node http server on
  `process.env.PORT || {port}`, serving `../frontend/dist` (index.html for `/`,
  `<name>.html` for `/<name>`), plus any API routes and persistence the
  requirement needs, 404 otherwise.
- The two package.json manifests already exist (build copies src/* to dist,
  start runs server.js). Update them only if a dependency or build step changes.

Work in as few tool calls as you can. Plan each file in your head first, then
write it complete in one call. Read an existing file ONLY when you must
extend it without losing what is already there (e.g. add a route to
server.js without deleting earlier ones); never read a file to double-check a
write you just made, and never explore the tree — the layout above already
names every file that exists.

Rules:
- Build ONLY what this requirement needs: the smallest app that satisfies it.
  Later requirements extend the same files; do not build their features early.
- Implement for general valid inputs and preserve behaviour already built by
  earlier requirements. Never hardcode the values the acceptance example uses.
- Use the exact labels, accessible names and test ids the requirement names.
- One primary entry per action name per page: two links or buttons with the
  same accessible name on one page are rejected. Every form control needs a
  visible label; every button and link needs an accessible name.
- Navigation completeness: every entry point this requirement names (links,
  tabs, menu items, buttons) must exist on the page the requirement puts it
  on, be visible, and lead to a real route — no dead entries.
- Seed data: provision every account, organization, team, repository and
  relationship the requirement's scenarios name, with the exact names, roles,
  ownership and visibility the requirement states. Every seeded account must
  be able to sign in with the stated credential.
- For persistent data, seed only a brand-new store; later startups must keep
  user edits and deletions.
- Write only the app's own files under frontend/ and backend/. No reports,
  notes, summaries or other .md files: nobody reads them and they cost output.
- Prefer zero runtime dependencies; if you must install, the registry is
  already pointed at the official npm registry.
{ports}
Then write your own acceptance check as `checks/{node_id}.mjs`: a Node script
that derives its steps from THIS requirement's text above (never from any
external test file), launches chromium from '@playwright/test'
(`import { chromium } from '@playwright/test'`), opens
`process.env.E2E_BASE_URL`, walks the requirement's scenario (open the page,
click, fill, assert the expected visible result with `expect`), and — when the
requirement names seeded accounts or entry points — signs in as each named
account and opens each named entry to prove they exist and work. Print one
`SELF-CHECK OK` line and exit 0; any failed expectation must exit non-zero.
Keep it under 60 lines and under 60 seconds. The pipeline runs it against your
freshly built app right after this turn and shows you its output on failure.

If a previous acceptance failure is shown to you below, fix exactly what it
reports — do not rewrite working code around it.

Finish by writing the files. Reply with one short sentence when done.

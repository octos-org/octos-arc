Implement requirement {node_id} of this web application, then stop.

{description}

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
- The description is the requirement; the scenarios are only examples. Make a
  checklist of every concrete rule the description states — each exact text
  and error message, HTTP status code, redirect, default value, and who may or
  may not see or do something (including visitors who are not signed in) —
  and implement every item, including the ones no scenario mentions.
- Build ONLY what this requirement needs: the smallest app that satisfies it.
  Later requirements extend the same files; do not build their features early.
- Keep every page, route, label and accessible name that already exists (see
  the application context) exactly as it is: extend, never rename — e.g. do
  not add an aria-label that changes an existing control's accessible name.
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
- When a computed value and its error/sentinel share a representation (e.g.
  both are plain strings), tag or wrap them so chained computations can tell
  a real value from an error marker — never detect "is this an error" by
  checking typeof alone, or a valid downstream value gets mistaken for an
  upstream failure.
- File contents shown to you may hold a `[...-redacted]` placeholder where a
  value (a hash, a key, a long id) was masked: never write one back. Do not
  hand-edit runtime data files (e.g. a JSON store); seed data lives in code.
- Write only the app's own files under frontend/ and backend/. No reports,
  notes, summaries or other .md files: nobody reads them and they cost output.
- Prefer zero runtime dependencies; if you must install, the registry is
  already pointed at the official npm registry.
Then write your own acceptance check as `checks/{node_id}.mjs`: a Node script
that derives its steps from THIS requirement's text above (never from any
external test file). Start it with
`import { chromium, expect } from '@playwright/test'` — that import resolves
as-is and the browser is installed, so write no module-resolution or
browser-discovery code. It opens `process.env.E2E_BASE_URL` and walks EVERY
scenario the requirement lists, in order: for each GIVEN/WHEN/THEN it opens
the named page, clicks, fills, and asserts the expected visible result with
`expect`. Rules for the check:
- Assert exactly what the requirement states: the headings, labels, button and
  link names and texts it quotes, found by role and exact name
  (`getByRole(role, { name, exact: true })`, `getByLabel`, `getByText`), the
  routes it names, the redirects and error messages it describes. Assert
  nothing it does not state (page structure, element counts, styling), and
  never compare a whole list item's text for equality — it also holds
  buttons and other text; check that it contains the expected text.
- Stored data survives between runs of the check: create your own records
  with a unique suffix (e.g. `Date.now()`), never assume an empty store or an
  exact global count; compare before and after instead.
- When the requirement names seeded accounts or entry points, sign in as each
  named account and open each named entry to prove they exist and work.
- After the scenarios, also check every other concrete rule the description
  states: each exact text or error message, each HTTP status code (the
  response of `page.goto`), each redirect, each default value, and each rule
  on who may or may not see or do something — sign in as each role the rule
  names (and as no one, for visitors) and assert that what must be absent is
  absent.
- Also re-check briefly the earlier behaviour this requirement relies on or
  touches, as the application context states it (same pages, same names).
Print one `SELF-CHECK OK` line per scenario and exit 0; any failed
expectation must exit non-zero. Keep the whole script under 120 lines and
under 4 minutes.
The pipeline runs it against your freshly built app right after this turn and
shows you its output on failure.

If a previous acceptance failure is shown to you below, fix exactly what it
reports — do not rewrite working code around it.
The acceptance also reruns some self-checks kept from earlier stages
(other files in checks/): if one fails, your change broke earlier behaviour —
fix the app; only if that check contradicts the current requirements,
correct the check.

Finish by writing the files. Reply with one short sentence when done.

Requirement {node_id}: {description}

Public acceptance example (implement the full requirement):
{spec}
{files_intro} frontend/src/index.html (+ one html per further route); backend/server.js = CommonJS (require) Node http server on process.env.PORT||{port} serving ../frontend/dist files (index.html for /, <name>.html for /<name>) plus any API routes and persistence the requirement needs, 404 for anything else, handling request errors without hiding unexpected process failures.{ports} Initial package.json files already exist (build copies src/* to dist; start runs server.js). Preserve existing architecture; update manifests when required by dependencies or build changes.
For persistent data, initialize required records only for a new store or an explicit migration. Later startups must preserve user edits, deletions and archive state; a missing record does not mean the store is new. Reset data only when the requirements explicitly demand it.
Rules: implement the requirement for general valid inputs and preserve existing behavior. Use required labels and accessible controls, with unique IDs and correct label associations. Derive storage, rendering, styling and validation from the task; do not hardcode test outputs. Return only requested file blocks. {size_rule}

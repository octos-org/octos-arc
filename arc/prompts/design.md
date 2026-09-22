Design — do NOT implement yet — requirement node {node_id} of the web application in the current directory.

{node_spec}
{ancestors}
{tests}
Read the acceptance spec files for this node in full and the existing code they will exercise. Then write ONE JSON object (at most 80 lines) to the file .arc/design/{node_id}.json AND repeat it in your reply inside a ```json fence. Shape:
{{"routes": [{{"method": "POST", "path": "/api/...", "request": {{}}, "response": {{}}, "errors": []}}],
 "pages": [{{"path": "/...", "elements": [{{"role": "textbox|button|link|combobox|checkbox|radio|alert", "name": "exact accessible name", "notes": ""}}]}}],
 "data_model": {{"collection": {{"field": "type"}}}},
 "files": ["backend/server.js", "frontend/src/..."],
 "notes": "validation rules, session handling, seed data, performance decisions"}}
Copy every accessible name verbatim from the specs. This is a reading turn: use only file reading, listing and grep — no builds, servers, curl or other shell commands — and do not create or modify any other file.
The finished web application was run against its FULL acceptance suite. The
failing tests and their errors are shown below. Fix the application so they
pass, without breaking the tests that already pass.

You are editing an existing workspace (`frontend/src/*.html`, `backend/server.js`
serving on `process.env.PORT || {port}`). Read the files a failure points at
before changing them. Write files with the write_file / edit_file tools —
nothing you put in chat is saved.

Rules:
- Fix the cause in the application code; never special-case test data.
- Keep every label, accessible name, test id and route that passing tests use.
- If the output below shows no failing test, reply "nothing to fix" and stop.
{ports}
Finish by writing the files. Reply with one short sentence when done.

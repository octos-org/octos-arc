The app failed the pre-grading startup rehearsal. The runner executes exactly:
1. cd frontend && npm install && npm run build   (must exit 0)
2. cd backend && npm install && npm start        (must bind PORT and stay up)
Rehearsal error:
{error}
Fix the project so this sequence works (typical causes: a require() path that does not match a real file, a file referenced but never written, a startup syntax error, a dependency missing from package.json). Verify: build the frontend, start the backend with `ARC_EXTRA_PORTS=0 PORT={smoke} npm start`, confirm it binds, stop it. Never bind {port}. Write the fix now.
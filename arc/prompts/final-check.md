Final end-to-end check of the web application in the current directory:
1. `npm run build` in frontend/ — fix any error.
2. Kill leftover servers, start the backend with `ARC_EXTRA_PORTS=0 PORT={smoke} npm start`, confirm `curl http://127.0.0.1:{smoke}/` serves the app and every API endpoint answers (success and error cases).
3. Audit required flows and states against the contracts below. Check accessible names, unique IDs and correct label associations. Resolve observed locator ambiguity in its intended scope; repeated text and destinations can be legitimate.
{tests}
{ui}{performance}
{port_rules}
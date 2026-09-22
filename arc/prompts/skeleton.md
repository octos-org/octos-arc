Build the skeleton of a full-stack web application in the current working directory. The requirement tree is at {req_dir} (skim it; individual features come in later turns).

{architecture_contract}
{tests}
Steps: create frontend/ and backend/ as specified with a home page and a health endpoint, seed the JSON store, run `npm run build` in frontend/, start the backend with `ARC_EXTRA_PORTS=0 PORT={smoke} npm start`, `curl http://127.0.0.1:{smoke}/` to confirm the page is served, then stop it.
{port_rules}
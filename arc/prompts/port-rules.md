Ports: run your own smoke servers ONLY with `ARC_EXTRA_PORTS=0 PORT={smoke} npm start` (port {smoke}). NEVER bind port {port} — the runner watches it and terminates the run. Stop every server you started before you finish. Do not run git; the harness commits.
For a background server in a one-shot shell, redirect the entire command group, including stdin, so descendants cannot hold the tool's capture pipes open:
```sh
(cd backend && exec env ARC_EXTRA_PORTS=0 PORT={smoke} npm start) < /dev/null > smoke-server.log 2>&1 &
echo $!
```
Set the shell tool workdir to the application directory and run this command unchanged. Keep every cd inside the redirected parentheses; prepending cd ... && outside them creates another background shell that retains the capture pipes. Retain the printed PID for cleanup; inspect smoke-server.log and confirm HTTP readiness before testing. Do not assume a successful background launch means the app is ready. Rebuild frontend/ after source changes. Stop your server before ending the turn so the harness can start its own.

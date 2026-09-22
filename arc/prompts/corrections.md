## identical_failure
Repeated attempts produced the same observed failure. Recheck the assumptions behind the repair: inspect expected and received values, preceding actions, locator scope, and actual application state. Change the cause supported by this evidence. Do not manufacture the expected output or bypass the underlying operation; preserve behavior for other inputs.
## regressions_restored
Your last two repairs made the tests worse; the harness restored frontend/ and backend/ to the best state ({best}/{total}). Start from that code.
## protected_restored
You changed official test/requirement files; the harness restored them: {files}. They are read-only ground truth — fix the app instead.
## layout_incomplete
Your turn ended without both frontend/package.json and backend/package.json (with `build` and `start` scripts) on disk; the harness could not even build the app. Create the missing files.
## implement_timed_out
Your implementation turn ran out of time; work in smaller steps and verify with curl early.
## parallel_suite
The full suite runs all spec files against one server; tests from different files must not interfere through shared server state (e.g. a value that every browser session shares). Keep persisted data only where the requirement demands persistence.
## evolution_regression
This node passed before this evolution round; the regression below must be fixed without removing the new behaviour.
## claim_without_verification
Your previous turn claimed completion without running any build, start or request command. Use the supplied isolated verification command before claiming success. If no verification entry is supplied, build and exercise the app in a disposable copy so validation does not change the delivered application's persistent data.
## repeated_error
You hit the same error {count} times in a row ({error}). Stop repeating the command; diagnose the root cause (read the file / port / path involved) and change approach.
## protected_writes
You modified protected files that must never change: {files}. Revert nothing yourself; only touch frontend/ and backend/ from now on.
## codegen_no_blocks
Your previous reply contained no file blocks, so nothing was written. Reply ONLY with `<<<FILE path>>>` ... `<<<END FILE>>>` blocks holding complete files: no prose, no markdown fences, no notes.

"""The local grader is the only way to score a generated app without spending a
competition run, so a wrong number from it is worse than no number at all."""
import importlib.util
import socket
import subprocess
import sys
import unittest
from pathlib import Path

SCRIPT = Path(__file__).resolve().parent.parent / "grade-local.py"
_spec = importlib.util.spec_from_file_location("grade_local", SCRIPT)
grade_local = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(grade_local)


class PortGuardTests(unittest.TestCase):
    def test_should_report_a_listening_port_as_taken(self):
        with socket.socket() as srv:
            srv.bind(("127.0.0.1", 0)); srv.listen(1)
            taken = srv.getsockname()[1]
            self.assertFalse(grade_local.port_is_free(taken))
        self.assertTrue(grade_local.port_is_free(taken))

    def test_should_refuse_to_grade_when_the_port_already_serves(self):
        """A crash used to leave the previous app listening, and the readiness
        probe only asked whether *something* answered -- so the next run graded
        the previous app under this app's name and printed a confident 0."""
        with socket.socket() as srv:
            srv.bind(("127.0.0.1", 0)); srv.listen(1)
            taken = srv.getsockname()[1]
            code = grade_local.main([str(Path(__file__).parent), "smoke--counter", str(taken)])
        self.assertEqual(code, 5)


class AppTreeTests(unittest.TestCase):
    def test_should_not_write_a_lock_file_into_the_graded_app(self):
        """Grading npm-installs inside the app it is about to score. Those runs
        wrote backend/package-lock.json and frontend/package-lock.json, the
        pre-test `git add -A` snapshot staged them, and the cleanup could no
        longer remove them -- so scoring a bookstack run added two files to the
        deliverable it claims never to change."""
        for cwd, command in grade_local.app_install_steps(Path("/app")):
            self.assertIn("--no-package-lock", command, f"{cwd} install writes a lock file")

    def test_should_still_build_the_frontend_and_install_the_backend(self):
        steps = dict((cwd.name, command) for cwd, command in grade_local.app_install_steps(Path("/app")))
        self.assertIn("npm run build", steps["frontend"])
        self.assertIn("npm install", steps["backend"])


class TeardownTests(unittest.TestCase):
    def test_should_stop_a_server_whose_process_group_cannot_be_signalled(self):
        """`killpg` raised PermissionError on macOS and killed the grader
        instead of the app, leaking the listener that poisons the next run."""
        proc = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(60)"])
        self.addCleanup(lambda: proc.poll() is None and proc.kill())
        original = grade_local.os.killpg

        def deny(*_a, **_k):
            raise PermissionError(1, "Operation not permitted")

        grade_local.os.killpg = deny
        try:
            grade_local.stop_server(proc)
        finally:
            grade_local.os.killpg = original
        self.assertIsNotNone(proc.poll(), "server survived a teardown that could not signal its group")

    def test_should_be_a_noop_for_an_already_finished_server(self):
        proc = subprocess.Popen([sys.executable, "-c", "pass"])
        proc.wait()
        grade_local.stop_server(proc)
        self.assertIsNotNone(proc.poll())


if __name__ == "__main__":
    unittest.main()

#!/usr/bin/env python3
"""Build and verify a disposable app copy; never reset the source application's data.

This isolates relative filesystem writes, not external databases or absolute paths.
Dependencies are installed in the copy rather than linked to the source.
"""
from __future__ import annotations

import argparse
import json
from pathlib import Path
import shutil
import socket
import tempfile

from acceptance import AcceptanceRunner, AppServer, RunSummary, failure_summaries


def copy_application(source: Path, destination: Path) -> None:
    def ignore(directory, names):
        excluded = {name for name in names if name in {'.git', '.arc', 'node_modules', '__pycache__'}}
        for name in set(names) - excluded:
            if (Path(directory) / name).is_symlink():
                raise ValueError(f'Cannot isolate symbolic link: {Path(directory) / name}')
        return excluded
    shutil.copytree(source, destination, ignore=ignore)


def verify(app: Path, tests: Path, playwright: Path, report_dir: Path,
           specs: list[str], workers: int = 1, timeout: int = 900) -> int:
    app, tests, playwright, report_dir = [p.resolve() for p in (app, tests, playwright, report_dir)]
    if report_dir == app or app in report_dir.parents:
        raise ValueError('Report directory must be outside the source application')
    report_dir.mkdir(parents=True, exist_ok=True)
    selected = specs or sorted(str(p.relative_to(tests)) for p in tests.rglob('*.spec.ts'))
    for spec in selected:
        candidate = (tests / spec).resolve()
        if tests not in candidate.parents or not candidate.is_file():
            raise ValueError(f'Spec must be a file inside the acceptance tree: {spec}')
    if not selected:
        raise ValueError('No acceptance specs found')
    with tempfile.TemporaryDirectory(prefix='octos-verify-') as directory:
        copy = Path(directory) / 'app'
        copy_application(app, copy)
        with socket.socket() as sock:
            sock.bind(('127.0.0.1', 0))
            port = sock.getsockname()[1]
        log = lambda message: print(message, flush=True)
        runner = AcceptanceRunner(playwright, tests, report_dir / 'prepared', log, workers=workers)
        server = AppServer(copy, port, log)
        try:
            error = server.build() or server.start()
            summary = RunSummary(error=error) if error else runner.run(
                selected, f'http://127.0.0.1:{port}', workers=workers, wall_timeout=timeout)
        finally:
            server.stop()
    result = {'passed': summary.passed, 'total': summary.total,
              'error': summary.error, 'failures': failure_summaries(summary)}
    (report_dir / 'summary.json').write_text(json.dumps(result, ensure_ascii=False, indent=2))
    print(f'{summary.passed} passed, {summary.total - summary.passed} failed', flush=True)
    if summary.error:
        print(summary.error, flush=True)
    if result['failures']:
        print(result['failures'], flush=True)
    print(f'Acceptance report: {report_dir}', flush=True)
    return 0 if not summary.error and summary.total > 0 and summary.passed == summary.total else 1


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ['app', 'tests', 'playwright']:
        parser.add_argument('--' + name, type=Path, required=True)
    parser.add_argument('--report-dir', type=Path)
    parser.add_argument('--spec', action='append', default=[])
    parser.add_argument('--workers', type=int, default=1)
    parser.add_argument('--timeout', type=int, default=900)
    args = parser.parse_args()
    report_dir = args.report_dir or Path(tempfile.mkdtemp(prefix='octos-verify-report-'))
    return verify(args.app, args.tests, args.playwright, report_dir, args.spec, args.workers, args.timeout)


if __name__ == '__main__':
    raise SystemExit(main())

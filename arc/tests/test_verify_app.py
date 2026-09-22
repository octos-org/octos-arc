import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest


class IsolatedVerificationTests(unittest.TestCase):
    @unittest.skipUnless(os.environ.get('OCTOS_TEST_PLAYWRIGHT_ROOT'), 'requires installed Playwright')
    def test_should_preserve_source_data_and_report_real_browser_failure(self):
        with tempfile.TemporaryDirectory(prefix="verify app '") as directory:
            root = Path(directory)
            app, tests = root / 'app', root / 'tests'
            tests.mkdir()
            for name in ['frontend', 'backend']:
                (app / name).mkdir(parents=True)
                (app / name / 'package.json').write_text(json.dumps({'scripts': {'build': 'node -e "0"', 'start': 'node server.js'}}))
            data = app / 'backend/store.json'
            data.write_text('{"count":7}')
            (app / 'backend/server.js').write_text('''const fs=require('fs');require('http').createServer((req,res)=>{
if(req.url==='/change'){fs.writeFileSync('store.json','{"count":99}');res.end('changed');}
else {res.end('ready');}}).listen(process.env.PORT);''')
            (tests / "generic one's.spec.ts").write_text('''import {test,expect} from '@playwright/test';
test('real mutation followed by failure',async({request})=>{
 const response=await request.get('/change');expect(await response.text()).toBe('changed');
 expect(1).toBe(2);
});''')
            command = [sys.executable, str(Path(__file__).parents[1] / 'verify_app.py'),
                       '--app', str(app), '--tests', str(tests), '--playwright', os.environ['OCTOS_TEST_PLAYWRIGHT_ROOT'],
                       '--report-dir', str(root / 'report'), '--spec', "generic one's.spec.ts"]
            result = subprocess.run(command, capture_output=True, text=True, timeout=90)
            self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
            self.assertEqual(data.read_text(), '{"count":7}')
            summary = json.loads((root / 'report/summary.json').read_text())
            self.assertEqual((summary['passed'], summary['total']), (0, 1))
            self.assertIn('Expected: 2', summary['failures'])
            self.assertIn('1 failed', result.stdout)
            spec = tests / "generic one's.spec.ts"
            spec.write_text(spec.read_text().replace('expect(1).toBe(2)', 'expect(1).toBe(1)'))
            passed = subprocess.run(command, capture_output=True, text=True, timeout=90)
            self.assertEqual(passed.returncode, 0, passed.stdout + passed.stderr)
            self.assertEqual(data.read_text(), '{"count":7}')
            self.assertIn('1 passed, 0 failed', passed.stdout)

    def test_should_reject_links_instead_of_sharing_application_data(self):
        from verify_app import copy_application
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            app = root / 'app'
            app.mkdir()
            external = root / 'data.json'
            external.write_text('untouched')
            (app / 'data.json').symlink_to(external)
            with self.assertRaisesRegex(ValueError, 'symbolic link'):
                copy_application(app, root / 'copy')
            self.assertEqual(external.read_text(), 'untouched')

    def test_should_reject_reports_inside_source(self):
        from verify_app import verify
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            with self.assertRaisesRegex(ValueError, 'outside'):
                verify(root, root, root, root / 'report', [])


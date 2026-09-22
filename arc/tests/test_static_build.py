import json
import re
import subprocess
import tempfile
import unittest
from pathlib import Path
from main import CODEGEN_MANIFESTS

class StaticBuildTests(unittest.TestCase):
    def test_both_builds_preserve_nested_sources_without_stale_aliases(self):
        rust = (Path(__file__).resolve().parents[2]/'crates/octos-arc/src/codegen.rs').read_text()
        literal = re.search(r'pub const FRONTEND_MANIFEST: &str = ("(?:\\.|[^"\\])*");', rust).group(1)
        manifests = [CODEGEN_MANIFESTS['frontend/package.json'], json.loads(json.loads(literal))]
        for manifest in manifests:
            with self.subTest(manifest=manifest), tempfile.TemporaryDirectory() as d:
                root = Path(d)
                (root/'src/assets').mkdir(parents=True)
                (root/'src/profile.html').write_text('<h1>Profile</h1>')
                (root/'src/assets/style.css').write_text('body {color: blue}')
                (root/'dist').mkdir()
                (root/'dist/profile').write_text('stale alias')
                result = subprocess.run(manifest['scripts']['build'], shell=True, cwd=root, capture_output=True, text=True)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual((root/'dist/profile.html').read_text(), '<h1>Profile</h1>')
                self.assertTrue((root/'dist/assets/style.css').is_file())
                self.assertFalse((root/'dist/profile').exists())

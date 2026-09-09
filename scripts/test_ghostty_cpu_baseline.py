#!/usr/bin/env python3
"""Exercise the patched build script with a fake Zig, without native compilation."""
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]
SOURCE = ROOT / 'vendor/libghostty-vt-sys/build.rs'


class CpuBaseline(unittest.TestCase):
    def test_native_and_cross_builds_use_baseline_cpu(self):
        with tempfile.TemporaryDirectory(prefix='asd-cpu-test-') as temp:
            root = Path(temp)
            driver = root / 'build-script'
            subprocess.run(['rustc', '--edition=2024', str(SOURCE), '-o', str(driver)], check=True)
            source = root / 'source'
            source.mkdir()
            (source / 'build.zig').write_text('// Test source placeholder.\n')
            fake = root / 'zig'
            fake.write_text('''#!/usr/bin/env python3
import json, os, pathlib, sys
args = sys.argv[1:]
pathlib.Path(os.environ['ZIG_ARGUMENT_LOG']).write_text(json.dumps(args))
prefix = pathlib.Path(args[args.index('--prefix') + 1])
(prefix / 'lib').mkdir(parents=True, exist_ok=True)
(prefix / 'include/ghostty').mkdir(parents=True, exist_ok=True)
(prefix / 'include/ghostty/vt.h').touch()
for name in ['libghostty-vt.a', 'libghostty-vt.so', 'ghostty-vt-static.lib']:
    (prefix / 'lib' / name).touch()
''')
            fake.chmod(0o755)
            cases = [
                ('x86_64-unknown-linux-gnu', 'x86_64-unknown-linux-gnu', None),
                ('aarch64-apple-darwin', 'aarch64-apple-darwin', None),
                ('x86_64-pc-windows-msvc', 'x86_64-pc-windows-msvc', None),
                ('x86_64-unknown-linux-gnu', 'aarch64-unknown-linux-gnu', 'aarch64-linux-gnu'),
            ]
            for index, (host, target, zig_target) in enumerate(cases):
                with self.subTest(host=host, target=target):
                    out = root / str(index)
                    out.mkdir()
                    args_log = root / f'args-{index}.json'
                    env = os.environ.copy()
                    env.pop('DOCS_RS', None)
                    env.pop('GHOSTTY_ZIG_SYSTEM_DIR', None)
                    env.update(PATH=str(root) + os.pathsep + env['PATH'], HOST=host,
                               TARGET=target, OUT_DIR=str(out), DEBUG='false',
                               OPT_LEVEL='3', GHOSTTY_SOURCE_DIR=str(source),
                               LIBGHOSTTY_VT_SYS_OPTIMIZE='ReleaseFast', ZIG_ARGUMENT_LOG=str(args_log))
                    result = subprocess.run([str(driver)], env=env, capture_output=True, text=True)
                    self.assertEqual(result.returncode, 0, result.stderr)
                    args = json.loads(args_log.read_text())
                    self.assertEqual(args.count('-Dcpu=baseline'), 1, args)
                    self.assertIn('-Doptimize=ReleaseFast', args)
                    if zig_target:
                        self.assertIn('-Dtarget=' + zig_target, args)
                    else:
                        self.assertFalse(any(arg.startswith('-Dtarget=') for arg in args))


if __name__ == '__main__':
    unittest.main()

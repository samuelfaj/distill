"""Exercise the installer without downloading or replacing a user's executable."""
import hashlib
import os
from pathlib import Path
import subprocess
import tempfile
import unittest


class ReleaseInstallerTest(unittest.TestCase):
    def test_install_upgrade_and_reject_corrupt_download(self):
        script = Path(__file__).resolve().parents[1] / 'install.sh'
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            fake_bin = root / 'fake-bin'
            fake_bin.mkdir()
            curl = fake_bin / 'curl'
            curl.write_text('''#!/usr/bin/env python3
import os, pathlib, sys
args = sys.argv[1:]
url = next(a for a in args if a.startswith('https://'))
root = pathlib.Path(os.environ['FIXTURE'])
if url.endswith('/latest'):
    print('{"tag_name":"v2.0.0"}')
else:
    content = (root / ('checksums' if url.endswith('/SHA256SUMS') else 'binary')).read_bytes()
    pathlib.Path(args[args.index('-o') + 1]).write_bytes(content)
''')
            curl.chmod(0o755)
            os_name = 'macos' if os.uname().sysname == 'Darwin' else 'linux'
            arch = 'aarch64' if os.uname().machine in ('arm64', 'aarch64') else 'x86_64'
            asset = f'distill-{os_name}-{arch}'
            install = root / 'installation with spaces'
            env = dict(os.environ, PATH=f'{fake_bin}:{os.environ["PATH"]}',
                       FIXTURE=str(root), DISTILL_INSTALL_DIR=str(install))
            def fixture(version):
                binary = f'#!/bin/sh\necho "Distill {version}"\n'.encode()
                (root / 'binary').write_bytes(binary)
                (root / 'checksums').write_text(f'{hashlib.sha256(binary).hexdigest()}  {asset}\n')
            def run(version):
                return subprocess.run(['sh', str(script)], env=dict(env, DISTILL_VERSION=version),
                                      capture_output=True, text=True)
            fixture('2.0.0')
            self.assertEqual(run('latest').returncode, 0)
            current = install / 'bin/distill'
            self.assertIn('2.0.0', subprocess.check_output([current], text=True))
            self.assertEqual(run('2.0.0').returncode, 0)
            fixture('2.0.1')
            self.assertEqual(run('2.0.1').returncode, 0)
            self.assertIn('2.0.1', subprocess.check_output([current], text=True))
            previous_target = current.readlink()
            (root / 'binary').write_bytes(b'corrupted download')
            self.assertNotEqual(run('2.0.2').returncode, 0)
            self.assertEqual(current.readlink(), previous_target)
            self.assertIn('2.0.1', subprocess.check_output([current], text=True))

    def test_persists_path_in_shell_rc_once_and_honors_opt_out(self):
        script = Path(__file__).resolve().parents[1] / 'install.sh'
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            fake_bin = root / 'fake-bin'
            fake_bin.mkdir()
            curl = fake_bin / 'curl'
            curl.write_text('''#!/usr/bin/env python3
import os, pathlib, sys
args = sys.argv[1:]
url = next(a for a in args if a.startswith('https://'))
root = pathlib.Path(os.environ['FIXTURE'])
content = (root / ('checksums' if url.endswith('/SHA256SUMS') else 'binary')).read_bytes()
pathlib.Path(args[args.index('-o') + 1]).write_bytes(content)
''')
            curl.chmod(0o755)
            os_name = 'macos' if os.uname().sysname == 'Darwin' else 'linux'
            arch = 'aarch64' if os.uname().machine in ('arm64', 'aarch64') else 'x86_64'
            asset = f'distill-{os_name}-{arch}'
            binary = b'#!/bin/sh\necho "Distill 2.0.0"\n'
            (root / 'binary').write_bytes(binary)
            (root / 'checksums').write_text(f'{hashlib.sha256(binary).hexdigest()}  {asset}\n')
            home = root / 'home'
            home.mkdir()
            install = root / 'install'

            def run(shell, **extra):
                env = dict(os.environ, PATH=f'{fake_bin}:{os.environ["PATH"]}', FIXTURE=str(root),
                           HOME=str(home), SHELL=shell, DISTILL_INSTALL_DIR=str(install),
                           DISTILL_VERSION='2.0.0', **extra)
                env.pop('ZDOTDIR', None)
                env.pop('XDG_CONFIG_HOME', None)
                return subprocess.run(['sh', str(script)], env=env, capture_output=True, text=True)

            # A fresh terminal only sees the install if the rc file persists the PATH entry.
            export = f'export PATH="{install}/bin:$PATH"'
            self.assertEqual(run('/bin/zsh').returncode, 0)
            self.assertEqual((home / '.zshrc').read_text().count(export), 1)
            self.assertEqual(run('/bin/zsh').returncode, 0)
            self.assertEqual((home / '.zshrc').read_text().count(export), 1)

            self.assertEqual(run('/usr/bin/fish').returncode, 0)
            self.assertIn(f'fish_add_path "{install}/bin"',
                          (home / '.config/fish/config.fish').read_text())

            self.assertEqual(run('/bin/sh').returncode, 0)
            self.assertIn(export, (home / '.profile').read_text())

            out = run('/bin/bash', DISTILL_NO_MODIFY_PATH='1')
            self.assertEqual(out.returncode, 0)
            self.assertFalse((home / '.bashrc').exists() or (home / '.bash_profile').exists())
            self.assertIn('Add this directory to your shell PATH', out.stdout)


    def test_git_bash_hands_off_to_the_powershell_installer(self):
        script = Path(__file__).resolve().parents[1] / 'install.sh'
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            fake_bin = root / 'fake-bin'
            fake_bin.mkdir()
            uname = fake_bin / 'uname'
            uname.write_text('#!/bin/sh\ncase "$1" in -s) echo MINGW64_NT-10.0-26200 ;; *) echo x86_64 ;; esac\n')
            uname.chmod(0o755)
            record = root / 'powershell-args'
            powershell = fake_bin / 'powershell.exe'
            powershell.write_text(f'#!/bin/sh\nprintf "%s\\n" "$@" > {record}\n')
            powershell.chmod(0o755)
            env = dict(os.environ, PATH=f'{fake_bin}:{os.environ["PATH"]}')
            run = subprocess.run(['sh', str(script)], env=env, capture_output=True, text=True)
            self.assertEqual(run.returncode, 0, run.stderr)
            launched = record.read_text()
            self.assertIn('https://raw.githubusercontent.com/samuelfaj/distill/main/install.ps1', launched)
            self.assertIn('iex', launched)


if __name__ == '__main__':
    unittest.main()

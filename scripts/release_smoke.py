#!/usr/bin/env python3
"""Run a Linux release binary through real daemon, PTY, snapshot and TUI paths."""
import argparse
import json
import os
from pathlib import Path
import select
import struct
import subprocess
import tempfile
import time


def smoke(binary):
    # POSIX PTYs are intentional: this gate runs on the Linux compatibility host.
    import fcntl
    import pty
    import termios

    root = Path(tempfile.mkdtemp(prefix='asd-release-smoke-'))
    env = {k: v for k, v in os.environ.items() if not k.startswith('ASD_')}
    env.pop('BASH_ENV', None)
    env.pop('ENV', None)
    env.update(ASD_SOCKET=str(root / 'asd.sock'), XDG_DATA_HOME=str(root / 'data'),
               XDG_CONFIG_HOME=str(root / 'config'), SHELL='/bin/bash', TERM='xterm-256color')
    children = []
    master = None
    log = (root / 'daemon.log').open('wb')
    result = {'binary': str(binary), 'artifacts': str(root)}

    def cli(*args):
        command = subprocess.run([str(binary), *args], env=env, capture_output=True, timeout=15)
        if command.returncode:
            raise RuntimeError(f'{args}: exit {command.returncode}: {command.stderr.decode(errors="replace")}')
        return command.stdout

    def alive():
        for child in children:
            if child.poll() is not None:
                raise RuntimeError(f'{child.args[1]} exited {child.returncode}')

    def read_until(marker, timeout=15):
        data = bytearray()
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            alive()
            ready, _, _ = select.select([master], [], [], 0.1)
            if ready:
                chunk = os.read(master, 65536)
                if not chunk:
                    raise RuntimeError('TUI output closed')
                data.extend(chunk)
                if marker in data:
                    return
                if len(data) > 1024 * 1024:
                    del data[:-len(marker)]
        raise RuntimeError(f'TUI did not render {marker!r}')

    try:
        result['version'] = cli('--version').decode().strip()
        daemon = subprocess.Popen([str(binary), 'daemon'], env=env, stdout=log, stderr=log)
        children.append(daemon)
        deadline = time.monotonic() + 10
        while not (root / 'asd.sock').exists():
            alive()
            if time.monotonic() >= deadline:
                raise RuntimeError('daemon socket did not appear')
            time.sleep(0.05)
        cli('new', 'smoke', '--cwd', str(root), '--cmd', '/bin/bash --noprofile --norc')
        cli('send', 'smoke', '--text', "printf 'ASD_RELEASE_%s\\n' 'SMOKE_OK'", '--enter')
        cli('wait', 'smoke', '--text', 'ASD_RELEASE_SMOKE_OK', '--timeout', '5s')
        master, slave = pty.openpty()
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack('HHHH', 40, 130, 0, 0))
        try:
            tui = subprocess.Popen([str(binary), 'ui', 'smoke'], env=env,
                                   stdin=slave, stdout=slave, stderr=slave, start_new_session=True)
            children.append(tui)
        finally:
            os.close(slave)
        read_until(b'ASD_RELEASE_SMOKE_OK')
        result['snapshot_rendered'] = True
        os.write(master, b'\x01f')
        read_until(b'Workspace files')
        result['file_browser_rendered'] = True
        os.write(master, b'\x1b')
        fcntl.ioctl(master, termios.TIOCSWINSZ, struct.pack('HHHH', 32, 100, 0, 0))
        os.kill(tui.pid, __import__('signal').SIGWINCH)
        cli('send', 'smoke', '--text', "printf 'ASD_RESIZE_%s\\n' 'OK'", '--enter')
        read_until(b'ASD_RESIZE_OK')
        result['resize_and_output'] = True
        alive()
        result['passed'] = True
    except Exception as error:
        result.update(passed=False, error=str(error))
    finally:
        result['exit_states_before_cleanup'] = [child.poll() for child in children]
        for child in reversed(children):
            if child.poll() is None:
                child.terminate()
                try:
                    child.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    child.kill()
                    child.wait(timeout=5)
        if master is not None:
            os.close(master)
        log.close()
        (root / 'result.json').write_text(json.dumps(result, indent=2))
    return result


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('binary', type=Path)
    args = parser.parse_args()
    result = smoke(args.binary.resolve(strict=True))
    print(json.dumps(result, indent=2))
    raise SystemExit(0 if result['passed'] else 1)

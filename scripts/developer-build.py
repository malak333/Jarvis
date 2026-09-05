#!/usr/bin/env python3
"""Build/open the supervised developer app using an existing Windows SSH session."""
import argparse
import json
import os
from pathlib import Path
import plistlib
import re
import shutil
import subprocess
import tarfile
import tempfile
import time
import urllib.request

ROOT = Path(__file__).resolve().parents[1]
STATE = Path.home() / 'Library/Application Support/Assemblywright/Developer'
APP = ROOT / 'target/developer/Assemblywright Developer.app'


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--host', default='mike@100.64.23.14')
    parser.add_argument('--socket', default=str(Path.home() / '.ssh/assemblywright-codex-windows.sock'))
    parser.add_argument('--remote-root', default='C:/a/aw-developer-20260905')
    parser.add_argument('--model-controller', default=str(Path.home() / 'Antigravity/local-ai-mac/scripts/local-ai'))
    parser.add_argument('--build', action='store_true', help='Build and transfer current source before opening')
    parser.add_argument('--no-open', action='store_true', help='Prepare connections without opening the app')
    parser.add_argument('--stop', action='store_true', help='Stop the idle developer runner; retains projects and queue')
    args = parser.parse_args()
    if not re.fullmatch(r'[A-Za-z]:[/\\][A-Za-z0-9_/\\-]+', args.remote_root):
        raise SystemExit('Remote root must be a simple absolute Windows path without spaces.')
    remote = args.remote_root.replace('/', '\\').rstrip('\\')
    ssh = ['ssh', '-S', args.socket, '-o', 'BatchMode=yes', '-o', 'ConnectTimeout=10', args.host]
    subprocess.run(['ssh', '-S', args.socket, '-O', 'check', args.host], check=True)
    STATE.mkdir(parents=True, exist_ok=True)
    config_path = STATE / 'runtime.json'

    def request(action=None):
        config = json.loads(config_path.read_text())
        data = json.dumps({'action': action}).encode() if action else None
        req = urllib.request.Request(config['endpoint'] + ('/control' if action else '/status'), data=data, headers={'Authorization': 'Bearer ' + config['token'], 'Content-Type': 'application/json'})
        return json.load(urllib.request.urlopen(req, timeout=5))

    current = None
    try:
        current = request()
    except (OSError, ValueError):
        pass
    if args.stop or args.build:
        if current:
            if current['running']:
                raise SystemExit('Use Stop in the app and wait for the saved checkpoint before rebuilding or closing the runner.')
            request('shutdown')
            time.sleep(.5)
            current = None
        if args.stop:
            print('Developer runner stopped. Projects and queue are retained.')
            return

    if args.build:
        print('Building the Mac app and Windows runner…', flush=True)
        subprocess.run(['swift', 'build', '--disable-sandbox', '--package-path', str(ROOT / 'apps/mac'), '--product', 'AssemblywrightMacApp'], check=True)
        with tempfile.TemporaryDirectory(prefix='assemblywright-developer-build-') as temp:
            archive = Path(temp) / 'source.tar.gz'
            with tarfile.open(archive, 'w:gz') as output:
                for name in ['Cargo.toml', 'Cargo.lock', 'rust-toolchain.toml', 'crates']:
                    output.add(ROOT / name, arcname=name)
            subprocess.run(ssh + [f'if not exist {remote} mkdir {remote}'], check=True)
            subprocess.run(['scp', '-o', f'ControlPath={args.socket}', str(archive), f'{args.host}:{args.remote_root}/source.tar.gz'], check=True)
        subprocess.run(ssh + [f'cd /d {remote} && tar -xf source.tar.gz && cargo build -p assemblywright-master --bin assemblywright-developer && if not exist bin mkdir bin'], check=True)
        subprocess.run(ssh + [f'copy /Y {remote}\\target\\debug\\assemblywright-developer.exe {remote}\\bin\\assemblywright-developer.exe >NUL'], check=True)
        (APP / 'Contents/MacOS').mkdir(parents=True, exist_ok=True)
        replacement = APP / 'Contents/MacOS/.AssemblywrightMacApp.new'
        shutil.copy2(ROOT / 'apps/mac/.build/debug/AssemblywrightMacApp', replacement)
        replacement.replace(APP / 'Contents/MacOS/AssemblywrightMacApp')
        (APP / 'Contents/Info.plist').write_bytes(plistlib.dumps({
            'CFBundleIdentifier': 'com.nobiletechnology.assemblywright.developer', 'CFBundleExecutable': 'AssemblywrightMacApp',
            'CFBundleName': 'Assemblywright Developer', 'CFBundleDisplayName': 'Assemblywright Developer',
            'CFBundleVersion': '1', 'CFBundleShortVersionString': '0.1.4', 'CFBundlePackageType': 'APPL',
            'LSMinimumSystemVersion': '14.0', 'AssemblywrightDeveloperBuild': True,
        }))
        subprocess.run(['codesign', '--force', '--sign', '-', str(APP)], check=True)

    try:
        urllib.request.urlopen('http://127.0.0.1:8080/health', timeout=3).close()
    except OSError:
        print('Starting the configured local model…', flush=True)
        subprocess.run([args.model_controller, 'start'], check=True)
    for direction, specification in [('-L', '17796:127.0.0.1:7796'), ('-R', '18080:127.0.0.1:8080')]:
        subprocess.run(['ssh', '-S', args.socket, '-O', 'forward', direction, specification, args.host], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    subprocess.run(ssh + ['curl.exe --fail --silent --max-time 5 http://127.0.0.1:18080/health'], check=True, stdout=subprocess.DEVNULL)
    if not current:
        log = (STATE / 'windows-runner.log').open('ab')
        process = subprocess.Popen(ssh + [f'{remote}\\bin\\assemblywright-developer.exe --data-dir {remote}\\state --workspace-root {remote}\\projects --model-url http://127.0.0.1:18080/v1'], stdin=subprocess.DEVNULL, stdout=log, stderr=log, start_new_session=True)
        (STATE / 'ssh-runner.pid').write_text(str(process.pid))
        log.close()
    token_result = None
    for _ in range(30):
        token_result = subprocess.run(ssh + [f'type {remote}\\state\\developer-token'], capture_output=True, text=True)
        if token_result.returncode == 0 and token_result.stdout.strip():
            break
        time.sleep(.2)
    if token_result is None or token_result.returncode != 0:
        raise SystemExit('Windows runner did not start. See ' + str(STATE / 'windows-runner.log'))
    config_path.write_text(json.dumps({'endpoint': 'http://127.0.0.1:17796', 'token': token_result.stdout.strip()}))
    os.chmod(config_path, 0o600)
    for attempt in range(30):
        try:
            status = request()
            break
        except OSError:
            if attempt == 29:
                raise SystemExit('Windows runner is unavailable. See ' + str(STATE / 'windows-runner.log'))
            time.sleep(.2)
    print('Connected to ' + status['host'] + '. Projects: ' + status['workspace_root'])
    if not args.no_open:
        if not APP.exists():
            raise SystemExit('Run this command again with --build to create the Mac app.')
        subprocess.run(['open', str(APP)], check=True)
    print('Developer app: ' + str(APP))


if __name__ == '__main__':
    main()

#!/usr/bin/env python3
"""Disposable native HTTP/process coverage; the model response is a labeled fixture."""
import argparse
import http.server
import json
import os
from pathlib import Path
import socket
import subprocess
import sys
import tempfile
import threading
import time
import urllib.error
import urllib.request
import uuid


def process_alive(pid):
    if sys.platform == 'win32':
        import ctypes
        from ctypes import wintypes
        kernel = ctypes.WinDLL('kernel32', use_last_error=True)
        kernel.OpenProcess.argtypes = [wintypes.DWORD, wintypes.BOOL, wintypes.DWORD]
        kernel.OpenProcess.restype = wintypes.HANDLE
        kernel.WaitForSingleObject.argtypes = [wintypes.HANDLE, wintypes.DWORD]
        kernel.CloseHandle.argtypes = [wintypes.HANDLE]
        handle = kernel.OpenProcess(0x00100000, False, pid)
        if not handle:
            assert ctypes.get_last_error() == 87, 'Unable to inspect owned validation process'
            return False
        try:
            result = kernel.WaitForSingleObject(handle, 0)
            assert result in (0, 258), 'Unable to query owned validation process'
            return result == 258
        finally:
            kernel.CloseHandle(handle)
    result = subprocess.run(['ps', '-o', 'stat=', '-p', str(pid)], capture_output=True, text=True)
    return bool(result.stdout.strip()) and not result.stdout.strip().startswith('Z')


def wait_reaped(pids):
    deadline = time.monotonic() + 8
    while time.monotonic() < deadline:
        if not any(process_alive(pid) for pid in pids):
            return
        time.sleep(.05)
    raise AssertionError('Validation descendants survived termination: ' + str(pids))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary', required=True)
    args = parser.parse_args()
    calls = []

    class Model(http.server.BaseHTTPRequestHandler):
        def do_POST(self):
            calls.append(json.loads(self.rfile.read(int(self.headers['Content-Length']))))
            content = json.dumps({'files': [{'path': 'result.txt', 'content': 'real file from fixture model\n'}]})
            if len(calls) == 2:
                content = '```json\n' + content + '\n```'
            if len(calls) == 4:
                content = 'invalid JSON fixture'
            body = json.dumps({'choices': [{'message': {'content': content}}]}).encode()
            self.send_response(200)
            self.send_header('Content-Type', 'application/json')
            self.send_header('Content-Length', str(len(body)))
            self.end_headers()
            self.wfile.write(body)

        def log_message(self, *unused):
            pass

    model = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Model)
    threading.Thread(target=model.serve_forever, daemon=True).start()
    with socket.socket() as reservation:
        reservation.bind(('127.0.0.1', 0))
        port = reservation.getsockname()[1]
    with tempfile.TemporaryDirectory(prefix='assemblywright-developer-e2e-') as temp:
        root = Path(temp)
        data = root / 'state'
        projects = root / 'projects'
        command = [str(Path(args.binary).resolve()), '--data-dir', str(data), '--workspace-root', str(projects), '--bind', f'127.0.0.1:{port}', '--model-url', f'http://127.0.0.1:{model.server_port}/v1']
        output = (root / 'runner.log').open('wb')
        process = subprocess.Popen(command, stdin=subprocess.DEVNULL, stdout=output, stderr=output)
        token = ''

        def call(action=None, **values):
            body = json.dumps(dict(action=action, **values)).encode() if action else None
            req = urllib.request.Request(f'http://127.0.0.1:{port}/' + ('control' if action else 'status'), data=body, headers={'Authorization': 'Bearer ' + token, 'Content-Type': 'application/json'})
            return json.load(urllib.request.urlopen(req, timeout=5))

        def wait(predicate, timeout=20):
            deadline = time.monotonic() + timeout
            while time.monotonic() < deadline:
                try:
                    snapshot = call()
                    if predicate(snapshot):
                        return snapshot
                except (OSError, urllib.error.URLError):
                    pass
                time.sleep(.05)
            raise AssertionError('Timed out: ' + json.dumps(locals().get('snapshot')) + '\n' + (root / 'runner.log').read_text(errors='replace'))

        try:
            deadline = time.monotonic() + 15
            while not (data / 'developer-token').exists() and time.monotonic() < deadline:
                time.sleep(.05)
            token = (data / 'developer-token').read_text().strip()
            wait(lambda s: not s['running'])
            try:
                urllib.request.urlopen(f'http://127.0.0.1:{port}/status', timeout=3)
                raise AssertionError('Unauthenticated status accepted')
            except urllib.error.HTTPError as error:
                assert error.code == 401
            python = '"' + sys.executable + '"'
            validation = python + ' -c "import time; from pathlib import Path; time.sleep(2); assert Path(\'result.txt\').is_file()"'
            first_id = str(uuid.uuid4())
            feature = dict(id=first_id, project='first', instruction='Create a fixture result file', validation=validation)
            call('enqueue', **feature)
            assert len(call('enqueue', **feature)['queue']) == 1
            try:
                call('enqueue', **dict(feature, instruction='different request'))
                raise AssertionError('Changed duplicate request accepted')
            except urllib.error.HTTPError as error:
                assert error.code == 409
            for project in ['second', 'third']:
                call('enqueue', id=str(uuid.uuid4()), project=project, instruction='Create a fixture result file', validation=python + ' -c "from pathlib import Path; assert Path(\'result.txt\').is_file()"')
            call('auto_run', enabled=False)
            call('start')
            wait(lambda s: s['queue'][0]['checkpoint'] == 'applied' and s['running'])
            modified = (projects / 'first/result.txt').stat().st_mtime_ns
            started = time.monotonic()
            call('stop')
            paused = wait(lambda s: not s['running'], 5)
            assert paused['queue'][0]['status'] == 'paused'
            assert paused['queue'][0]['checkpoint'] == 'applied'
            stop_seconds = time.monotonic() - started
            process.terminate()
            process.wait(timeout=5)
            process = subprocess.Popen(command, stdin=subprocess.DEVNULL, stdout=output, stderr=output)
            restored_pause = wait(lambda s: not s['running'] and s['queue'][0]['status'] == 'paused')
            assert restored_pause['queue'][0]['checkpoint'] == 'applied'
            call('resume')
            wait(lambda s: s['running'])
            call('emergency')
            paused = wait(lambda s: not s['running'], 5)
            assert paused['emergency_paused']
            try:
                call('resume')
                raise AssertionError('Resume ignored Emergency Pause')
            except urllib.error.HTTPError as error:
                assert error.code == 409
            call('clear_emergency')
            call('resume')
            completed = wait(lambda s: not s['running'] and s['queue'][0]['status'] == 'succeeded')
            assert completed['queue'][1]['status'] == 'queued', completed
            assert len(calls) == 1, 'Resume repeated model generation'
            assert (projects / 'first/result.txt').stat().st_mtime_ns == modified, 'Resume rewrote applied files'
            call('auto_run', enabled=True)
            call('start')
            completed = wait(lambda s: not s['running'] and all(f['status'] == 'succeeded' for f in s['queue']))
            assert len(calls) == 3
            for project in ['malformed', 'must-wait']:
                call('enqueue', id=str(uuid.uuid4()), project=project, instruction='Fixture malformed-response boundary', validation=python + ' -c "raise SystemExit(0)"')
            call('start')
            failed = wait(lambda s: not s['running'] and s['queue'][3]['status'] == 'failed')
            assert failed['queue'][4]['status'] == 'queued'
            assert not (projects / 'malformed/result.txt').exists()
            assert len(calls) == 4
            process.terminate()
            process.wait(timeout=5)
            process = subprocess.Popen(command, stdin=subprocess.DEVNULL, stdout=output, stderr=output)
            restored = wait(lambda s: len(s['queue']) == 5)
            assert [f['status'] for f in restored['queue']] == ['succeeded'] * 3 + ['failed', 'queued']
            assert not restored['running']
            # Retry malformed generation, then exercise a real failed validation.
            call('resume')
            wait(lambda s: not s['running'] and all(f['status'] == 'succeeded' for f in s['queue']))
            call('enqueue', id=str(uuid.uuid4()), project='validation-failure', instruction='Fixture validation failure', validation=python + ' -c "from pathlib import Path; assert Path(\'fixed.txt\').exists()"')
            call('enqueue', id=str(uuid.uuid4()), project='after-failure', instruction='Must wait for validation success', validation=python + ' -c "raise SystemExit(0)"')
            call('start')
            failed_validation = wait(lambda s: not s['running'] and s['queue'][-2]['status'] == 'failed')
            assert failed_validation['queue'][-2]['checkpoint'] == 'applied'
            assert 'Validation failed' in failed_validation['queue'][-2]['message']
            assert failed_validation['queue'][-1]['status'] == 'queued'
            planned = len(calls)
            (projects / 'validation-failure/fixed.txt').write_text('owner repair')
            call('resume')
            wait(lambda s: not s['running'] and all(f['status'] == 'succeeded' for f in s['queue']))
            assert len(calls) == planned + 1, 'Validation retry regenerated applied changes'
            # A Start authorizes only the queue present at that moment.
            call('enqueue', id=str(uuid.uuid4()), project='batch-frontier', instruction='Bound the starting batch', validation=validation)
            call('start')
            wait(lambda s: s['running'] and s['queue'][-1]['checkpoint'] == 'applied')
            call('enqueue', id=str(uuid.uuid4()), project='future-work', instruction='Requires another Start', validation=python + ' -c "raise SystemExit(0)"')
            bounded = wait(lambda s: not s['running'])
            assert bounded['queue'][-2]['status'] == 'succeeded'
            assert bounded['queue'][-1]['status'] == 'queued'
            assert not (projects / 'future-work/result.txt').exists()
            call('start')
            wait(lambda s: not s['running'] and all(f['status'] == 'succeeded' for f in s['queue']))
            # Owned descendants must terminate on Stop/Emergency and Windows runner death.
            tree_script = root / 'validation-tree.py'
            tree_script.write_text("import json,os,subprocess,sys,time\nfrom pathlib import Path\nif Path('allow-validation').exists():\n    assert Path('result.txt').exists()\n    raise SystemExit(0)\nchild = subprocess.Popen([sys.executable, '-c', 'import time; time.sleep(30)'])\nPath('validation-pids.json').write_text(json.dumps([os.getpid(), child.pid]))\ntime.sleep(30)\n")
            call('enqueue', id=str(uuid.uuid4()), project='process-tree', instruction='Validate process ownership', validation=python + ' "' + str(tree_script) + '"')
            tree = projects / 'process-tree'
            pids_path = tree / 'validation-pids.json'

            def started_tree():
                wait(lambda s: s['running'] and pids_path.exists())
                # File creation and writing may be observed separately.
                for _ in range(100):
                    try:
                        return json.loads(pids_path.read_text())
                    except ValueError:
                        time.sleep(.01)
                raise AssertionError('Validation did not publish its process IDs')

            call('start')
            pids = started_tree()
            call('stop')
            wait(lambda s: not s['running'])
            wait_reaped(pids)
            pids_path.unlink()
            call('resume')
            pids = started_tree()
            call('emergency')
            wait(lambda s: not s['running'])
            wait_reaped(pids)
            call('clear_emergency')
            windows_crash_reaped = None
            if sys.platform == 'win32':
                pids_path.unlink()
                call('resume')
                pids = started_tree()
                process.kill()
                process.wait(timeout=5)
                wait_reaped(pids)
                process = subprocess.Popen(command, stdin=subprocess.DEVNULL, stdout=output, stderr=output)
                crash_state = wait(lambda s: not s['running'] and s['queue'][-1]['status'] == 'paused')
                assert crash_state['queue'][-1]['checkpoint'] == 'applied'
                windows_crash_reaped = True
            before_resume = len(calls)
            (tree / 'allow-validation').write_text('owner-approved retry')
            call('resume')
            wait(lambda s: not s['running'] and s['queue'][-1]['status'] == 'succeeded')
            assert len(calls) == before_resume
            print(json.dumps({'native_platform': sys.platform, 'descendants_reaped_after_stop_and_emergency': True, 'windows_runner_crash_reaps_job': windows_crash_reaped, 'stop_seconds': round(stop_seconds, 3), 'checkpoint_resume_no_rewrite_or_replanning': True, 'emergency_during_validation': True, 'auto_run_off_waits': True, 'auto_run_on_advances': True, 'restart_preserves_results': True, 'paused_checkpoint_survives_restart': True, 'validation_failure_blocks_advancement': True, 'start_binds_queue_frontier': True, 'malformed_output_blocks_advancement': True, 'model_calls': len(calls)}))
        finally:
            try:
                call('emergency')
                wait(lambda s: not s['running'], 5)
            except Exception:
                pass
            if process.poll() is None:
                process.terminate()
                process.wait(timeout=5)
            output.close()
    model.shutdown()


if __name__ == '__main__':
    main()

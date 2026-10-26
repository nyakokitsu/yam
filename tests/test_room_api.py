"""Isolated integration test: no hardware writes, packages, updates or audio."""
import json
from pathlib import Path
import socket
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request

daemon, client = map(lambda p: str(Path(p).resolve()), sys.argv[1:3])
with tempfile.TemporaryDirectory(prefix='yam-room-api-') as temporary:
    root = Path(temporary)
    (root / 'settings.toml').write_text('''[led]
profile = "custom"
count = 1
sysfs_root = "/nonexistent-yam-room-test-led"
path_template = "{root}/{channel}-{index}"
[web]
ui = false
api = true
[updates]
enabled = false
interval_minutes = 360
''')
    with socket.socket() as reservation:
        reservation.bind(('127.0.0.1', 0))
        port = reservation.getsockname()[1]
    base = f'http://127.0.0.1:{port}'

    def request(path, payload=None):
        data = None if payload is None else json.dumps(payload).encode()
        req = urllib.request.Request(base + '/api' + path, data=data,
                                     method='GET' if data is None else 'PUT',
                                     headers={'Content-Type': 'application/json'})
        with urllib.request.urlopen(req, timeout=2) as response:
            return json.load(response)

    def start():
        process = subprocess.Popen([daemon, '--listen', f'127.0.0.1:{port}', '--data-dir', str(root)],
                                   stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        try:
            for _ in range(100):
                assert process.poll() is None, 'daemon exited during readiness'
                try:
                    request('/room')
                    return process
                except (OSError, urllib.error.URLError):
                    time.sleep(0.05)
            raise AssertionError('room API never became ready')
        except BaseException:
            process.terminate()
            process.wait(timeout=5)
            raise

    def stop(process):
        process.terminate()
        process.wait(timeout=5)

    p = start()
    try:
        original = request('/device')
        assert original['room'] is None
        assert request('/room') == {'room': None}
        assert request('/room', {'room': ' Кухня '}) == {'room': 'Кухня'}
        assert request('/device')['room'] == 'Кухня'
        assert not (root / 'device.toml').exists(), 'room created a hardware override'
        for invalid in ({'room': 'bad\nroom'}, {'room': 'x' * 65}, {'room': 'Room', 'model': 'spoof'}):
            try:
                request('/room', invalid)
                raise AssertionError('invalid metadata accepted')
            except urllib.error.HTTPError as error:
                assert error.code in (400, 422)
        assert request('/room') == {'room': 'Кухня'}
        subprocess.run([client, '--url', base, 'room', 'Детская'], check=True, stdout=subprocess.DEVNULL)
        assert request('/room') == {'room': 'Детская'}
        current = request('/device')
        assert {k: v for k, v in original.items() if k != 'room'} == {k: v for k, v in current.items() if k != 'room'}
    finally:
        stop(p)
    p = start()
    try:
        assert request('/room') == {'room': 'Детская'}, 'room lost across restart'
        subprocess.run([client, '--url', base, 'room', '--clear'], check=True, stdout=subprocess.DEVNULL)
        assert request('/device')['room'] is None
    finally:
        stop(p)
    (root / 'room.toml').write_text('not valid [')
    p = subprocess.Popen([daemon, '--listen', f'127.0.0.1:{port}', '--data-dir', str(root)],
                         stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    assert p.wait(timeout=5) != 0, 'invalid room metadata silently ignored'
print('ROOM_API_CLI_PERSISTENCE_PASS')

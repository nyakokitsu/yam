#!/usr/bin/env python3
"""Exercise the real daemon, CLI and Lua package API against temporary LED sysfs files."""
import argparse
import io
import json
from pathlib import Path
import socket
import subprocess
import tarfile
import tempfile
import time
import urllib.error
import urllib.request

parser = argparse.ArgumentParser()
parser.add_argument('--daemon', default='target/debug/yamd')
parser.add_argument('--cli', default='target/debug/yam')
args = parser.parse_args()
with tempfile.TemporaryDirectory(prefix='yam-logo-') as tmp:
    root = Path(tmp)
    leds = root / 'leds'
    for zone, count in [('ring', 24), ('logo', 6)]:
        for index in range(count):
            for channel in ['red', 'green', 'blue']:
                node = leds / f'cucumber:{channel}:{zone}{index}'
                node.mkdir(parents=True)
                (node / 'brightness').write_text('17')
                (node / 'trigger').write_text('none')
    data = root / 'data'
    data.mkdir()
    (data / 'settings.toml').write_text(
        f'[led]\nprofile="midi"\ncount=24\nsysfs_root={json.dumps(str(leds))}\n')
    with socket.socket() as sock:
        sock.bind(('127.0.0.1', 0))
        port = sock.getsockname()[1]
    url = f'http://127.0.0.1:{port}'
    def request(path, body=None, method=None, headers=None):
        encoded = None if body is None else json.dumps(body).encode()
        req = urllib.request.Request(url + '/api' + path, encoded,
            headers or {'Content-Type': 'application/json'}, method=method)
        with urllib.request.urlopen(req, timeout=3) as response:
            raw = response.read()
            return json.loads(raw) if raw else None
    def value(zone, channel, index, attr='brightness'):
        return (leds / f'cucumber:{channel}:{zone}{index}' / attr).read_text()
    def rejected(body):
        try:
            request('/led/logo', body)
        except urllib.error.HTTPError as error:
            assert error.code == 400
        else:
            raise AssertionError('invalid logo request accepted')
    def cli(*command):
        subprocess.run([args.cli, '--url', url, *command], check=True, capture_output=True)
    log = (root / 'daemon.log').open('w+')
    daemon = subprocess.Popen([args.daemon, '--listen', f'127.0.0.1:{port}',
        '--data-dir', str(data), '--model', 'yandexmidi'], stdout=log, stderr=log)
    try:
        for _ in range(150):
            try:
                device = request('/device')
                break
            except OSError:
                assert daemon.poll() is None, 'daemon exited before ready'
                time.sleep(.02)
        else:
            raise AssertionError('daemon did not become ready')
        assert {'led.logo.rgb', 'led.logo.6', 'led.ring.24'} <= set(device['features'])
        cli('led', '--zone', 'logo', 'set', *(['#123456'] * 6))
        for index in range(6):
            assert [value('logo', c, index) for c in ['red', 'green', 'blue']] == ['18', '52', '86']
        assert value('ring', 'red', 0) == '17'
        rejected({'colors': ['#ffffff'] * 5})
        rejected({'colors': ['#ffffff'] * 5 + ['#é0000']})
        assert value('logo', 'red', 0) == '18'
        cli('led', 'set', *(['#010203'] * 24))
        assert value('ring', 'red', 0) == '1'
        assert value('logo', 'red', 0) == '18'
        cli('led', 'off')
        assert value('ring', 'red', 0, 'trigger') == 'ledfb-ring'
        assert value('logo', 'red', 0) == '18'
        cli('led', '--zone', 'logo', 'off')
        assert value('logo', 'red', 0, 'trigger') == 'ledfb-logo'
        print('PASS HTTP/CLI logo frames, invalid input, independent ring and release')

        manifest = b'id="logo-test"\nname="Logo test"\nversion="1"\nentry="main.lua"\npermissions=["led"]\n[compatibility]\nfeatures=["led.logo.rgb"]\n'
        script = b'assert(device.led_count == 24 and device.logo_led_count == 6)\nalice.logo.set({"#102030","#102030","#102030","#102030","#102030","#102030"})\nwhile true do alice.sleep(20) end\n'
        archive = io.BytesIO()
        with tarfile.open(fileobj=archive, mode='w:gz') as tar:
            for name, content in [('plugin.toml', manifest), ('main.lua', script)]:
                entry = tarfile.TarInfo(name)
                entry.size = len(content)
                tar.addfile(entry, io.BytesIO(content))
        boundary = 'yam-logo-test-boundary'
        payload = (f'--{boundary}\r\nContent-Disposition: form-data; name="package"; filename="logo.yap"\r\nContent-Type: application/octet-stream\r\n\r\n'.encode()
            + archive.getvalue() + f'\r\n--{boundary}--\r\n'.encode())
        req = urllib.request.Request(url + '/api/plugins', payload,
            {'Content-Type': f'multipart/form-data; boundary={boundary}'})
        with urllib.request.urlopen(req, timeout=3) as response:
            assert json.load(response)['id'] == 'logo-test'
        request('/plugins/logo-test/toggle', method='POST')
        request('/plugins/logo-test/invoke', method='POST')
        for _ in range(100):
            if value('logo', 'red', 0) == '16':
                break
            time.sleep(.02)
        else:
            raise AssertionError('Lua logo frame not written')
        request('/plugins/logo-test/stop', method='POST')
        assert value('logo', 'red', 0) == '0'
        assert value('logo', 'red', 0, 'trigger') == 'ledfb-logo'
        assert value('ring', 'red', 0, 'trigger') == 'ledfb-ring'
        print('PASS package upload, enable/start, Lua logo API and stop cleanup')

        settings = request('/settings')
        settings['led']['profile'] = 'micro'
        request('/settings', settings, 'PUT')
        assert 'led.logo.rgb' not in request('/device')['features']
        rejected({'colors': ['#ffffff'] * 6})
        assert value('logo', 'red', 0) == '0'
        print('PASS unsupported profile rejection and live capability refresh')
    except Exception:
        log.flush()
        log.seek(0)
        print(log.read())
        raise
    finally:
        daemon.terminate()
        try:
            daemon.wait(timeout=5)
        except subprocess.TimeoutExpired:
            daemon.kill()
            daemon.wait()
        log.close()

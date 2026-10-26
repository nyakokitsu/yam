"""Isolated HTTP/runtime/IPC fixture. No speaker access, logs, audio or LEDs."""
import contextlib
import json
import os
from pathlib import Path
import socket
import struct
import subprocess
import sys
import tempfile
import threading
import time
import urllib.request
import urllib.error

def vi(n):
    out = bytearray()
    while n > 127:
        out.append((n & 127) | 128); n >>= 7
    out.append(n); return bytes(out)
def field(n, v): return vi(n * 8 + 2) + vi(len(v)) + v
def frame(v): return struct.pack('<i', len(v)) + v
def volume(v): return frame(field(198, field(1, field(11, b'\x35' + struct.pack('<f', v)))))
def speech(text, request):
    return frame(field(3, field(7, request.encode()) + field(15, field(1, text.encode()) + b'\x10\x01')))
def reserve():
    s=socket.socket(); s.bind(('127.0.0.1',0)); s.listen(); s.settimeout(.2); return s

with tempfile.TemporaryDirectory(prefix='yam-media-fixture-') as temporary:
    root=Path(temporary); os.chmod(root, 0o700)
    listeners={key: reserve() for key in ('state','control','speech')}
    ports={key:s.getsockname()[1] for key,s in listeners.items()}
    stop=threading.Event(); writes=[]; state_connections=[]; speech_connections=[]
    lock=threading.Lock()
    def accept(kind):
        while not stop.is_set():
            try: conn,_=listeners[kind].accept()
            except socket.timeout: continue
            except OSError: return
            conn.settimeout(2)
            if kind=='state':
                with lock: state_connections.append(conn)
                conn.sendall(volume(70))
            elif kind=='speech':
                with lock: speech_connections.append(conn)
            else:
                with conn:
                    data=b''
                    while len(data)<4: data+=conn.recv(4-len(data))
                    size=struct.unpack('<i',data)[0]
                    while len(data)<size+4: data+=conn.recv(size+4-len(data))
                    writes.append(data[4:])
                with lock:
                    for state in state_connections:
                        with contextlib.suppress(OSError): state.sendall(volume(65))
    threads=[threading.Thread(target=accept,args=(kind,),daemon=True) for kind in listeners]
    for t in threads:t.start()
    api_socket=reserve(); port=api_socket.getsockname()[1];api_socket.close()
    base=f'http://127.0.0.1:{port}'
    (root/'settings.toml').write_text('''allow_plugin_binaries = true
[led]
profile = "custom"
count = 1
sysfs_root = "/nonexistent-yam-media-fixture"
path_template = "{root}/{channel}-{index}"
[web]
ui = false
api = true
[updates]
enabled = false
interval_minutes = 360
''')
    for id,permissions in [('fixture',['exec','ipc.speech','commands']),('denied',['exec'])]:
        plugin=root/'plugins'/id;plugin.mkdir(parents=True)
        policy='\n[commands]\nphrases = ["следующий трек"]\n' if id=='fixture' else ''
        (plugin/'plugin.toml').write_text(f'''id = "{id}"
name = "Isolated media fixture"
version = "0.0.0"
entry = "fixture.sh"
permissions = {json.dumps(permissions)}
[launch]
kind = "binary"
mode = "service"
path = "fixture.sh"
'''+policy)
        script=plugin/'fixture.sh'
        script.write_text('#!/bin/sh\numask 077\nprintf %s "$YAM_API_TOKEN" > .fixture-token\nwhile :; do sleep 1; done\n')
        script.chmod(0o700)
    (root/'plugins/enabled.json').write_text('["fixture","denied"]')
    env=dict(os.environ,YAM_STEREO_PAIR_ADDR=f'127.0.0.1:{ports["state"]}',
             YAM_IOHUB_CLIENTS_ADDR=f'127.0.0.1:{ports["control"]}',YAM_ALICED_ADDR=f'127.0.0.1:{ports["speech"]}')
    daemon=subprocess.Popen([str(Path(sys.argv[1]).resolve()),'--listen',f'127.0.0.1:{port}','--data-dir',str(root)],
                            env=env,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)
    def request(path,token='',payload=None,method=None):
        data=None if payload is None else json.dumps(payload).encode()
        req=urllib.request.Request(base+path,data=data,method=method or ('GET' if data is None else 'POST'),
              headers={'X-YAM-Token':token,'Content-Type':'application/json'})
        with urllib.request.urlopen(req,timeout=6) as r:
            raw=r.read();return json.loads(raw) if raw else None
    def rejected(path,token='',payload=None,method=None):
        try:request(path,token,payload,method)
        except urllib.error.HTTPError as e:assert 400<=e.code<500;return
        raise AssertionError('unauthorized/invalid request accepted')
    try:
        for _ in range(100):
            assert daemon.poll() is None,'daemon failed'
            if all((root/'plugins'/id/'.fixture-token').exists() for id in ('fixture','denied')):break
            time.sleep(.05)
        token=(root/'plugins/fixture/.fixture-token').read_text()
        denied=(root/'plugins/denied/.fixture-token').read_text()
        url='/api/runtime/fixture'
        rejected(url+'/speech/next',payload={});rejected(url+'/speech/next','wrong',{})
        rejected(url+'/speech/next',denied,{})
        rejected('/api/runtime/denied/speech/next',denied,{})
        caps=request(url+'/media',token);assert caps['version']==1 and caps['speech']
        assert 'native_volume' not in caps, 'duplicated native volume API returned'
        rejected(url+'/volume',token) # Removed: reuse the station-volume package.
        assert len(writes)==0
        # Poll creates one persistent local IPC connection. Split headers/payloads
        # survive timeouts; no read_exact truncation/replay between HTTP requests.
        assert request(url+'/speech/next',token,{}) is None
        for _ in range(30):
            if speech_connections:break
            time.sleep(.01)
        conn=speech_connections[-1];data=speech('следующий трек','fixture-request')
        conn.sendall(data[:2]);assert request(url+'/speech/next',token,{}) is None
        conn.sendall(data[2:7]);assert request(url+'/speech/next',token,{}) is None
        conn.sendall(data[7:]);event=request(url+'/speech/next',token,{})
        assert event['final'] and event['request_id']=='fixture-request' and event['transcript']=='следующий трек'
        assert event['initial_snapshot'], 'first native snapshot can be a stale command'
        assert len(speech_connections)==1
        rejected(url+'/command/cancel',token,{'request_id':'fixture-request'})
        conn.sendall(speech('следующий трек','new-request'))
        assert request(url+'/speech/next',token,{})['request_id']=='new-request'
        assert request(url+'/command/cancel',token,{'request_id':'new-request'}) is None
        rejected(url+'/command/cancel',token,{'request_id':'new-request'})
        conn.sendall(speech('следующий трек','new-request'))
        request(url+'/speech/next',token,{})
        rejected(url+'/command/cancel',token,{'request_id':'new-request'}) # repeated final is not a second claim.
        conn.sendall(struct.pack('<i',-1));rejected(url+'/speech/next',token,{})
        # Stop invalidates token; no continuation of an earlier invocation.
        before=len(speech_connections);poll_result=[]
        def in_flight_poll():
            try:request(url+'/speech/next',token,{});poll_result.append('stale success')
            except urllib.error.HTTPError as error:poll_result.append(error.code)
        pending=threading.Thread(target=in_flight_poll);pending.start()
        for _ in range(30):
            if len(speech_connections)>before:break
            time.sleep(.01)
        req=urllib.request.Request(base+'/api/plugins/fixture/stop',data=b'{}',method='POST',headers={'Content-Type':'application/json'})
        with urllib.request.urlopen(req,timeout=3):pass
        time.sleep(.2);rejected(url+'/speech/next',token,{})
        pending.join(timeout=3);assert poll_result and isinstance(poll_result[0],int),'in-flight poll outlived revoked runtime token'
    finally:
        daemon.terminate();daemon.wait(timeout=8);stop.set()
        for s in listeners.values():s.close()
        for s in state_connections+speech_connections:s.close()
        for t in threads:t.join(timeout=3)
print('SPEECH_API_AUTH_FRAGMENTED_IPC_NO_DUPLICATE_VOLUME_NO_HARDWARE_PASS')

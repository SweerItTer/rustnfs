#!/usr/bin/env python3
"""Exercise a built Windows RustNFS GUI and its real daemon in isolated Wine.

Requires Wine, Xvfb, MinGW, Pillow with XCB, and this directory's GUI driver.
Pass --binary-dir pointing to a completed release build. Uses temporary config,
exports, a private Wine prefix, and only its own child processes. No real shares.
"""
from __future__ import annotations
import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import socket
import struct
import subprocess
import sys
import time
from datetime import datetime, timezone

PORTS = (104, 105, 106)
RESET, SAVE, START, STOP, TCP, UDP = 107, 108, 120, 121, 123, 124
WINDOW_CLASS = "RustNFS.Native.Manager.0.1"


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument('--binary-dir', type=Path, required=True)
    parser.add_argument('--work-dir', type=Path, required=True)
    parser.add_argument('--report', type=Path, required=True)
    parser.add_argument('--display', default=':92')
    parser.add_argument('--wine', default='/usr/lib/wine/wine64')
    args = parser.parse_args()
    work = args.work_dir.resolve()
    work.mkdir(parents=True, exist_ok=True)
    app = work / 'app'
    app.mkdir(exist_ok=True)
    prefix = work / 'wine-prefix'
    env = dict(os.environ, DISPLAY=args.display, WINEPREFIX=str(prefix), WINEARCH='win64',
               WINEDEBUG='-all', WINEDLLOVERRIDES='mscoree,mshtml=')
    report = {'suite': 'windows-gui-ports-acceptance', 'version': '0.1.2',
              'timestamp_utc': datetime.now(timezone.utc).isoformat(),
              'execution': 'Windows x64 EXEs in Wine 9 / Xvfb on Linux; not native Windows',
              'checks': [], 'screenshots': {}, 'executables': {}}
    for name in ('rustnfs-gui.exe', 'rustnfsd.exe'):
        source = args.binary_dir / name
        shutil.copy2(source, app / name)
        report['executables'][name] = {'sha256': hashlib.sha256(source.read_bytes()).hexdigest()}
    share = work / 'share'
    share.mkdir(exist_ok=True)
    initial = {'bind_address': '127.0.0.1', 'portmap_port': 16011, 'mount_port': 16048,
               'nfs_port': 16049, 'allowed_clients': ['127.0.0.1/32', '10.0.0.1/32'],
               'exports': [{'name': 'qa', 'path': 'Z:' + str(share).replace('/', '\\'),
                            'read_only': False, 'uid': 42, 'gid': 43,
                            'file_mode': 493, 'dir_mode': 493}], 'max_connections': 17}
    config_file = app / 'rustnfs.json'
    config_file.write_text(json.dumps(initial, indent=2) + '\n')
    driver = work / 'windows_gui_driver.exe'
    source = Path(__file__).with_name('windows_gui_driver.c')
    subprocess.run(['x86_64-w64-mingw32-gcc', '-municode', '-Os', '-Wall', str(source),
                    '-o', str(driver), '-luser32'], check=True)
    display_log = (work / 'xvfb.log').open('wb')
    xvfb = subprocess.Popen(['Xvfb', args.display, '-screen', '0', '1200x1000x24', '-nolisten', 'tcp'],
                            stdout=display_log, stderr=subprocess.STDOUT)
    gui = None
    app_log = (work / 'gui-process.log').open('wb')

    def record(name, **details):
        report['checks'].append({'name': name, 'status': 'pass', **details})
        print('PASS', name, flush=True)

    def check(value, message):
        if not value:
            raise AssertionError(message)

    def drive(*command, allow_missing=False):
        run = subprocess.run([args.wine, str(driver), *map(str, command)], env=env,
                              stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=12)
        if allow_missing and run.returncode == 2:
            return None
        check(run.returncode == 0, f'driver {command}: {run.returncode} {run.stderr!r} {run.stdout!r}')
        return json.loads(run.stdout.decode('utf-8-sig'))

    def until(predicate, message, timeout=12):
        end = time.monotonic() + timeout
        while time.monotonic() < end:
            value = predicate()
            if value:
                return value
            time.sleep(.05)
        raise AssertionError(message)

    def dump():
        return drive('dump')

    def controls(snapshot=None):
        return {item['id']: item for item in (snapshot or dump())['controls'] if item['id'] > 0}

    def port_values(snapshot=None):
        c = controls(snapshot)
        return [c[i]['text'] for i in PORTS]

    def command_text(snapshot=None):
        values = [c for c in (snapshot or dump())['controls'] if c['class'].lower() == 'edit' and c['id'] == 0]
        return sorted(values, key=lambda c: c['rect'][1])[0]['text']

    def log_text(snapshot=None):
        values = [c for c in (snapshot or dump())['controls'] if c['class'].lower() == 'edit' and c['id'] == 0]
        return sorted(values, key=lambda c: c['rect'][1])[-1]['text']

    def screenshot(name):
        from PIL import ImageGrab
        snap = dump()
        path = work / (name + '.png')
        ImageGrab.grab(bbox=tuple(snap['rect']), xdisplay=args.display).save(path)
        report['screenshots'][name] = str(path)
        return path

    def launch():
        nonlocal gui
        gui = subprocess.Popen([args.wine, str(app / 'rustnfs-gui.exe')], env=env, cwd=app,
                                stdout=app_log, stderr=subprocess.STDOUT)
        until(lambda: drive('dump', allow_missing=True), 'GUI did not open', timeout=30)

    def close():
        nonlocal gui
        drive('close')
        gui.wait(timeout=12)
        check(gui.returncode == 0, f'GUI exit {gui.returncode}')
        gui = None

    def other_settings(config):
        return {key: value for key, value in config.items() if key not in ('portmap_port', 'mount_port', 'nfs_port')}

    def rpc_null(protocol, port, program, version):
        xid = program + port
        call = struct.pack('!10I', xid, 0, 2, program, version, 0, 0, 0, 0, 0)
        if protocol == 'UDP':
            with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as s:
                s.settimeout(2)
                s.sendto(call, ('127.0.0.1', port))
                reply, _ = s.recvfrom(2048)
        else:
            with socket.create_connection(('127.0.0.1', port), timeout=2) as s:
                s.sendall(struct.pack('!I', 0x80000000 | len(call)) + call)
                def read(count):
                    value = b''
                    while len(value) < count:
                        data = s.recv(count-len(value))
                        check(bool(data), 'short RPC reply')
                        value += data
                    return value
                marker, = struct.unpack('!I', read(4))
                check(marker & 0x80000000, 'fragmented null reply')
                reply = read(marker & 0x7fffffff)
        check(reply == struct.pack('!6I', xid, 1, 0, 0, 0, 0), f'{protocol} null reply {port}: {reply.hex()}')

    try:
        until(lambda: Path('/tmp/.X11-unix/X' + args.display.lstrip(':')).exists(), 'Xvfb not ready')
        subprocess.run([args.wine, 'cmd', '/c', 'exit'], env=env, stdout=app_log,
                       stderr=subprocess.STDOUT, check=True, timeout=45)
        launch()
        check(port_values() == ['16011', '16048', '16049'], 'did not load configured ports')
        check('0.1.2' in dump()['title'], 'wrong GUI version')
        record('load_existing_custom_ports', values=port_values())
        screenshot('ports-loaded')

        for ident, value in zip(PORTS, ('16111', '16148', '16149')):
            drive('set', ident, value)
        preview = command_text()
        check('port=16149,mountport=16148' in preview, preview)
        check(json.loads(config_file.read_text()) == initial, 'editing unexpectedly wrote config')
        copied = {}
        for ident, protocol in ((TCP, 'tcp'), (UDP, 'udp')):
            drive('click', ident)
            copied[protocol] = until(lambda: (value if f'mountproto={protocol}' in (value := drive('clipboard')['text']) else None), 'clipboard did not update')
            check('port=16149,mountport=16148' in copied[protocol], copied[protocol])
        record('preview_and_clipboard_use_unsaved_ports', preview=preview, copied=copied)

        drive('click', SAVE)
        until(lambda: json.loads(config_file.read_text())['nfs_port'] == 16149, 'save did not persist ports')
        saved = json.loads(config_file.read_text())
        check(other_settings(saved) == other_settings(initial), 'save changed unrelated settings')
        close()
        launch()
        check(port_values() == ['16111', '16148', '16149'], 'restart did not restore edited ports')
        record('save_and_reload_custom_ports')

        before_reset = config_file.read_bytes()
        drive('click', RESET)
        until(lambda: port_values() == ['111', '20048', '2049'], 'defaults not restored in UI')
        check(config_file.read_bytes() == before_reset, 'reset persisted before save')
        drive('click', SAVE)
        until(lambda: json.loads(config_file.read_text())['nfs_port'] == 2049, 'default ports not saved')
        defaults = json.loads(config_file.read_text())
        check([defaults['portmap_port'], defaults['mount_port'], defaults['nfs_port']] == [111, 20048, 2049], 'wrong defaults')
        check(other_settings(defaults) == other_settings(initial), 'reset changed non-port settings')
        record('restore_defaults_only_changes_ports', values=[111, 20048, 2049])

        for name, value in (('empty',''), ('zero','0'), ('above_maximum','65536'), ('not_integer','abc'), ('duplicate','111')):
            baseline = config_file.read_bytes()
            drive('set', 106, value)
            snapshot = dump()
            check(not controls(snapshot)[TCP]['enabled'] and not controls(snapshot)[UDP]['enabled'], 'invalid ports allow copy')
            drive('click', SAVE)
            dialog = until(lambda: (value if (value := drive('dialog'))['present'] else None), 'invalid input did not show dialog')
            message = '\n'.join(c['text'] for c in dialog['controls'] if c['class'].lower() == 'static')
            check('端口' in message and ('NFS' in message or name == 'duplicate'), message)
            check(config_file.read_bytes() == baseline, 'invalid input changed saved config')
            drive('dismiss')
            until(lambda: not drive('dialog')['present'], 'error dialog remains')
            drive('set', 106, '2049')
            record('reject_' + name, message=message)

        for ident, value in zip(PORTS, ('16111', '16148', '16149')):
            drive('set', ident, value)
        drive('click', START)
        until(lambda: 'RUSTNFS_READY' in log_text(), 'daemon did not become ready', timeout=20)
        snapshot = dump()
        check(all(not controls(snapshot)[i]['enabled'] for i in (*PORTS, RESET, SAVE)), 'settings enabled while running')
        check(controls(snapshot)[STOP]['enabled'], 'stop disabled while running')
        check(json.loads(config_file.read_text())['nfs_port'] == 16149, 'start did not save edits')
        for port, program, version in ((16111,100000,2), (16148,100005,3), (16149,100003,3)):
            for protocol in ('TCP','UDP'):
                rpc_null(protocol, port, program, version)
        record('start_real_backend_on_edited_ports_and_lock_settings', ports=[16111,16148,16149], transports=['TCP','UDP'])
        screenshot('ports-running')
        report['running_log'] = log_text()
        drive('click', STOP)
        until(lambda: controls()[START]['enabled'], 'daemon did not stop', timeout=12)
        snapshot = dump()
        check(all(controls(snapshot)[i]['enabled'] for i in (*PORTS, RESET, SAVE)), 'settings did not unlock')
        check('RUSTNFS_STOPPED' in log_text(snapshot), 'daemon did not shut down gracefully')
        for port in (16111,16148,16149):
            for kind in (socket.SOCK_STREAM,socket.SOCK_DGRAM):
                with socket.socket(socket.AF_INET,kind) as s:
                    s.bind(('127.0.0.1',port))
        record('stop_unlocks_settings_and_releases_ports')

        drive('size', 840, 780)
        snapshot = dump()
        check(all(controls(snapshot)[i]['rect'][2] <= snapshot['rect'][2] and controls(snapshot)[i]['rect'][3] <= snapshot['rect'][3] for i in (*PORTS,RESET,SAVE)), 'new controls clipped at minimum size')
        screenshot('ports-minimum-size')
        record('minimum_window_contains_new_controls', rect=snapshot['rect'])
        baseline = config_file.read_bytes()
        drive('set', 106, '')
        drive('close')
        dialog = until(lambda: (value if (value := drive('dialog'))['present'] else None), 'invalid-close did not prompt')
        check(any(c['id'] == 6 for c in dialog['controls']) and any(c['id'] == 7 for c in dialog['controls']), 'invalid-close prompt is not Yes/No')
        drive('dismiss', 7)
        until(lambda: not drive('dialog')['present'], 'No did not return to editing')
        check(dump()['enabled'], 'window disabled after declining discard')
        check(config_file.read_bytes() == baseline, 'No altered config')
        drive('close')
        until(lambda: drive('dialog')['present'], 'second invalid-close prompt missing')
        drive('dismiss', 6)
        gui.wait(timeout=12)
        check(gui.returncode == 0, 'discard exit failed')
        gui = None
        check(config_file.read_bytes() == baseline, 'discard changed persisted config')
        record('invalid_close_can_cancel_or_discard_without_overwriting_config')
        report['passed'] = len(report['checks'])
        report['failed'] = 0
    except Exception as error:
        report['failed'] = 1
        report['passed'] = len(report['checks'])
        report['failure'] = repr(error)
        print('FAIL', repr(error), flush=True)
        try:
            report['failure_dump'] = drive('dump')
            screenshot('failure')
        except Exception:
            pass
    finally:
        if gui and gui.poll() is None:
            try:
                dialog = drive('dialog')
                if dialog['present']:
                    drive('dismiss', 6 if any(c['id']==6 for c in dialog['controls']) else 1)
                if gui.poll() is None:
                    drive('close')
                gui.wait(timeout=6)
            except Exception:
                gui.kill()
                gui.wait(timeout=3)
        # This Wine prefix belongs exclusively to this test run.
        subprocess.run(['/usr/lib/wine/wineserver', '-k'], env=env, timeout=10, check=False,
                       stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        xvfb.terminate()
        xvfb.wait(timeout=5)
        app_log.close()
        display_log.close()
        args.report.parent.mkdir(parents=True, exist_ok=True)
        args.report.write_text(json.dumps(report, ensure_ascii=False, indent=2) + '\n')
    return int(report['failed'] != 0)


if __name__ == '__main__':
    sys.exit(main())

#!/usr/bin/env python3
"""Run the Windows server under Wine with an isolated NFS UDP port conflict."""
import argparse
import datetime
import hashlib
import json
import os
from pathlib import Path
import platform
import socket
import subprocess
import tempfile


def reserve_pair():
    for _ in range(100):
        tcp = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        udp = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        tcp.bind(("127.0.0.1", 0))
        port = tcp.getsockname()[1]
        try:
            udp.bind(("127.0.0.1", port))
        except OSError:
            tcp.close()
            udp.close()
            continue
        tcp.listen(1)
        return port, tcp, udp
    raise RuntimeError("Could not reserve a loopback TCP/UDP pair")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--exe", type=Path, required=True)
    parser.add_argument("--wine", default="/usr/lib/wine/wine64")
    parser.add_argument("--prefix", type=Path, required=True)
    parser.add_argument("--report", type=Path, required=True)
    args = parser.parse_args()
    env = dict(os.environ, WINEPREFIX=str(args.prefix.resolve()), WINEARCH="win64",
               WINEDEBUG="-all", WINEDLLOVERRIDES="mscoree,mshtml=")
    pairs = [reserve_pair() for _ in range(3)]
    blocker = pairs[2][2]
    try:
        with tempfile.TemporaryDirectory(prefix="rustnfs-v012-port-conflict-") as root:
            root = Path(root)
            share = root / "share"
            share.mkdir()
            config = {
                "bind_address": "127.0.0.1",
                "portmap_port": pairs[0][0], "mount_port": pairs[1][0],
                "nfs_port": pairs[2][0], "allowed_clients": ["127.0.0.1/32"],
                "exports": [{"name": "test", "path": "Z:" + str(share).replace("/", "\\"),
                             "read_only": False, "uid": 0, "gid": 0,
                             "file_mode": 493, "dir_mode": 493}],
                "max_connections": 32,
            }
            config_path = root / "conflict.json"
            config_path.write_text(json.dumps(config, indent=2), encoding="utf-8")
            for _, tcp, udp in pairs:
                tcp.close()
                if udp is not blocker:
                    udp.close()
            command = [args.wine, str(args.exe.resolve()), "--config",
                       "Z:" + str(config_path).replace("/", "\\")]
            result = subprocess.run(command, env=env, input="", capture_output=True,
                                    text=True, encoding="utf-8", errors="replace", timeout=45)
            combined = result.stdout + result.stderr
            checks = {
                "exit_code_is_1": result.returncode == 1,
                "exact_custom_nfs_udp_endpoint": f"NFS UDP 127.0.0.1:{pairs[2][0]}" in combined,
                "port_in_use_reason": "端口已被占用" in combined,
                "original_windows_error_10048": "os error 10048" in combined,
                "no_ready_on_partial_start": "RUSTNFS_READY" not in combined,
            }
            rebound = []
            for index, (port, _, _) in enumerate(pairs):
                for protocol, kind in [("TCP", socket.SOCK_STREAM), ("UDP", socket.SOCK_DGRAM)]:
                    if index == 2 and protocol == "UDP":
                        continue
                    with socket.socket(socket.AF_INET, kind) as probe:
                        probe.bind(("127.0.0.1", port))
                        if protocol == "TCP":
                            probe.listen(1)
                    rebound.append(f"{protocol} 127.0.0.1:{port}")
            checks["all_five_partial_listeners_released"] = len(rebound) == 5
            report = {
                "schema_version": 1, "suite": "windows-port-conflict", "version": "0.1.2",
                "timestamp_utc": datetime.datetime.now(datetime.timezone.utc).isoformat(),
                "environment": {"host_platform": platform.platform(), "native_windows": False,
                                "execution": "Windows x64 GNU executable under Wine on Linux",
                                "wine_version": subprocess.check_output([args.wine, "--version"], text=True).strip()},
                "executable": {"path": str(args.exe.resolve()),
                               "sha256": hashlib.sha256(args.exe.read_bytes()).hexdigest()},
                "config": config, "occupied_socket": f"NFS UDP 127.0.0.1:{pairs[2][0]}",
                "command": command, "exit_code": result.returncode,
                "stdout": result.stdout, "stderr": result.stderr,
                "released_listeners": rebound, "checks": checks, "passed": all(checks.values()),
            }
            args.report.write_text(json.dumps(report, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
            print(json.dumps({"passed": report["passed"], "checks": checks, "stderr": result.stderr},
                             ensure_ascii=False, indent=2))
            return 0 if report["passed"] else 1
    finally:
        for _, tcp, udp in pairs:
            tcp.close()
            udp.close()


if __name__ == "__main__":
    raise SystemExit(main())

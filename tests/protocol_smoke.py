#!/usr/bin/env python3
r"""Independent, destructive-in-its-own-scratch-directory NFSv3 acceptance test.

No third-party Python modules, kernel mount, root, or server code imports.

Fixture requirements:
  * Export /share is writable and export /readonly is read-only.
  * Each export contains fixture.txt with exactly b"RustNFS fixture\n".
  * Use disposable test exports, not production data. The script creates a
    uniquely named protocol-smoke-* directory, then removes only that directory.
    Negative pathname tests also use unique names. An incorrectly unrestricted
    server may create those names, which the script attempts to remove.
  * Stop other processes that modify these exports while testing pagination.

Examples:
  python tests/protocol_smoke.py
  python tests/protocol_smoke.py --pmap-port 12011 --mount-port 12048 \
      --nfs-port 12049 --transport both --max-record-bytes 2097152
  python tests/protocol_smoke.py --host 192.168.1.10 --transport tcp
  python tests/protocol_smoke.py --windows-case-rename

The executable under test may run natively on Windows or under Wine. This
script connects to an already running server; it does not start or install it.
stdout is a single JSON result; exit 0 means every requested check passed.
Use --fixture-text to override the fixture contents (literal shell newlines).
The invalid-attribute checks assume fixed UID/GID mapping and unsupported
identity changes, as configured for the default acceptance-test exports.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import socket
import struct
import sys
import time
import uuid
from dataclasses import dataclass
from typing import Any, Callable


PMAP, MOUNT, NFS = 100000, 100005, 100003
OK, NOENT, ACCES, EXIST, INVAL, ROFS = 0, 2, 13, 17, 22, 30
STALE, BADHANDLE, BAD_COOKIE, NOTSUPP, TOOSMALL = 70, 10001, 10003, 10004, 10005
FILE_SYNC = 2
NAMES = {
    0: "OK", 1: "PERM", 2: "NOENT", 5: "IO", 6: "NXIO", 13: "ACCES",
    17: "EXIST", 18: "XDEV", 19: "NODEV", 20: "NOTDIR", 21: "ISDIR",
    22: "INVAL", 27: "FBIG", 28: "NOSPC", 30: "ROFS", 31: "MLINK",
    63: "NAMETOOLONG", 66: "NOTEMPTY", 69: "DQUOT", 70: "STALE",
    10001: "BADHANDLE", 10002: "NOT_SYNC", 10003: "BAD_COOKIE",
    10004: "NOTSUPP", 10005: "TOOSMALL", 10006: "SERVERFAULT",
    10007: "BADTYPE", 10008: "JUKEBOX",
}


def check(condition: bool, message: str) -> None:
    if not condition:
        raise AssertionError(message)


def u32(value: int) -> bytes:
    return struct.pack("!I", value)


def u64(value: int) -> bytes:
    return struct.pack("!Q", value)


def opaque(value: bytes) -> bytes:
    return u32(len(value)) + value + b"\0" * (-len(value) % 4)


def string(value: str | bytes) -> bytes:
    return opaque(value.encode("utf-8") if isinstance(value, str) else value)


def sattr(mode: int | None = None, size: int | None = None) -> bytes:
    # set_mode; set_uid=false; set_gid=false; set_size; two DONT_CHANGE times.
    return ((u32(0) if mode is None else u32(1) + u32(mode)) + u32(0) +
            u32(0) + (u32(0) if size is None else u32(1) + u64(size)) +
            u32(0) + u32(0))


def requested_sattr(*, uid: int | None = None, gid: int | None = None,
                    size: int | None = None, atime: tuple[int, int] | None = None,
                    mtime: tuple[int, int] | None = None) -> bytes:
    """Encode caller-requested attributes, including semantic error cases."""
    return (u32(1) + u32(0o755) +
            (u32(0) if uid is None else u32(1) + u32(uid)) +
            (u32(0) if gid is None else u32(1) + u32(gid)) +
            (u32(0) if size is None else u32(1) + u64(size)) +
            (u32(0) if atime is None else u32(2) + u32(atime[0]) + u32(atime[1])) +
            (u32(0) if mtime is None else u32(2) + u32(mtime[0]) + u32(mtime[1])))


class XDR:
    def __init__(self, data: bytes):
        self.data = data
        self.pos = 0

    def fixed(self, count: int) -> bytes:
        check(0 <= count <= len(self.data) - self.pos,
              f"truncated XDR at {self.pos}: need {count}, have {len(self.data)-self.pos}")
        out = self.data[self.pos:self.pos + count]
        self.pos += count
        return out

    def u32(self) -> int:
        return struct.unpack("!I", self.fixed(4))[0]

    def u64(self) -> int:
        return struct.unpack("!Q", self.fixed(8))[0]

    def boolean(self) -> bool:
        value = self.u32()
        check(value in (0, 1), f"invalid XDR boolean {value}")
        return bool(value)

    def opaque(self, limit: int = 2 * 1024 * 1024) -> bytes:
        count = self.u32()
        check(count <= limit, f"opaque length {count} exceeds {limit}")
        data = self.fixed(count)
        pad = self.fixed(-count % 4)
        check(not any(pad), "XDR padding contains nonzero bytes")
        return data

    def string(self, limit: int = 1024) -> str:
        return self.opaque(limit).decode("utf-8")

    def done(self) -> None:
        check(self.pos == len(self.data), f"{len(self.data)-self.pos} unexpected reply bytes")


def attr(reader: XDR) -> dict[str, Any]:
    result = {key: reader.u32() for key in ("type", "mode", "nlink", "uid", "gid")}
    result.update({key: reader.u64() for key in ("size", "used")})
    result["rdev"] = (reader.u32(), reader.u32())
    result.update({key: reader.u64() for key in ("fsid", "fileid")})
    for key in ("atime", "mtime", "ctime"):
        sec, nsec = reader.u32(), reader.u32()
        check(nsec < 1_000_000_000, f"invalid {key} nanoseconds {nsec}")
        result[key] = (sec, nsec)
    return result


def post_attr(reader: XDR) -> dict[str, Any] | None:
    return attr(reader) if reader.boolean() else None


def post_fh(reader: XDR) -> bytes | None:
    return reader.opaque(64) if reader.boolean() else None


def wcc(reader: XDR) -> None:
    if reader.boolean():
        reader.u64()
        reader.fixed(16)  # pre-operation mtime and ctime
    post_attr(reader)


@dataclass
class Reply:
    accepted: bool
    code: int
    payload: bytes
    raw: bytes


class RPC:
    def __init__(self, host: str, transport: str, timeout: float):
        self.host, self.transport, self.timeout = host, transport, timeout
        self.xid = int.from_bytes(os.urandom(4), "big")
        self.streams: dict[int, socket.socket] = {}
        self.udp: socket.socket | None = None

    def next_xid(self) -> int:
        self.xid = (self.xid + 1) & 0xffffffff
        return self.xid

    def request(self, program: int, version: int, procedure: int, args: bytes = b"",
                xid: int | None = None, rpc_version: int = 2,
                auth_none: bool = False) -> tuple[int, bytes]:
        xid = self.next_xid() if xid is None else xid
        credential = b"" if auth_none else (u32(0) + string("protocol-smoke") +
                                             u32(0) + u32(0) + u32(0))
        header = b"".join(u32(v) for v in (xid, 0, rpc_version, program, version, procedure))
        return xid, header + u32(0 if auth_none else 1) + opaque(credential) + u32(0) + u32(0) + args

    @staticmethod
    def parse(data: bytes, xid: int) -> Reply:
        r = XDR(data)
        check(r.u32() == xid, "reply XID does not match request")
        check(r.u32() == 1, "RPC message is not a REPLY")
        accepted = r.u32()
        check(accepted in (0, 1), "invalid RPC reply discriminant")
        if accepted == 0:
            r.u32()  # verifier flavor
            r.opaque(400)
        code = r.u32()
        return Reply(accepted == 0, code, data[r.pos:], data)

    @staticmethod
    def framed(data: bytes, cuts: list[int] | None = None) -> bytes:
        if not cuts:
            return u32(0x80000000 | len(data)) + data
        pieces, start = [], 0
        for cut in cuts:
            check(0 <= cut <= len(data)-start, "invalid test fragmentation")
            pieces.append(data[start:start+cut])
            start += cut
        pieces.append(data[start:])
        return b"".join(u32(len(p) | (0x80000000 if i == len(pieces)-1 else 0)) + p
                        for i, p in enumerate(pieces))

    @staticmethod
    def receive_exact(sock: socket.socket, count: int) -> bytes:
        parts = bytearray()
        while len(parts) < count:
            data = sock.recv(count - len(parts))
            if not data:
                raise ConnectionError("peer closed connection before completing RPC reply")
            parts.extend(data)
        return bytes(parts)

    @classmethod
    def receive_record(cls, sock: socket.socket) -> bytes:
        data = bytearray()
        for _ in range(1024):
            marker = struct.unpack("!I", cls.receive_exact(sock, 4))[0]
            length = marker & 0x7fffffff
            check(len(data) + length <= 2 * 1024 * 1024, "RPC reply exceeds client safety limit")
            data.extend(cls.receive_exact(sock, length))
            if marker & 0x80000000:
                return bytes(data)
        raise AssertionError("RPC reply has too many fragments")

    def stream(self, port: int) -> socket.socket:
        if port not in self.streams:
            self.streams[port] = socket.create_connection((self.host, port), self.timeout)
        return self.streams[port]

    def datagram(self) -> socket.socket:
        if self.udp is None:
            self.udp = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
            self.udp.settimeout(self.timeout)
        return self.udp

    def exchange(self, port: int, packet: bytes, xid: int,
                 cuts: list[int] | None = None, send_chunk: int = 0) -> Reply:
        try:
            if self.transport == "tcp":
                sock = self.stream(port)
                frame = self.framed(packet, cuts)
                if send_chunk:
                    for start in range(0, len(frame), send_chunk):
                        sock.sendall(frame[start:start+send_chunk])
                else:
                    sock.sendall(frame)
                data = self.receive_record(sock)
            else:
                sock = self.datagram()
                sock.sendto(packet, (self.host, port))
                data, peer = sock.recvfrom(65535)
                check(peer[1] == port, "UDP reply came from unexpected port")
            return self.parse(data, xid)
        except Exception:
            # A negative protocol check must not poison later checks/cleanup
            # with a closed TCP connection or a late UDP reply.
            if self.transport == "tcp":
                failed = self.streams.pop(port, None)
                if failed:
                    failed.close()
            elif self.udp:
                self.udp.close()
                self.udp = None
            raise

    def call_reply(self, port: int, program: int, version: int, procedure: int,
                   args: bytes = b"", **kwargs: Any) -> Reply:
        xid, packet = self.request(program, version, procedure, args, **kwargs)
        return self.exchange(port, packet, xid)

    def call(self, port: int, program: int, version: int, procedure: int,
             args: bytes = b"") -> bytes:
        reply = self.call_reply(port, program, version, procedure, args)
        check(reply.accepted and reply.code == 0,
              f"RPC failed: accepted={reply.accepted}, status={reply.code}")
        return reply.payload

    def close(self) -> None:
        for sock in self.streams.values():
            sock.close()
        self.streams.clear()
        if self.udp:
            self.udp.close()
            self.udp = None


class Client:
    def __init__(self, rpc: RPC, nfs_port: int, mount_port: int):
        self.rpc, self.nfs_port, self.mount_port = rpc, nfs_port, mount_port

    def nfs(self, procedure: int, args: bytes) -> tuple[int, XDR]:
        reader = XDR(self.rpc.call(self.nfs_port, NFS, 3, procedure, args))
        return reader.u32(), reader

    def mount(self, path: str) -> bytes:
        r = XDR(self.rpc.call(self.mount_port, MOUNT, 3, 1, string(path)))
        status = r.u32()
        check(status == OK, f"MOUNT {path}: {NAMES.get(status, status)}")
        handle = r.opaque(64)
        count = r.u32()
        check(count <= 16, f"unexpected authentication list size {count}")
        auth = [r.u32() for _ in range(count)]
        check(1 in auth, f"MOUNT {path} does not offer AUTH_SYS")
        check(len(handle) > 0, "MOUNT returned empty file handle")
        r.done()
        return handle

    def getattr(self, fh: bytes) -> tuple[int, dict[str, Any] | None]:
        status, r = self.nfs(1, opaque(fh))
        value = attr(r) if status == OK else None
        r.done()
        return status, value

    def lookup(self, fh: bytes, name: str | bytes) -> tuple[int, bytes | None]:
        status, r = self.nfs(3, opaque(fh) + string(name))
        handle = r.opaque(64) if status == OK else None
        if status == OK:
            post_attr(r)
        post_attr(r)
        r.done()
        return status, handle

    def read(self, fh: bytes, offset: int, count: int) -> tuple[int, bytes, bool]:
        status, r = self.nfs(6, opaque(fh) + u64(offset) + u32(count))
        post_attr(r)
        data, eof = b"", False
        if status == OK:
            actual = r.u32()
            eof = r.boolean()
            data = r.opaque()
            check(actual == len(data), "READ count differs from opaque data length")
            check(actual <= count, "READ returned more bytes than requested")
        r.done()
        return status, data, eof

    def write(self, fh: bytes, offset: int, data: bytes,
              stable: int = FILE_SYNC) -> tuple[int, int, int, bytes]:
        status, r = self.nfs(7, opaque(fh) + u64(offset) + u32(len(data)) +
                             u32(stable) + opaque(data))
        wcc(r)
        count, committed, verifier = 0, 0, b""
        if status == OK:
            count, committed, verifier = r.u32(), r.u32(), r.fixed(8)
            check(committed in (0, 1, 2), f"invalid WRITE committed {committed}")
            check(committed >= stable, "WRITE reported weaker durability than requested")
            check(count <= len(data), "WRITE returned count larger than supplied data")
        r.done()
        return status, count, committed, verifier

    def create(self, directory: bytes, name: str | bytes,
               mode: int = 1, verifier: bytes | None = None) -> tuple[int, bytes | None]:
        args = opaque(directory) + string(name) + u32(mode)
        args += verifier if mode == 2 and verifier is not None else sattr(0o644)
        status, r = self.nfs(8, args)
        return self._new_object(status, r, directory, name)

    def mkdir(self, directory: bytes, name: str) -> tuple[int, bytes | None]:
        status, r = self.nfs(9, opaque(directory) + string(name) + sattr(0o755))
        return self._new_object(status, r, directory, name)

    def _new_object(self, status: int, r: XDR, directory: bytes,
                    name: str | bytes) -> tuple[int, bytes | None]:
        handle = None
        if status == OK:
            handle = post_fh(r)
            post_attr(r)
        wcc(r)
        r.done()
        if status == OK and handle is None:
            lookup_status, handle = self.lookup(directory, name)
            check(lookup_status == OK and handle is not None, "successful create cannot be looked up")
        return status, handle

    def remove(self, directory: bytes, name: str | bytes, is_dir: bool = False) -> int:
        status, r = self.nfs(13 if is_dir else 12, opaque(directory) + string(name))
        wcc(r)
        r.done()
        return status

    def rename(self, source_dir: bytes, source: str, target_dir: bytes, target: str) -> int:
        status, r = self.nfs(14, opaque(source_dir) + string(source) +
                            opaque(target_dir) + string(target))
        wcc(r)
        wcc(r)
        r.done()
        return status

    def setattr_size(self, fh: bytes, size: int) -> int:
        status, r = self.nfs(2, opaque(fh) + sattr(size=size) + u32(0))
        wcc(r)
        r.done()
        return status

    def commit(self, fh: bytes, offset: int = 0, count: int = 0) -> tuple[int, bytes]:
        status, r = self.nfs(21, opaque(fh) + u64(offset) + u32(count))
        wcc(r)
        verifier = r.fixed(8) if status == OK else b""
        r.done()
        return status, verifier

    def fsinfo(self, fh: bytes) -> dict[str, int]:
        status, r = self.nfs(19, opaque(fh))
        post_attr(r)
        check(status == OK, f"FSINFO: {NAMES.get(status, status)}")
        result = {key: r.u32() for key in
                  ("rtmax", "rtpref", "rtmult", "wtmax", "wtpref", "wtmult", "dtpref")}
        result["maxfilesize"] = r.u64()
        r.fixed(8)
        result["properties"] = r.u32()
        r.done()
        check(0 < result["rtmax"] <= 2*1024*1024, "unreasonable rtmax for test client")
        check(0 < result["wtmax"] <= 2*1024*1024, "unreasonable wtmax for test client")
        return result

    def readdir(self, fh: bytes, cookie: int = 0, verifier: bytes = b"\0"*8,
                count: int = 1024, plus: bool = False) -> tuple[int, list[dict[str, Any]], bool, bytes]:
        args = opaque(fh) + u64(cookie) + verifier + u32(count)
        if plus:
            args += u32(count)
        raw = self.rpc.call(self.nfs_port, NFS, 3, 17 if plus else 16, args)
        r = XDR(raw)
        status = r.u32()
        post_attr(r)
        entries: list[dict[str, Any]] = []
        eof, returned_verifier = False, b""
        if status == OK:
            check(len(raw)-4 <= count,
                  f"READDIR{'PLUS' if plus else ''} resok uses {len(raw)-4} bytes, limit {count}")
            returned_verifier = r.fixed(8)
            while r.boolean():
                check(len(entries) < 10000, "unbounded directory result")
                entry = {"fileid": r.u64(), "name": r.string(255), "cookie": r.u64()}
                if plus:
                    entry["attr"] = post_attr(r)
                    entry["fh"] = post_fh(r)
                entries.append(entry)
            eof = r.boolean()
        r.done()
        return status, entries, eof, returned_verifier

    def list_all(self, fh: bytes, count: int = 1024,
                 plus: bool = False) -> tuple[list[dict[str, Any]], int, bytes]:
        entries: list[dict[str, Any]] = []
        cookie, verifier = 0, b"\0"*8
        visited: set[int] = set()
        for page in range(1, 501):
            status, batch, eof, verifier = self.readdir(fh, cookie, verifier, count, plus)
            check(status == OK, f"READDIR page {page}: {NAMES.get(status, status)}")
            entries.extend(batch)
            if eof:
                return entries, page, verifier
            check(bool(batch), "READDIR returned no entries without EOF")
            cookie = batch[-1]["cookie"]
            check(cookie not in visited and cookie != 0, "READDIR cookie does not advance")
            visited.add(cookie)
        raise AssertionError("READDIR did not finish within 500 pages")


class Suite:
    def __init__(self, args: argparse.Namespace):
        self.args = args
        self.results: list[dict[str, Any]] = []
        self.run_id = uuid.uuid4().hex[:12]

    def test(self, transport: str, name: str, fn: Callable[[], Any]) -> bool:
        start = time.monotonic()
        result: dict[str, Any] = {"transport": transport, "name": name}
        try:
            detail = fn()
            result["status"] = "pass"
            if detail is not None:
                result["detail"] = detail
        except Exception as exc:
            result["status"] = "fail"
            result["error"] = f"{type(exc).__name__}: {exc}"[:1200]
        result["duration_ms"] = round((time.monotonic()-start)*1000, 2)
        self.results.append(result)
        return result["status"] == "pass"

    @staticmethod
    def must_status(status: int, expected: int = OK, operation: str = "operation") -> None:
        check(status == expected,
              f"{operation}: expected {NAMES.get(expected, expected)}, got {NAMES.get(status, status)}")

    def run_transport(self, transport: str) -> None:
        a = self.args
        rpc = RPC(a.host, transport, a.timeout)
        c = Client(rpc, a.nfs_port, a.mount_port)
        state: dict[str, Any] = {}
        own_name = f"protocol-smoke-{self.run_id}-{transport}"

        def discovery() -> dict[str, Any]:
            protocol = 6 if transport == "tcp" else 17
            for program, version, expected in ((MOUNT, 3, a.mount_port), (NFS, 3, a.nfs_port)):
                r = XDR(rpc.call(a.pmap_port, PMAP, 2, 3,
                                b"".join(u32(v) for v in (program, version, protocol, 0))))
                port = r.u32()
                r.done()
                check(port == expected, f"GETPORT {program}/3 returned {port}, expected {expected}")
            for program, version, protocol in ((999999, 1, protocol), (NFS, 999, protocol), (NFS, 3, 255)):
                r = XDR(rpc.call(a.pmap_port, PMAP, 2, 3,
                                b"".join(u32(v) for v in (program, version, protocol, 0))))
                check(r.u32() == 0, "GETPORT returned a port for unsupported mapping")
                r.done()
            r = XDR(rpc.call(a.pmap_port, PMAP, 2, 4))
            mappings = []
            while r.boolean():
                check(len(mappings) < 64, "unbounded portmapper DUMP")
                mappings.append(tuple(r.u32() for _ in range(4)))
            r.done()
            check((NFS, 3, 6 if transport == "tcp" else 17, a.nfs_port) in mappings,
                  "NFS mapping missing from DUMP")
            return {"registered_mappings": len(mappings)}

        self.test(transport, "portmapper.discovery", discovery)

        def mounts() -> dict[str, Any]:
            state["root"] = c.mount(a.share)
            state["ro"] = c.mount(a.readonly)
            for key in ("root", "ro"):
                status, info = c.getattr(state[key])
                self.must_status(status, operation="mounted root GETATTR")
                check(info is not None and info["type"] == 2, "export root is not a directory")
            r = XDR(rpc.call(a.mount_port, MOUNT, 3, 5))
            exported = []
            while r.boolean():
                check(len(exported) < 1000, "unbounded MOUNT EXPORT list")
                exported.append(r.string())
                groups = 0
                while r.boolean():
                    r.string(255)
                    groups += 1
                    check(groups < 1000, "unbounded MOUNT group list")
            r.done()
            check(a.share in exported and a.readonly in exported, "configured shares missing from EXPORT")
            r = XDR(rpc.call(a.mount_port, MOUNT, 3, 1, string("/no-such-" + self.run_id)))
            check(r.u32() != 0, "MOUNT accepted missing export")
            r.done()
            return {"exports": exported}

        if not self.test(transport, "mount.exports_and_root", mounts):
            rpc.close()
            return

        def fixture_read() -> None:
            for key in ("root", "ro"):
                status, fh = c.lookup(state[key], a.fixture_name)
                self.must_status(status, operation=f"{key} LOOKUP fixture")
                check(fh is not None, "missing fixture handle")
                status, data, eof = c.read(fh, 0, 8192)
                self.must_status(status, operation="READ fixture")
                check(data == a.fixture_text.encode(), f"fixture differs in {key} export")
                check(eof, "small fixture did not return EOF")
                state[key + "_fixture"] = fh

        self.test(transport, "read.existing_fixtures", fixture_read)

        def setup() -> None:
            status, fh = c.mkdir(state["root"], own_name)
            self.must_status(status, operation="create test directory")
            check(fh is not None, "test directory missing handle")
            state["work"] = fh
            state["fsinfo"] = c.fsinfo(fh)

        if not self.test(transport, "setup.owned_directory", setup):
            rpc.close()
            return

        def roundtrip() -> dict[str, Any]:
            status, fh = c.create(state["work"], "binary-roundtrip.bin")
            self.must_status(status, operation="CREATE binary")
            check(fh is not None, "CREATE omitted usable handle")
            payload = bytes(range(256)) * 763 + b"\x00end\xff\n"
            info = state["fsinfo"]
            chunk = min(8192, info["wtmax"], info["rtmax"])
            verifier = None
            for offset in range(0, len(payload), chunk):
                data = payload[offset:offset+chunk]
                status, count, committed, current = c.write(fh, offset, data)
                self.must_status(status, operation="WRITE binary")
                check(count == len(data), "short WRITE in ordinary transfer")
                if verifier is not None:
                    check(current == verifier, "write verifier changed without server restart")
                verifier = current
            status, current = c.commit(fh)
            self.must_status(status, operation="COMMIT binary")
            check(current == verifier, "COMMIT verifier differs from WRITE")
            output = bytearray()
            for offset in range(0, len(payload), chunk):
                status, data, eof = c.read(fh, offset, min(chunk, len(payload)-offset))
                self.must_status(status, operation="READ binary")
                check(len(data) == min(chunk, len(payload)-offset), "short ordinary READ")
                output.extend(data)
            check(hashlib.sha256(output).digest() == hashlib.sha256(payload).digest(), "roundtrip SHA256 mismatch")
            status, attrs = c.getattr(fh)
            self.must_status(status)
            check(attrs is not None and attrs["size"] == len(payload), "GETATTR size differs from data")
            state["binary"], state["payload"] = fh, payload
            return {"bytes": len(payload), "sha256": hashlib.sha256(output).hexdigest(), "chunk_bytes": chunk}

        self.test(transport, "files.binary_roundtrip_and_commit", roundtrip)

        def metadata_and_limits() -> None:
            for key in ("work", "ro"):
                status, r = c.nfs(4, opaque(state[key]) + u32(0x3f))
                post_attr(r)
                self.must_status(status, operation="ACCESS")
                access = r.u32()
                r.done()
                check(access & ~0x3f == 0, "ACCESS returned undefined flags")
                if key == "ro":
                    check(access & 0x1c == 0, "read-only export advertises MODIFY/EXTEND/DELETE")
            status, r = c.nfs(18, opaque(state["work"]))
            post_attr(r)
            self.must_status(status, operation="FSSTAT")
            tbytes, fbytes, abytes, tfiles, ffiles, afiles = (r.u64() for _ in range(6))
            r.u32()
            r.done()
            check(abytes <= fbytes <= tbytes, "FSSTAT space counts are inconsistent")
            check(afiles <= ffiles <= tfiles, "FSSTAT file counts are inconsistent")
            status, r = c.nfs(20, opaque(state["work"]))
            post_attr(r)
            self.must_status(status, operation="PATHCONF")
            r.u32()
            name_max = r.u32()
            for _ in range(4):
                r.boolean()
            r.done()
            check(0 < name_max <= 255, "PATHCONF reports unexpected filename limit")
            check(state["fsinfo"]["properties"] & 3 == 0,
                  "FSINFO advertises unsupported hard links or symbolic links")
            info, fh, payload = state["fsinfo"], state["binary"], state["payload"]
            status, data, _ = c.read(fh, 0, info["rtmax"]+1)
            check(status in (OK, INVAL), f"over-limit READ returned unexpected status {status}")
            if status == OK:
                check(len(data) <= info["rtmax"], "READ exceeds advertised rtmax")
                check(data == payload[:len(data)], "over-limit READ changed data")
            if info["wtmax"] < len(payload) and info["wtmax"] < 60000:
                status, count, _, _ = c.write(fh, 0, payload[:info["wtmax"]+1])
                check(status in (OK, INVAL), f"over-limit WRITE returned unexpected status {status}")
                if status == OK:
                    check(count <= info["wtmax"], "WRITE exceeds advertised wtmax")

        self.test(transport, "filesystem.access_space_pathconf_and_io_limits", metadata_and_limits)

        def io_edges() -> None:
            fh, payload = state["binary"], state["payload"]
            for offset, count in ((0, 0), (len(payload), 1), (len(payload)+100, 17)):
                status, data, eof = c.read(fh, offset, count)
                self.must_status(status, operation="READ boundary")
                check(data == b"", "READ boundary returned unexpected data")
                if offset >= len(payload):
                    check(eof, "READ beyond EOF did not set EOF")
            for stable in (0, 1, 2):
                status, count, committed, verifier = c.write(fh, 0, payload[:11], stable)
                self.must_status(status, operation="WRITE stable mode")
                check(count == 11, "WRITE stable mode returned short count")
                status, after = c.commit(fh, 0, 11)
                self.must_status(status)
                check(after == verifier, "range COMMIT verifier mismatch")
            status, count, _, _ = c.write(fh, len(payload)+1000, b"")
            self.must_status(status, operation="zero-byte WRITE")
            check(count == 0, "zero-byte WRITE count is nonzero")
            self.must_status(c.setattr_size(fh, 19), operation="SETATTR truncate")
            status, data, eof = c.read(fh, 0, 8192)
            self.must_status(status)
            check(data == payload[:19] and eof, "truncate did not change file contents/EOF")

        self.test(transport, "files.eof_zero_length_durability_truncate", io_edges)

        def names_and_create() -> None:
            for name in ("含 空格.txt", "empty.bin"):
                status, fh = c.create(state["work"], name)
                self.must_status(status, operation="CREATE Unicode/empty")
                status, data, eof = c.read(fh, 0, 17)
                self.must_status(status)
                check(data == b"" and eof, "new file not empty")
                status, _ = c.create(state["work"], name)
                self.must_status(status, EXIST, "GUARDED CREATE existing")
            verifier = bytes.fromhex("c0ffee0000000001")
            status, fh = c.create(state["work"], "exclusive.bin", 2, verifier)
            if a.exclusive_create == "unsupported":
                self.must_status(status, NOTSUPP, "EXCLUSIVE CREATE declared unsupported")
                return
            self.must_status(status, operation="EXCLUSIVE CREATE")
            status, _, _, _ = c.write(fh, 0, b"keep this data")
            self.must_status(status)
            status, _ = c.create(state["work"], "exclusive.bin", 2, verifier)
            self.must_status(status, operation="EXCLUSIVE CREATE same verifier")
            status, data, _ = c.read(fh, 0, 100)
            self.must_status(status)
            check(data == b"keep this data", "EXCLUSIVE retry truncated file")
            status, _ = c.create(state["work"], "exclusive.bin", 2, b"differnt")
            self.must_status(status, EXIST, "EXCLUSIVE CREATE different verifier")

        self.test(transport, "files.unicode_empty_and_create_modes", names_and_create)

        def identities() -> None:
            status, parent = c.mkdir(state["work"], "parent-before")
            self.must_status(status)
            status, child = c.create(parent, "child-before.txt")
            self.must_status(status)
            status, _, _, _ = c.write(child, 0, b"identity survives renames")
            self.must_status(status)
            self.must_status(c.rename(parent, "child-before.txt", parent, "child-after.txt"), operation="rename file")
            status, data, _ = c.read(child, 0, 100)
            self.must_status(status, operation="READ old handle after file rename")
            check(data == b"identity survives renames", "file rename changed handle identity")
            self.must_status(c.rename(state["work"], "parent-before", state["work"], "parent-after"), operation="rename parent")
            status, data, _ = c.read(child, 0, 100)
            self.must_status(status, operation="READ old handle after parent rename")
            check(data == b"identity survives renames", "parent rename changed child identity")
            status, _ = c.lookup(parent, "child-after.txt")
            self.must_status(status, operation="LOOKUP using renamed parent handle")
            self.must_status(c.remove(parent, "child-after.txt"))
            status, replacement = c.create(parent, "child-after.txt")
            self.must_status(status)
            status, _ = c.getattr(child)
            check(status in (STALE, BADHANDLE), f"old deleted handle returned {NAMES.get(status, status)}")
            check(replacement != child, "replacement reused deleted file handle")
            status = c.remove(state["work"], "parent-after", True)
            self.must_status(status, 66, "RMDIR nonempty")
            self.must_status(c.remove(parent, "child-after.txt"))
            self.must_status(c.remove(state["work"], "parent-after", True))

        self.test(transport, "files.rename_parent_and_stale_identity", identities)

        def directory_identities() -> None:
            status, parent = c.mkdir(state["work"], "tree-before")
            self.must_status(status)
            status, child = c.mkdir(parent, "child")
            self.must_status(status)
            status, grandchild = c.mkdir(child, "grandchild")
            self.must_status(status)
            status, payload = c.create(grandchild, "payload.bin")
            self.must_status(status)
            status, _, _, _ = c.write(payload, 0, b"nested-directory-identity")
            self.must_status(status)

            def old_handles_work() -> None:
                status, found = c.lookup(grandchild, "payload.bin")
                self.must_status(status, operation="LOOKUP using moved grandchild handle")
                status, data, _ = c.read(payload, 0, 128)
                self.must_status(status, operation="READ using moved descendant file handle")
                check(data == b"nested-directory-identity", "directory move changed descendant file")
                status, info = c.getattr(found)
                self.must_status(status)
                old_status, old_info = c.getattr(payload)
                self.must_status(old_status)
                check(info is not None and old_info is not None and
                      (info["fsid"], info["fileid"]) == (old_info["fsid"], old_info["fileid"]),
                      "LOOKUP after directory move found a different file")

            self.must_status(c.rename(state["work"], "tree-before", state["work"], "tree-after"),
                             operation="rename ancestor with cached child-directory handles")
            old_handles_work()
            status, _ = c.mkdir(child, "created-through-old-handle")
            self.must_status(status, operation="MKDIR through renamed child-directory handle")
            self.must_status(c.remove(child, "created-through-old-handle", True),
                             operation="RMDIR through renamed child-directory handle")

            status, destination = c.mkdir(state["work"], "tree-destination")
            self.must_status(status)
            self.must_status(c.rename(child, "grandchild", destination, "moved-grandchild"),
                             operation="move directory across parents")
            old_handles_work()
            status, old_empty = c.mkdir(destination, "replace-empty")
            self.must_status(status)
            self.must_status(c.rename(destination, "moved-grandchild", destination, "replace-empty"),
                             operation="rename directory over an existing empty directory")
            old_handles_work()
            status, _ = c.getattr(old_empty)
            check(status in (STALE, BADHANDLE), "replaced destination-directory handle was not invalidated")
            self.must_status(c.remove(destination, "replace-empty", True), 66,
                             "RMDIR populated moved directory")
            self.must_status(c.remove(grandchild, "payload.bin"))
            self.must_status(c.remove(destination, "replace-empty", True),
                             operation="RMDIR while client retains directory handle")
            status, _ = c.getattr(grandchild)
            check(status in (STALE, BADHANDLE), "removed directory handle did not become stale")
            self.must_status(c.remove(parent, "child", True))
            status, _ = c.getattr(child)
            check(status in (STALE, BADHANDLE), "removed child-directory handle did not become stale")
            self.must_status(c.remove(state["work"], "tree-after", True))
            self.must_status(c.remove(state["work"], "tree-destination", True))

        self.test(transport, "directories.move_replace_rmdir_with_existing_handles", directory_identities)

        def rejected_create_attributes() -> dict[str, Any]:
            cases = {
                "unmapped-uid": requested_sattr(uid=0x7fffffff),
                "unmapped-gid": requested_sattr(gid=0x7fffffff),
                "invalid-atime-nsec": requested_sattr(atime=(1, 1_000_000_000)),
                "invalid-mtime-nsec": requested_sattr(mtime=(1, 1_000_000_000)),
                "unrepresentable-size": requested_sattr(size=0xffffffffffffffff),
            }
            failures, observed = [], []
            for directory in (False, True):
                operation = "MKDIR" if directory else "CREATE"
                for case_name, attrs in cases.items():
                    name = f"rejected-{operation.lower()}-{case_name}"
                    args = opaque(state["work"]) + string(name)
                    if not directory:
                        args += u32(1)  # GUARDED
                    args += attrs
                    reply = rpc.call_reply(a.nfs_port, NFS, 3, 9 if directory else 8, args)
                    rejected = False
                    outcome: str | int = f"RPC accepted={reply.accepted} status={reply.code}"
                    if reply.accepted and reply.code == 0:
                        reader = XDR(reply.payload)
                        status = reader.u32()
                        outcome = NAMES.get(status, status)
                        c._new_object(status, reader, state["work"], name)
                        rejected = status != OK
                    elif reply.accepted and reply.code == 4:
                        rejected = True
                        outcome = "RPC_GARBAGE_ARGS"
                    else:
                        failures.append(f"{operation} {case_name} returned unexpected {outcome}")
                    status, _ = c.lookup(state["work"], name)
                    observed.append({"operation": operation, "attributes": case_name,
                                     "outcome": outcome, "left_object": status == OK})
                    if status == OK:
                        # This name belongs solely to the test; clean up even
                        # when a failed create leaked the filesystem object.
                        c.remove(state["work"], name, directory)
                    if not rejected:
                        failures.append(f"{operation} accepted {case_name}")
                    if status != NOENT:
                        failures.append(f"{operation} {case_name} left a name after rejection ({NAMES.get(status, status)})")
            check(not failures, "; ".join(failures))
            return {"rejections_without_residual_objects": len(observed), "cases": observed}

        self.test(transport, "atomicity.rejected_create_and_mkdir_attributes_leave_no_objects", rejected_create_attributes)

        if a.windows_case_rename:
            def windows_case_identity() -> None:
                status, fh = c.create(state["work"], "MiXeD-Case.txt")
                self.must_status(status)
                status, _, _, _ = c.write(fh, 0, b"case-variant-file")
                self.must_status(status)
                self.must_status(c.rename(state["work"], "mixed-case.TXT", state["work"], "renamed-case.txt"),
                                 operation="rename file through case-variant source name")
                self.must_status(c.rename(state["work"], "RENAMED-case.TXT", state["work"], "ReNaMeD-CASE.txt"),
                                 operation="case-only file rename")
                status, data, _ = c.read(fh, 0, 100)
                self.must_status(status, operation="READ old handle after case-variant/case-only rename")
                check(data == b"case-variant-file", "case rename changed file identity")

                status, parent = c.mkdir(state["work"], "PaReNt-MiXeD")
                self.must_status(status)
                status, child = c.mkdir(parent, "ChIlD-MiXeD")
                self.must_status(status)
                status, child_file = c.create(child, "CasePayload.bin")
                self.must_status(status)
                status, _, _, _ = c.write(child_file, 0, b"case-variant-descendant")
                self.must_status(status)
                self.must_status(c.rename(state["work"], "parent-mixed", state["work"], "PARENT-RENAMED"),
                                 operation="rename parent through case-variant source name")
                self.must_status(c.rename(state["work"], "PARENT-RENAMED", state["work"], "parent-renamed"),
                                 operation="case-only parent rename")
                status, data, _ = c.read(child_file, 0, 100)
                self.must_status(status, operation="READ descendant handle after parent case rename")
                check(data == b"case-variant-descendant", "parent case rename changed descendant identity")
                status, _ = c.lookup(child, "casepayload.BIN")
                self.must_status(status, operation="case-insensitive LOOKUP through old child handle")
                self.must_status(c.remove(child, "casepayload.BIN"))
                self.must_status(c.remove(parent, "child-MIXED", True))
                self.must_status(c.remove(state["work"], "PARENT-renamed", True))

            self.test(transport, "windows.case_variant_rename_preserves_existing_handles", windows_case_identity)

        def readonly() -> None:
            errors = []
            for kind in ("file", "directory"):
                name = f"must-not-create-{self.run_id}-{kind}"
                status, _ = (c.create(state["ro"], name) if kind == "file"
                             else c.mkdir(state["ro"], name))
                if status == OK:
                    c.remove(state["ro"], name, kind == "directory")
                if status != ROFS:
                    errors.append(f"{kind} create returned {NAMES.get(status, status)}")
            fh = state.get("ro_fixture")
            if fh is not None:
                # If a broken server permits this, bytes and size remain unchanged.
                original = a.fixture_text.encode()
                status, _, _, _ = c.write(fh, 0, original)
                if status != ROFS:
                    errors.append(f"WRITE returned {NAMES.get(status, status)}")
                status = c.setattr_size(fh, len(original))
                if status != ROFS:
                    errors.append(f"SETATTR returned {NAMES.get(status, status)}")
            check(not errors, "; ".join(errors))

        self.test(transport, "exports.readonly_mutations_denied", readonly)

        def pagination() -> dict[str, int]:
            status, directory = c.mkdir(state["work"], "pagination")
            self.must_status(status)
            expected = {f"entry-{i:03d}.txt" for i in range(47)}
            for name in sorted(expected):
                status, _ = c.create(directory, name)
                self.must_status(status, operation="CREATE pagination fixture")
            result = {}
            for plus in (False, True):
                entries, pages, verifier = c.list_all(directory, 512, plus)
                actual = [e["name"] for e in entries if e["name"] not in (".", "..")]
                check(set(actual) == expected and len(actual) == len(expected), "pagination lost or duplicated filenames")
                check(pages > 1, "small response limit did not produce multiple pages")
                if plus:
                    for e in entries:
                        if e.get("fh") is not None and e["name"] not in (".", ".."):
                            status, attrs = c.getattr(e["fh"])
                            self.must_status(status)
                            check(attrs is not None and attrs["fileid"] == e["fileid"], "READDIRPLUS fileid/handle mismatch")
                result["readdirplus_pages" if plus else "readdir_pages"] = pages
                status, _, _, _ = c.readdir(directory, 0x7fffffffffffffff, verifier, 512, plus)
                self.must_status(status, BAD_COOKIE, "READDIR invalid cookie")
                status, _, _, _ = c.readdir(directory, 0, b"\0"*8, 1, plus)
                self.must_status(status, TOOSMALL, "READDIR impossible response limit")
            return result

        self.test(transport, "directories.pagination_cookies_and_limits", pagination)

        def path_boundaries() -> dict[str, int]:
            bad = ["", ".", "..", "../escape-" + self.run_id,
                   "..\\escape-" + self.run_id, "/escape-" + self.run_id,
                   "C:\\escape-" + self.run_id, "note:stream", "CON", "NUL.txt",
                   "aux", "trailing.", "trailing ", "x"*256, b"bad\0name", b"\xff"]
            accepted = []
            for name in bad:
                status, _ = c.create(state["work"], name)
                if status == OK:
                    accepted.append(repr(name))
                    # Only names created by this test are removed. '.'/'..'
                    # cannot be new regular files and are never removed.
                    if name not in ("", ".", ".."):
                        c.remove(state["work"], name)
            check(not accepted, "unsafe Windows/path names accepted: " + ", ".join(accepted))
            status, parent = c.lookup(state["root"], "..")
            if status == OK:
                root_status, root_attrs = c.getattr(state["root"])
                parent_status, parent_attrs = c.getattr(parent)
                check(root_status == parent_status == OK and root_attrs is not None and parent_attrs is not None,
                      "could not inspect root '..' result")
                check((root_attrs["fsid"], root_attrs["fileid"]) ==
                      (parent_attrs["fsid"], parent_attrs["fileid"]), "root '..' escapes export")
            else:
                check(status in (NOENT, ACCES, INVAL), f"unexpected root '..' error {status}")
            status, _ = c.getattr(os.urandom(32))
            check(status in (STALE, BADHANDLE), f"random handle returned {status}")
            return {"rejected_names": len(bad)}

        self.test(transport, "security.names_root_boundary_and_unknown_handle", path_boundaries)

        def rpc_errors() -> None:
            trials = [
                ("RPC version", NFS, 3, 0, b"", {"rpc_version": 1}, False, 0),
                ("program", 999999, 3, 0, b"", {}, True, 1),
                ("program version", NFS, 99, 0, b"", {}, True, 2),
                ("procedure", NFS, 3, 9999, b"", {}, True, 3),
                ("truncated handle", NFS, 3, 1, u32(64) + b"x", {}, True, 4),
                ("oversized opaque claim", NFS, 3, 1, u32(16*1024*1024), {}, True, 4),
            ]
            for name, program, version, proc, payload, kwargs, accepted, code in trials:
                reply = rpc.call_reply(a.nfs_port, program, version, proc, payload, **kwargs)
                check((reply.accepted, reply.code) == (accepted, code),
                      f"{name}: expected RPC {accepted}/{code}, got {reply.accepted}/{reply.code}")
                if code == 0 and not accepted:
                    r = XDR(reply.payload)
                    check(r.u32() == 2 and r.u32() == 2, "RPC_MISMATCH range is wrong")
                    r.done()
                self._null(rpc, a.nfs_port)
            bad_fh = opaque(state["work"]) + u32(16*1024*1024)
            reply = rpc.call_reply(a.nfs_port, NFS, 3, 3, bad_fh)
            check(reply.accepted and reply.code == 4, "truncated/oversized filename did not return GARBAGE_ARGS")
            self._null(rpc, a.nfs_port)

        self.test(transport, "rpc.errors_truncated_xdr_and_allocation_claims", rpc_errors)

        if transport == "tcp":
            self.test(transport, "rpc.tcp_fragments_and_coalesced_records", lambda: self.tcp_framing(rpc))
            self.test(transport, "rpc.tcp_record_size_limit", self.tcp_limit)
        else:
            self.test(transport, "rpc.udp_duplicate_create_remove_rename", lambda: self.udp_duplicates(c, state["work"]))

        def clean() -> None:
            self.cleanup_directory(c, state["work"])
            self.must_status(c.remove(state["root"], own_name, True), operation="remove owned test directory")
            for path in (a.share, a.readonly):
                r = XDR(rpc.call(a.mount_port, MOUNT, 3, 3, string(path)))
                r.done()

        self.test(transport, "cleanup.owned_test_data", clean)
        rpc.close()

    @staticmethod
    def _null(rpc: RPC, port: int) -> None:
        r = XDR(rpc.call(port, NFS, 3, 0))
        r.done()

    def tcp_framing(self, rpc: RPC) -> None:
        xid, packet = rpc.request(NFS, 3, 0, auth_none=True)
        reply = rpc.exchange(self.args.nfs_port, packet, xid, cuts=[1, 7, 0, 5], send_chunk=1)
        check(reply.accepted and reply.code == 0 and reply.payload == b"", "fragmented NULL failed")
        first_id, first = rpc.request(NFS, 3, 0)
        second_id, second = rpc.request(NFS, 3, 0)
        sock = rpc.stream(self.args.nfs_port)
        sock.sendall(RPC.framed(first) + RPC.framed(second))
        seen = set()
        for _ in range(2):
            data = RPC.receive_record(sock)
            xid = struct.unpack("!I", data[:4])[0]
            check(xid in (first_id, second_id) and xid not in seen, "coalesced replies have unexpected XID")
            seen.add(xid)
            reply = RPC.parse(data, xid)
            check(reply.accepted and reply.code == 0 and reply.payload == b"", "coalesced NULL failed")

    def tcp_limit(self) -> dict[str, int]:
        limit = self.args.max_record_bytes
        with socket.create_connection((self.args.host, self.args.nfs_port), self.args.timeout) as sock:
            sock.settimeout(self.args.timeout)
            sock.sendall(u32(0x80000000 | (limit + 1)))
            try:
                data = sock.recv(4)
                check(data == b"", "oversized TCP record produced data rather than closing")
            except ConnectionResetError:
                pass
        probe = RPC(self.args.host, "tcp", self.args.timeout)
        try:
            self._null(probe, self.args.nfs_port)
        finally:
            probe.close()
        return {"declared_bytes": limit+1, "server_limit_bytes": limit}

    def udp_duplicates(self, c: Client, work: bytes) -> None:
        rpc = c.rpc

        def repeated(proc: int, args: bytes) -> bytes:
            xid, packet = rpc.request(NFS, 3, proc, args)
            # A server may discard a duplicate while the first call is still
            # executing. It must replay the completed result for a later retry.
            # An isolated socket prevents optional duplicate responses from
            # polluting subsequent, unrelated RPC calls.
            with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as sock:
                sock.settimeout(self.args.timeout)
                endpoint = (self.args.host, self.args.nfs_port)
                sock.sendto(packet, endpoint)
                sock.sendto(packet, endpoint)
                first = sock.recvfrom(65535)[0]
                reply = RPC.parse(first, xid)
                check(reply.accepted and reply.code == 0, "duplicate request did not complete")
                check(XDR(reply.payload).u32() == OK, f"duplicate procedure {proc} failed")
                sock.settimeout(0.05)
                try:
                    concurrent = sock.recvfrom(65535)[0]
                    check(concurrent == first, f"concurrent duplicate procedure {proc} changed RPC response")
                except socket.timeout:
                    pass
                sock.settimeout(self.args.timeout)
                sock.sendto(packet, endpoint)
                retried = sock.recvfrom(65535)[0]
                check(retried == first, f"completed duplicate procedure {proc} did not replay original result")
                return reply.payload

        repeated(8, opaque(work) + string("duplicate-create.txt") + u32(1) + sattr(0o644))
        status, fh = c.lookup(work, "duplicate-create.txt")
        self.must_status(status)
        status, _, _, _ = c.write(fh, 0, b"exactly once")
        self.must_status(status)
        repeated(14, opaque(work) + string("duplicate-create.txt") + opaque(work) + string("duplicate-renamed.txt"))
        status, data, _ = c.read(fh, 0, 100)
        self.must_status(status)
        check(data == b"exactly once", "duplicate rename changed data")
        repeated(12, opaque(work) + string("duplicate-renamed.txt"))
        status, _ = c.lookup(work, "duplicate-renamed.txt")
        self.must_status(status, NOENT)

        # Reusing an XID with different arguments must not return an unrelated
        # cached result. Verify both objects exist, not just reply status.
        xid = rpc.next_xid()
        for name in ("same-xid-a.txt", "same-xid-b.txt"):
            _, packet = rpc.request(NFS, 3, 8, opaque(work) + string(name) + u32(1) + sattr(0o644), xid=xid)
            reply = rpc.exchange(self.args.nfs_port, packet, xid)
            check(reply.accepted and reply.code == 0 and XDR(reply.payload).u32() == OK, "XID reuse failed")
            status, _ = c.lookup(work, name)
            self.must_status(status, operation="LOOKUP after same-XID distinct request")

    def cleanup_directory(self, c: Client, fh: bytes, depth: int = 0) -> None:
        check(depth < 12, "cleanup recursion limit")
        entries, _, _ = c.list_all(fh, 8192)
        check(len(entries) < 2000, "refusing unexpectedly large cleanup")
        for entry in entries:
            name = entry["name"]
            if name in (".", ".."):
                continue
            status, child = c.lookup(fh, name)
            self.must_status(status, operation="cleanup LOOKUP")
            status, attributes = c.getattr(child)
            self.must_status(status, operation="cleanup GETATTR")
            is_dir = attributes is not None and attributes["type"] == 2
            if is_dir:
                self.cleanup_directory(c, child, depth+1)
            self.must_status(c.remove(fh, name, is_dir), operation="cleanup REMOVE/RMDIR")

    def run(self) -> dict[str, Any]:
        started = time.monotonic()
        transports = ("tcp", "udp") if self.args.transport == "both" else (self.args.transport,)
        for transport in transports:
            try:
                self.run_transport(transport)
            except Exception as exc:
                self.results.append({"transport": transport, "name": "suite.unexpected_exception",
                                     "status": "fail", "error": f"{type(exc).__name__}: {exc}"})
        passed = sum(r["status"] == "pass" for r in self.results)
        return {"schema_version": 1, "suite": "independent-nfsv3-protocol-smoke",
                "run_id": self.run_id, "host": self.args.host,
                "profile": {"windows_case_rename": self.args.windows_case_rename,
                            "exclusive_create": self.args.exclusive_create,
                            "max_record_bytes": self.args.max_record_bytes},
                "ports": {"portmapper": self.args.pmap_port, "mount": self.args.mount_port,
                          "nfs": self.args.nfs_port},
                "passed": passed, "failed": len(self.results)-passed,
                "duration_seconds": round(time.monotonic()-started, 3), "tests": self.results}


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--host", default="127.0.0.1")
    parser.add_argument("--pmap-port", type=int, default=111)
    parser.add_argument("--mount-port", type=int, default=20048)
    parser.add_argument("--nfs-port", type=int, default=2049)
    parser.add_argument("--transport", choices=("tcp", "udp", "both"), default="both")
    parser.add_argument("--timeout", type=float, default=5.0)
    parser.add_argument("--share", default="/share")
    parser.add_argument("--readonly", default="/readonly")
    parser.add_argument("--fixture-name", default="fixture.txt")
    parser.add_argument("--fixture-text", default="RustNFS fixture\n")
    parser.add_argument("--max-record-bytes", type=int, default=2*1024*1024,
                        help="configured server RPC/TCP record limit (for rejection check)")
    parser.add_argument("--exclusive-create", choices=("unsupported", "supported"), default="unsupported",
                        help="declared EXCLUSIVE CREATE capability; unsupported must return NOTSUPP")
    parser.add_argument("--windows-case-rename", action="store_true",
                        help="also require Windows case-insensitive lookup/rename and case-only rename")
    args = parser.parse_args()
    for port in (args.pmap_port, args.mount_port, args.nfs_port):
        parser.error("ports must be in 1..65535") if not 1 <= port <= 65535 else None
    if not 0 < args.max_record_bytes < 0x7fffffff:
        parser.error("max-record-bytes must fit a positive RPC record length")
    result = Suite(args).run()
    print(json.dumps(result, ensure_ascii=False, indent=2))
    return 0 if result["failed"] == 0 else 1


if __name__ == "__main__":
    sys.exit(main())

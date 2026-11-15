"""Minimal obs-websocket v5 client (stdlib only) for driving OBS and Lightair alike."""
import base64, hashlib, json, os, socket, struct, time

class ObsWs:
    def __init__(self, host="127.0.0.1", port=4455, password=None, timeout=30):
        self.s = socket.create_connection((host, port), timeout=timeout)
        key = base64.b64encode(os.urandom(16)).decode()
        self.s.sendall((f"GET / HTTP/1.1\r\nHost: {host}:{port}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n"
                        f"Sec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Protocol: obswebsocket.json\r\n\r\n").encode())
        head = b""
        while b"\r\n\r\n" not in head:
            head += self.s.recv(1)
        assert b" 101 " in head.split(b"\r\n")[0], head
        self.buf = b""
        self.events = []
        self.n = 0
        hello = self.recv()
        d = {"rpcVersion": 1, "eventSubscriptions": 0x7FF}
        auth = hello["d"].get("authentication")
        if auth:
            secret = base64.b64encode(hashlib.sha256((password + auth["salt"]).encode()).digest()).decode()
            d["authentication"] = base64.b64encode(hashlib.sha256((secret + auth["challenge"]).encode()).digest()).decode()
        self.send({"op": 1, "d": d})
        r = self.recv()
        assert r["op"] == 2, r

    def _read(self, n):
        while len(self.buf) < n:
            c = self.s.recv(65536)
            if not c:
                raise ConnectionError("closed")
            self.buf += c
        out, self.buf = self.buf[:n], self.buf[n:]
        return out

    def recv(self):
        while True:
            b0, b1 = self._read(2)
            op, ln = b0 & 0x0F, b1 & 0x7F
            if ln == 126: ln = struct.unpack(">H", self._read(2))[0]
            elif ln == 127: ln = struct.unpack(">Q", self._read(8))[0]
            data = self._read(ln)
            if op == 8:
                raise ConnectionError(f"closed: {struct.unpack('>H', data[:2])[0] if len(data)>=2 else ''} {data[2:]}")
            if op == 9:
                self._frame(10, data); continue
            if op == 1:
                return json.loads(data)

    def _frame(self, op, payload):
        mask = os.urandom(4)
        n = len(payload)
        hdr = bytes([0x80 | op])
        if n < 126: hdr += bytes([0x80 | n])
        elif n < 65536: hdr += bytes([0x80 | 126]) + struct.pack(">H", n)
        else: hdr += bytes([0x80 | 127]) + struct.pack(">Q", n)
        self.s.sendall(hdr + mask + bytes(b ^ mask[i % 4] for i, b in enumerate(payload)))

    def send(self, msg):
        self._frame(1, json.dumps(msg).encode())

    def req(self, typ, data=None, check=True):
        self.n += 1
        rid = str(self.n)
        self.send({"op": 6, "d": {"requestType": typ, "requestId": rid, "requestData": data or {}}})
        while True:
            m = self.recv()
            if m["op"] == 7 and m["d"]["requestId"] == rid:
                st = m["d"]["requestStatus"]
                if check and not st["result"]:
                    raise RuntimeError(f"{typ}: {st}")
                return m["d"].get("responseData", {}) if st["result"] else st
            if m["op"] == 5:
                self.events.append(m["d"])

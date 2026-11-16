// A small WebSocket client (text messages only): Stream Deck runs plugins on Node 20, which has
// no WebSocket of its own, and the plugin ships without dependencies.

"use strict";

const http = require("http");
const crypto = require("crypto");
const { EventEmitter } = require("events");

class Socket extends EventEmitter {
  constructor(url, headers = {}) {
    super();
    this.open = false;
    const key = crypto.randomBytes(16).toString("base64");
    const req = http.request(url.replace(/^ws:/, "http:"), {
      headers: {
        ...headers,
        Connection: "Upgrade",
        Upgrade: "websocket",
        "Sec-WebSocket-Version": "13",
        "Sec-WebSocket-Key": key,
      },
    });
    req.on("upgrade", (res, socket, head) => {
      const want = crypto.createHash("sha1").update(key + "258EAFA5-E914-47DA-95CA-C5AB0DC85B11").digest("base64");
      if (res.headers["sec-websocket-accept"] !== want) {
        socket.destroy();
        return this.closed(new Error("not a WebSocket server"));
      }
      this.socket = socket;
      this.buffer = head && head.length ? Buffer.from(head) : Buffer.alloc(0);
      this.parts = [];
      this.open = true;
      socket.setNoDelay(true);
      socket.on("data", (d) => this.read(d));
      socket.on("close", () => this.closed());
      socket.on("error", (e) => this.closed(e));
      this.emit("open");
      if (this.buffer.length) this.read(Buffer.alloc(0));
    });
    req.on("response", (res) => {
      res.resume();
      this.closed(new Error(res.statusCode === 401 ? "wrong password" : `answered ${res.statusCode}`));
    });
    req.on("error", (e) => this.closed(e));
    req.end();
  }

  closed(error) {
    if (this.done) return;
    this.done = true;
    this.open = false;
    if (this.socket) this.socket.destroy();
    this.emit("close", error);
  }

  read(data) {
    this.buffer = Buffer.concat([this.buffer, data]);
    for (;;) {
      const b = this.buffer;
      if (b.length < 2) return;
      const fin = (b[0] & 0x80) !== 0;
      const op = b[0] & 0x0f;
      const masked = (b[1] & 0x80) !== 0;
      let len = b[1] & 0x7f;
      let at = 2;
      if (len === 126) {
        if (b.length < 4) return;
        len = b.readUInt16BE(2);
        at = 4;
      } else if (len === 127) {
        if (b.length < 10) return;
        len = Number(b.readBigUInt64BE(2));
        at = 10;
      }
      const mask = masked ? b.subarray(at, at + 4) : null;
      if (masked) at += 4;
      if (b.length < at + len) return;
      let payload = Buffer.from(b.subarray(at, at + len));
      if (mask) for (let i = 0; i < payload.length; i++) payload[i] ^= mask[i % 4];
      this.buffer = b.subarray(at + len);
      if (op === 0x8) return this.closed();
      if (op === 0x9) {
        this.frame(0xa, payload);
        continue;
      }
      if (op === 0xa) continue;
      if (op === 0x1 || op === 0x2 || op === 0x0) {
        this.parts.push(payload);
        if (fin) {
          const text = Buffer.concat(this.parts).toString("utf8");
          this.parts = [];
          this.emit("message", text);
        }
      }
    }
  }

  frame(op, payload) {
    if (!this.open) return;
    const len = payload.length;
    const head = len < 126 ? Buffer.alloc(2) : len < 65536 ? Buffer.alloc(4) : Buffer.alloc(10);
    head[0] = 0x80 | op;
    if (len < 126) head[1] = 0x80 | len;
    else if (len < 65536) {
      head[1] = 0x80 | 126;
      head.writeUInt16BE(len, 2);
    } else {
      head[1] = 0x80 | 127;
      head.writeBigUInt64BE(BigInt(len), 2);
    }
    const mask = crypto.randomBytes(4);
    const body = Buffer.from(payload);
    for (let i = 0; i < body.length; i++) body[i] ^= mask[i % 4];
    this.socket.write(Buffer.concat([head, mask, body]));
  }

  send(text) {
    this.frame(0x1, Buffer.from(text, "utf8"));
  }

  close() {
    if (this.open) this.frame(0x8, Buffer.alloc(0));
    this.closed();
  }
}

module.exports = { Socket };

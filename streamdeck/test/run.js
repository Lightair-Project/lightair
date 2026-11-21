// Runs the plugin between a stand-in Stream Deck and Lightair: node streamdeck/test/run.js
// (or with --lightair PORT [--password P] to use a running Lightair's remote API instead of the
// stand-in; it switches scenes and mutes there, so use a test copy, never a show).
// Exits 0 when every key did what it should.

"use strict";

const http = require("http");
const crypto = require("crypto");
const path = require("path");
const { spawn } = require("child_process");

// ---- a WebSocket server, just enough for this ----

function serve(onConnect) {
  const server = http.createServer((_, res) => res.writeHead(404).end());
  server.on("upgrade", (req, socket) => {
    const accept = crypto.createHash("sha1").update(req.headers["sec-websocket-key"] + "258EAFA5-E914-47DA-95CA-C5AB0DC85B11").digest("base64");
    socket.write(`HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: ${accept}\r\n\r\n`);
    let buf = Buffer.alloc(0);
    const peer = {
      req,
      send(obj) {
        const body = Buffer.from(JSON.stringify(obj));
        const head = body.length < 126 ? Buffer.from([0x81, body.length]) : Buffer.from([0x81, 126, body.length >> 8, body.length & 255]);
        socket.write(Buffer.concat([head, body]));
      },
      onmessage: () => {},
    };
    socket.on("data", (d) => {
      buf = Buffer.concat([buf, d]);
      while (buf.length >= 6) {
        let len = buf[1] & 0x7f, at = 2;
        if (len === 126) { len = buf.readUInt16BE(2); at = 4; }
        if (buf.length < at + 4 + len) return;
        const mask = buf.subarray(at, at + 4);
        const body = Buffer.from(buf.subarray(at + 4, at + 4 + len)).map((b, i) => b ^ mask[i % 4]);
        const op = buf[0] & 0x0f;
        buf = buf.subarray(at + 4 + len);
        if (op === 0x8) return socket.end();
        if (op === 0x1) peer.onmessage(JSON.parse(Buffer.from(body).toString()));
      }
    });
    socket.on("error", () => {});
    onConnect(peer);
  });
  return new Promise((ok) => server.listen(0, "127.0.0.1", () => ok(server)));
}

// ---- a stand-in Lightair ----

async function standInLightair(password) {
  const s = {
    program: { id: "s1", name: "Starting Soon" },
    scenes: [{ id: "s1", name: "Starting Soon" }, { id: "s2", name: "Main" }],
    tracks: [{ id: "t1", name: "Mic", source: "src1", muted: false }],
    streaming: false,
    recording: false,
    // two destinations: Twitch is live, YouTube isn't
    destinations: [{ id: "d1", name: "Twitch", state: "idle" }, { id: "d2", name: "YouTube", state: "idle" }],
  };
  const clients = [];
  const emit = (name, data) => clients.forEach((c) => c.send({ jsonrpc: "2.0", method: "event", params: { name, data } }));
  const server = await serve((peer) => {
    if (password && peer.req.headers.authorization !== `Bearer ${password}`) {
      peer.send({ jsonrpc: "2.0", method: "event", params: { name: "nope" } });
    }
    clients.push(peer);
    peer.onmessage = (m) => {
      const ok = (result) => peer.send({ jsonrpc: "2.0", id: m.id, result });
      const fail = (message) => peer.send({ jsonrpc: "2.0", id: m.id, error: { code: -32004, message } });
      if (password && peer.req.headers.authorization !== `Bearer ${password}`) return fail("password needed");
      const p = m.params || {};
      switch (m.method) {
        case "events.subscribe": return ok({});
        case "state.get":
          return ok({ program: s.program, scenes: s.scenes, tracks: s.tracks, outputs: { streaming: { active: s.streaming, destinations: s.destinations }, recording: { active: s.recording } } });
        case "automation.list": return ok({ automations: [{ name: "Hype" }] });
        case "scene.switch": {
          const scene = s.scenes.find((x) => x.id === p.scene || x.name.toLowerCase() === String(p.scene).toLowerCase());
          if (!scene) return fail(`no scene ${p.scene}`);
          s.program = scene;
          ok({});
          return emit("scene.program", scene);
        }
        case "stream.toggle":
          s.streaming = !s.streaming;
          ok({});
          return emit("stream.state", { active: s.streaming });
        case "stream.start":
          if (s.streaming) return fail("already live");
          s.streaming = true;
          s.destinations.forEach((d) => (d.state = "active"));
          ok({});
          s.destinations.forEach((d) => emit("output.changed", { kind: "stream", id: d.id, name: d.name, state: d.state }));
          return emit("stream.state", { active: true });
        case "stream.stop":
          if (!s.streaming) return fail("not live");
          s.streaming = false;
          s.destinations.forEach((d) => (d.state = "idle"));
          ok({});
          s.destinations.forEach((d) => emit("output.changed", { kind: "stream", id: d.id, name: d.name, state: d.state }));
          return emit("stream.state", { active: false });
        case "output.toggle": {
          const d = s.destinations.find((x) => x.id === p.output || x.name.toLowerCase() === String(p.output).toLowerCase());
          if (!d) return fail(`no destination ${p.output}`);
          d.state = d.state === "idle" ? "active" : "idle";
          s.streaming = s.destinations.some((x) => x.state !== "idle");
          ok({ active: d.state !== "idle" });
          return emit("output.changed", { kind: "stream", id: d.id, name: d.name, state: d.state });
        }
        case "record.toggle":
          s.recording = !s.recording;
          ok({});
          return emit("record.state", { active: s.recording });
        case "audio.set": {
          const t = s.tracks.find((x) => x.id === p.track);
          t.muted = p.muted;
          ok({});
          return emit("audio.mute", { track: t.id, muted: t.muted });
        }
        case "clip.save": return ok({});
        case "automation.run": return p.automation === "Hype" ? ok({}) : fail(`no automation ${p.automation}`);
        default: return fail(`no method ${m.method}`);
      }
    };
  });
  return server.address().port;
}

// ---- the test ----

async function main() {
  const at = process.argv.indexOf("--lightair");
  const pw = process.argv.indexOf("--password");
  const password = pw > 0 ? process.argv[pw + 1] : at > 0 ? "" : "s3cret";
  const lightairPort = at > 0 ? Number(process.argv[at + 1]) : await standInLightair(password);
  const real = at > 0;

  const heard = [];
  let deck;
  const deckServer = await serve((peer) => {
    deck = peer;
    peer.onmessage = (m) => {
      heard.push(m);
      if (m.event === "getGlobalSettings") peer.send({ event: "didReceiveGlobalSettings", payload: { settings: { port: String(lightairPort), password } } });
    };
  });
  const plugin = spawn(process.execPath, [path.join(__dirname, "../app.lightair.sdPlugin/bin/plugin.js"), "-port", String(deckServer.address().port), "-pluginUUID", "PLUGIN", "-registerEvent", "registerPlugin", "-info", "{}"], { stdio: ["ignore", "inherit", "inherit"] });

  const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
  const until = async (what, test) => {
    for (let i = 0; i < 50; i++) {
      const hit = heard.find(test);
      if (hit) return hit;
      await sleep(100);
    }
    throw new Error(`never: ${what}`);
  };
  const appear = (context, action, settings) => deck.send({ event: "willAppear", action: `app.lightair.${action}`, context, payload: { settings } });
  const press = (context, action, settings) => deck.send({ event: "keyDown", action: `app.lightair.${action}`, context, payload: { settings } });
  const stateOf = (context, state) => (m) => m.event === "setState" && m.context === context && m.payload.state === state;

  try {
    await until("registered", (m) => m.event === "registerPlugin" && m.uuid === "PLUGIN");
    // a stand-in's scenes; a real Lightair's own
    let scene = "Main", track = "Mic";
    if (real) {
      // ask the plugin what Lightair has, as the key's settings panel would
      await sleep(1500);
      deck.send({ event: "sendToPlugin", action: "app.lightair.scene", context: "ask", payload: { want: "status" } });
      const info = await until("Lightair's lists", (m) => m.event === "sendToPropertyInspector" && m.context === "ask");
      if (!info.payload.connected) throw new Error(`not connected: ${info.payload.error}`);
      scene = info.payload.scenes[info.payload.scenes.length - 1];
      track = info.payload.tracks[0];
      console.log(`Lightair has scenes ${JSON.stringify(info.payload.scenes)}, tracks ${JSON.stringify(info.payload.tracks)}`);
    }
    appear("k-scene", "scene", { scene });
    appear("k-mute", "mute", { track });
    appear("k-clip", "clip", {});
    appear("k-auto", "automation", {});
    await until("scene key titled", (m) => m.event === "setTitle" && m.context === "k-scene" && m.payload.title === scene);
    press("k-scene", "scene", { scene });
    await until(`${scene} lights up once on air`, stateOf("k-scene", 1));
    press("k-mute", "mute", { track });
    await until(`${track} key lights up once muted`, stateOf("k-mute", 1));
    press("k-mute", "mute", { track });
    await until(`${track} key goes dark once unmuted`, (m) => stateOf("k-mute", 0)(m) && heard.filter(stateOf("k-mute", 1)).length > 0 && heard.indexOf(m) > heard.findIndex(stateOf("k-mute", 1)));
    press("k-auto", "automation", {});
    await until("an automation key with none chosen shows an alert", (m) => m.event === "showAlert" && m.context === "k-auto");
    if (!real) {
      appear("k-stream", "stream", {});
      press("k-stream", "stream", {});
      await until("the stream key lights up once live", stateOf("k-stream", 1));
      // one destination: its key follows only it
      appear("k-youtube", "destination", { destination: "YouTube" });
      appear("k-twitch", "destination", { destination: "twitch" });
      await until("a destination key is titled with its name", (m) => m.event === "setTitle" && m.context === "k-youtube" && m.payload.title === "YouTube");
      press("k-youtube", "destination", { destination: "YouTube" });
      await until("YouTube's key lights up once it is live", stateOf("k-youtube", 1));
      press("k-youtube", "destination", { destination: "YouTube" });
      await until("YouTube's key goes dark once stopped", (m) => stateOf("k-youtube", 0)(m) && heard.indexOf(m) > heard.findIndex(stateOf("k-youtube", 1)));
      // start all, then stop all
      appear("k-all", "stream-start", {});
      appear("k-none", "stream-stop", {});
      press("k-all", "stream-start", {});
      await until("start all lights up Twitch's key", stateOf("k-twitch", 1));
      press("k-all", "stream-start", {});
      press("k-none", "stream-stop", {});
      await until("stop all darkens the start key", (m) => stateOf("k-all", 0)(m) && heard.indexOf(m) > heard.findIndex(stateOf("k-all", 1)));
      press("k-clip", "clip", {});
      await until("the clip key shows OK", (m) => m.event === "showOk" && m.context === "k-clip");
    }
    console.log("PASS: every key did what it should");
    plugin.kill();
    process.exit(0);
  } catch (e) {
    console.error(`FAIL: ${e.message}`);
    console.error(JSON.stringify(heard.slice(-8)));
    plugin.kill();
    process.exit(1);
  }
}

main();

// Lightair for Stream Deck: keys that switch scenes, go live, record, save clips, run
// automations and mute tracks, through Lightair's remote API (JSON-RPC at ws://…/rpc,
// docs/remote-api.md). Keys light up from Lightair's own events, so they stay right whatever
// changed it.

"use strict";

const { Socket } = require("./ws");

const args = {};
for (let i = 2; i < process.argv.length - 1; i++) {
  if (process.argv[i].startsWith("-")) args[process.argv[i].slice(1)] = process.argv[i + 1];
}

const ACTION = "app.lightair.";
/** context → { action (short name), settings } */
const keys = new Map();
let global = {};

// What Lightair has on (from state.get, kept current by its events).
const state = { connected: false, error: "", program: null, streaming: false, recording: false, tracks: [], scenes: [], automations: [] };

// ---- Stream Deck ----

const deck = new Socket(`ws://127.0.0.1:${args.port}`);
deck.on("open", () => {
  deck.send(JSON.stringify({ event: args.registerEvent, uuid: args.pluginUUID }));
  deck.send(JSON.stringify({ event: "getGlobalSettings", context: args.pluginUUID }));
});
deck.on("close", () => process.exit(0));
deck.on("message", (text) => {
  let m;
  try {
    m = JSON.parse(text);
  } catch {
    return;
  }
  const short = (m.action || "").startsWith(ACTION) ? m.action.slice(ACTION.length) : "";
  switch (m.event) {
    case "didReceiveGlobalSettings": {
      const next = m.payload.settings || {};
      const changed = next.port !== global.port || next.password !== global.password;
      global = next;
      if (changed || !lightair) connect();
      break;
    }
    case "willAppear":
    case "didReceiveSettings":
      keys.set(m.context, { action: short, settings: m.payload.settings || {} });
      show(m.context);
      break;
    case "willDisappear":
      keys.delete(m.context);
      break;
    case "keyDown":
      keys.set(m.context, { action: short, settings: m.payload.settings || {} });
      press(m.context);
      break;
    case "propertyInspectorDidAppear":
    case "sendToPlugin":
      tellInspector(m.action, m.context);
      break;
  }
});

function toDeck(event, context, payload) {
  deck.send(JSON.stringify(payload === undefined ? { event, context } : { event, context, payload }));
}

function tellInspector(action, context) {
  toDeck("sendToPropertyInspector", context, {
    connected: state.connected,
    error: state.error,
    scenes: state.scenes.map((s) => s.name),
    tracks: state.tracks.map((t) => t.name),
    automations: state.automations,
  });
  void action;
}

// ---- Lightair ----

let lightair = null;
let nextId = 1;
const waiting = new Map();
let retry = null;

function connect() {
  clearTimeout(retry);
  if (lightair) {
    lightair.removeAllListeners("close");
    lightair.close();
  }
  const port = Number(global.port) || 4455;
  const headers = global.password ? { Authorization: `Bearer ${global.password}` } : {};
  const socket = new Socket(`ws://127.0.0.1:${port}/rpc`, headers);
  lightair = socket;
  socket.on("open", async () => {
    state.connected = true;
    state.error = "";
    try {
      await call("events.subscribe", { events: ["scene.program", "scene.list", "record.state", "stream.state", "audio.mute", "workspace.changed"] });
      await refresh();
    } catch (e) {
      state.error = e.message;
      showAll();
    }
  });
  socket.on("message", (text) => {
    let m;
    try {
      m = JSON.parse(text);
    } catch {
      return;
    }
    if (m.id !== undefined && waiting.has(m.id)) {
      const { ok, fail } = waiting.get(m.id);
      waiting.delete(m.id);
      if (m.error) fail(new Error(m.error.message));
      else ok(m.result);
    } else if (m.method === "event") {
      heard(m.params.name, m.params.data || {});
    }
  });
  socket.on("close", (error) => {
    if (socket !== lightair) return;
    state.connected = false;
    state.error = error ? error.message : "Lightair isn't open";
    for (const { fail } of waiting.values()) fail(new Error(state.error));
    waiting.clear();
    showAll();
    retry = setTimeout(connect, 3000);
  });
}

function call(method, params = {}) {
  return new Promise((ok, fail) => {
    if (!lightair || !lightair.open) return fail(new Error("Lightair isn't open"));
    const id = nextId++;
    waiting.set(id, { ok, fail });
    lightair.send(JSON.stringify({ jsonrpc: "2.0", id, method, params }));
    setTimeout(() => {
      if (waiting.delete(id)) fail(new Error("Lightair didn't answer"));
    }, 10000);
  });
}

let refreshing = null;
async function refresh() {
  const s = await call("state.get");
  state.program = s.program;
  state.scenes = (s.scenes || []).map((x) => ({ id: x.id, name: x.name }));
  state.tracks = (s.tracks || []).map((t) => ({ id: t.id, name: t.name, source: t.source, muted: t.muted }));
  state.streaming = !!(s.outputs && s.outputs.streaming && s.outputs.streaming.active);
  state.recording = !!(s.outputs && s.outputs.recording && s.outputs.recording.active);
  try {
    const a = await call("automation.list");
    state.automations = (a.automations || []).map((x) => x.name).filter(Boolean);
  } catch {
    state.automations = [];
  }
  showAll();
}

function heard(name, data) {
  switch (name) {
    case "scene.program":
      state.program = data;
      break;
    case "stream.state":
      state.streaming = !!data.active;
      break;
    case "record.state":
      state.recording = !!data.active;
      break;
    case "audio.mute": {
      const t = state.tracks.find((t) => t.id === data.track);
      if (t) t.muted = !!data.muted;
      break;
    }
    case "scene.list":
    case "workspace.changed":
      // names may have changed: read them again, once things settle
      clearTimeout(refreshing);
      refreshing = setTimeout(() => refresh().catch(() => {}), 300);
      return;
  }
  showAll();
}

// ---- Keys ----

const same = (a, b) => String(a || "").trim().toLowerCase() === String(b || "").trim().toLowerCase();

function track(settings) {
  return state.tracks.find((t) => t.id === settings.track || same(t.name, settings.track) || (t.source && t.source === settings.track));
}

function show(context) {
  const key = keys.get(context);
  if (!key) return;
  const s = key.settings;
  let on = null;
  switch (key.action) {
    case "scene":
      on = !!state.program && (state.program.id === s.scene || same(state.program.name, s.scene));
      if (s.scene) toDeck("setTitle", context, { title: s.scene, target: 0 });
      break;
    case "stream":
      on = state.streaming;
      break;
    case "record":
      on = state.recording;
      break;
    case "mute": {
      const t = track(s);
      on = !!(t && t.muted);
      if (s.track) toDeck("setTitle", context, { title: s.track, target: 0 });
      break;
    }
    case "automation":
      if (s.automation) toDeck("setTitle", context, { title: s.automation, target: 0 });
      break;
  }
  if (on !== null) toDeck("setState", context, { state: on ? 1 : 0 });
}

function showAll() {
  for (const context of keys.keys()) show(context);
}

async function press(context) {
  const { action, settings: s } = keys.get(context);
  try {
    switch (action) {
      case "scene":
        if (!s.scene) throw new Error("choose a scene");
        await call("scene.switch", { scene: s.scene });
        break;
      case "stream":
        await call("stream.toggle");
        break;
      case "record":
        await call("record.toggle");
        break;
      case "clip":
        await call("clip.save");
        toDeck("showOk", context);
        break;
      case "automation":
        if (!s.automation) throw new Error("choose an automation");
        await call("automation.run", { automation: s.automation });
        toDeck("showOk", context);
        break;
      case "mute": {
        const t = track(s);
        if (!t) throw new Error(`no track ${s.track || ""}`);
        await call("audio.set", { track: t.id, muted: !t.muted });
        break;
      }
    }
  } catch (e) {
    console.error(`${action}: ${e.message}`);
    toDeck("showAlert", context);
  }
  show(context);
}

process.on("uncaughtException", (e) => console.error(e));

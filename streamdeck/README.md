# Lightair for Stream Deck

Keys for Lightair, lit with what's on: **Switch scene** (red while on air), **Go live** (red
while live), **Record**, **Save clip**, **Run automation** and **Mute** (amber while muted). It
talks to Lightair's remote API (JSON-RPC at `ws://127.0.0.1:4455/rpc`, see
[docs/remote-api.md](../docs/remote-api.md)), so a key follows changes made anywhere.

- Install: Lightair › Settings › Remote API › Stream Deck › Install plugin (the app carries the
  packed plugin), or open `Lightair.streamDeckPlugin` from `cargo xtask bundle`/`dist`.
- A key's settings: its scene, automation or track (lists come from Lightair), and under
  Connection the port and password if Lightair asks for one.
- No dependencies: Node 20 (Stream Deck's) has no WebSocket, so `bin/ws.js` is a small client.
- Test: `node streamdeck/test/run.js` runs the plugin between a stand-in Stream Deck and a
  stand-in Lightair (part of `cargo xtask precommit`); `--lightair PORT` uses a running
  Lightair instead: a test copy, since it switches scenes and mutes.
- Images: `swift streamdeck/icons.swift` draws them in Lightair's colours.

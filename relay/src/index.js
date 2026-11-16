// Lightair's relay: one private inbox per streamer, named by a long random key Lightair makes.
//
//   POST /kofi/<key>   Ko-fi's webhook (form field `data`, JSON). Kept only if its
//                      verification_token is the one Lightair registered for the inbox.
//   GET  /ws/<key>?kofi=<verification token>
//                      Lightair's WebSocket: registers the token, gets every event (and any
//                      that arrived while it was away, up to a day old).
//
// Nothing else is kept: an event is deleted once a connected Lightair has it.

const KEY = /^[0-9a-f]{32,64}$/;

export default {
  async fetch(request, env) {
    const url = new URL(request.url);
    const [, kind, key] = url.pathname.split('/');
    if (!KEY.test(key || '')) return new Response('not found', { status: 404 });
    const inbox = env.INBOX.get(env.INBOX.idFromName(key));
    if (kind === 'kofi' && request.method === 'POST') return inbox.fetch(new Request('https://inbox/kofi', request));
    if (kind === 'ws' && request.headers.get('Upgrade') === 'websocket') return inbox.fetch(new Request('https://inbox/ws' + url.search, request));
    return new Response('not found', { status: 404 });
  },
};

export class Inbox {
  constructor(state) {
    this.state = state;
    this.sql = state.storage.sql;
    this.sql.exec('CREATE TABLE IF NOT EXISTS events (id INTEGER PRIMARY KEY AUTOINCREMENT, at INTEGER, body TEXT)');
    this.sql.exec('CREATE TABLE IF NOT EXISTS settings (name TEXT PRIMARY KEY, value TEXT)');
  }

  setting(name) {
    const row = this.sql.exec('SELECT value FROM settings WHERE name = ?', name).toArray()[0];
    return row ? row.value : null;
  }

  async fetch(request) {
    const url = new URL(request.url);
    if (url.pathname === '/ws') {
      const token = url.searchParams.get('kofi');
      if (token) this.sql.exec('INSERT OR REPLACE INTO settings (name, value) VALUES (?, ?)', 'kofi', token);
      const [client, server] = Object.values(new WebSocketPair());
      this.state.acceptWebSocket(server);
      // what came while Lightair was away (a day at most)
      this.sql.exec('DELETE FROM events WHERE at < ?', Date.now() - 86400000);
      for (const e of this.sql.exec('SELECT body FROM events ORDER BY id').toArray()) server.send(e.body);
      this.sql.exec('DELETE FROM events');
      return new Response(null, { status: 101, webSocket: client });
    }
    if (url.pathname === '/kofi') {
      const form = await request.formData().catch(() => null);
      let data;
      try { data = JSON.parse((form && form.get('data')) || ''); } catch (e) { return new Response('bad', { status: 400 }); }
      const expected = this.setting('kofi');
      if (!expected || data.verification_token !== expected) return new Response('unknown', { status: 403 });
      const body = JSON.stringify({
        source: 'kofi',
        type: data.type,
        name: data.from_name || 'Someone',
        amount: Number(data.amount || 0),
        currency: data.currency || 'USD',
        message: data.is_public === false ? '' : (data.message || ''),
        id: data.kofi_transaction_id || data.message_id || '',
        tier: data.tier_name || '',
      });
      const sockets = this.state.getWebSockets();
      if (sockets.length) for (const ws of sockets) ws.send(body);
      else this.sql.exec('INSERT INTO events (at, body) VALUES (?, ?)', Date.now(), body);
      return new Response('ok');
    }
    return new Response('not found', { status: 404 });
  }

  webSocketMessage(ws, message) {
    if (message === 'ping') ws.send('pong');
  }
}

# Lightair relay

Donation services that only send webhooks (Ko-fi) need an address on the internet. This
Cloudflare Worker is that address: it keeps a private inbox per streamer and hands each tip to
their Lightair over a WebSocket (tips that arrive while Lightair is closed wait up to a day).

Deploy (a free Cloudflare account is enough; Durable Objects with SQLite are on the free plan):

    cd relay
    npx wrangler deploy

Then in Lightair, Settings › Connections › Ko-fi: set the relay address (what `wrangler deploy`
printed, e.g. `https://lightair-relay.<you>.workers.dev`), copy the webhook address Lightair
shows into Ko-fi (ko-fi.com › Settings › API › Webhooks), and paste Ko-fi's verification
token into Lightair.

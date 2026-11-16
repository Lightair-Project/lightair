# Not done yet

What's left, as of 17 November 2026. The live-integrations plan
([plans/live-integrations.md](plans/live-integrations.md)) says what's built; this is the rest,
with what's known about each. Keep it current: strike items as they land.

## Asked for, being worked on

1. ~~**A real colour picker for every colour field.**~~ Done (shell/colour_field.rs). Many fields offer only a few swatches
   (text, colour objects: `object_props.rs` `PALETTE`/`swatches`; widget fields of type
   `colorpicker` are a text box with a preview: `shell/widget_props.rs`). Every colour field
   should open a proper picker (hue/saturation, alpha where it applies, hex input, the swatches
   as presets, an eyedropper: `eyedropper.rs` exists).
2. ~~**Edit text objects on the canvas.**~~ Done (shell/canvas_text.rs; the field shows the code font, not the object's). Double-click a text object to type in place (today the
   text is edited in the Inspector's field only: `object_props.rs` `TextEdit`).
3. ~~**Sign-in that finishes on its own.**~~ Done. The sign-in window (`lightair_live::accounts::sign_in`,
   Outputs › a destination › Sending) should close itself as soon as the platform's sign-in
   cookie appears (poll `accounts::signed_in` while it's open), and the row update.
4. ~~**A Stream manager panel.**~~ Done in part (stream_panel.rs: live, title, category, viewers, uptime for Twitch, YouTube, Kick; Open dashboard signed in). Left: change title and category from Lightair itself for YouTube and Kick (Twitch done, untested on a real channel: Edit in the Stream panel), stream health per destination, more platforms' details. One place per destination for what's live: title, category or
   game, tags, viewers, uptime, stream health; change title and category per platform (through
   the signed-in pages, no app approval: docs/plans/live-integrations.md, 2.2 and 2.10).

## Chat and events

- **Facebook, Instagram, X, LinkedIn** readers and sending: through signed-in hidden pages.
  Needs a signed-in session to build against (selectors / their own requests are unknown until
  then).
- **Huya** (TARS binary protocol) and **Niconico** (NDGR protobuf over HTTP streaming) readers.
- **SOOP** subscriptions (service codes not yet known), **YouTube** raids, **Bilibili**
  emoticons (sent as `[name]` text now).
- **YouTube Private streams**: readable only signed in as the owner; Unlisted works from the
  stream's link. Could find the owner's own live (Private/Unlisted) through the signed-in
  session.
- **Reading rules updated without a release**: selectors and endpoints in a signed file Lightair
  fetches (plan, 2.10).
- **TwitCasting** reader is untested on a live stream.
- **Sending** is verified only for Twitch's login and join (`LIGHTAIR_SELFTEST=twitch-login`);
  YouTube, Kick and TikTok sending (typing into their pages) are untested with a real session.
  Google may refuse sign-in in an embedded browser.
- ~~**Import a sign-in from Chrome / Brave / Edge / Arc**~~ (done: Outputs › Sending › "Use my
  browser's"; handing cookies to Lightair's browser checked with `LIGHTAIR_SELFTEST=cookie-set`,
  reading a real browser's untested here since it asks for the Keychain). Native Touch ID
  passkeys can't be used (Apple's restricted browser entitlement).
- Chat panel: ~~tabs per platform, reply to a message~~ (done); moderation (delete, timeout, ban) left.

## Widgets

- More first-party widgets from the plan: featured message, prediction, countdown (the Timer
  object covers some), emote combo.
- A widget gallery; Streamlabs custom-widget shim.
- Multi-line text fields in the widget Inspector (the social rotator takes commas for now).

## Automations

- Triggers: ~~stream/recording start and stop, app start, a web address per automation~~ (done);
  ~~hotkeys~~ (done, untested by pressing); MIDI, game controller, voice phrases, file changes left.
- Platform steps: title, category, ads, polls, predictions, rewards, raids, shoutouts with a clip,
  clips, markers, moderation (need the platform's session or official API).
- A block editor beyond the current list (nested if/random editing in place; today they're in
  the JSON).
- Streamer.bot import; Twitch channel-point redemptions (EventSub with a user token).

## Integrations

- ~~Philips Hue~~ (done: Settings › Connections › Philips Hue, automation step "Philips Hue
  light"; tested against a stand-in bridge only). ~~A Lightair Stream Deck plugin~~ (done:
  streamdeck/, Settings › Remote API › Stream Deck; tested with a stand-in Stream Deck against a
  real Lightair, not yet on a real Stream Deck). Left: bring-your-own official API credentials,
  Spotify's own API.
- **The relay** (`relay/`, Ko-fi) isn't deployed: needs a Cloudflare account
  (`npx wrangler deploy`), then Settings › Connections › Ko-fi.
- Pulsoid and Elgato Key Lights are untested without a token / a light; VTube Studio was tested
  against a stand-in server only.

## Known issues

- **`cargo xtask dist` sometimes leaves an invalid signature** ("code has no resources but
  signature indicates they must be present"); running it again works. Cause not found yet:
  find it (keep the full log of a failing run).
- **`media_native::a_rotated_video_shows_upright`** (lightair-preview) fails now and then when
  the whole suite runs in parallel; passes alone. So does
  `stream_sharing::identical_destinations_share_an_encoder` and `audio::track_levels_follow_the_fader`
  (lightair-engine; timing under load).
- Media source render time +1 ms (deferred earlier).
- Notarization needs `xcrun notarytool store-credentials` once (then `LIGHTAIR_NOTARY_PROFILE`).

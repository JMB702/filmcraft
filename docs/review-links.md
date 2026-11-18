# Review links

**File ▸ Share for Review…** (`review.share`) renders a sequence for the web and gives a private
link. Reviewers open it on a phone or a computer, watch, and leave comments at exact frames. Each
comment becomes a marker on the sequence in the running app, with no reload. It is self-hosted:
the video and the comments stay on this machine.

## How it works

```
FilmCraft app ── review.share ──► renders small.mp4 + hd.mp4, writes share.json
     ▲                                     │
     │ review.sync (markers, one           ▼
     │ undo step per comment)     review server (filmcraft --review-server <data>/Review)
     │                                     │  serves the page, the files (HTTP Range) and comments
     └──── Review/notify ◄──── events.jsonl (append, fsync) ◄──── reviewers' browsers
```

- **The server is its own process** (the same executable, in its own process group). It keeps
  taking comments after FilmCraft quits and exits by itself 15 minutes after the last live link
  ends. It only appends to `events.jsonl` and touches `Review/notify`; it never opens a project and
  never exposes or proxies the control channel.
- **The app applies comments.** A file watcher on `Review/notify` wakes the UI, which runs
  `review.sync`: every event not yet in the share's ledger (`applied.json`) is applied to the open
  project as one undo step (`Review Comment from <name>`), so the project becomes dirty, auto-save
  covers it, and the marker can be undone. Undoing it does not bring it back: the event stays in the
  ledger.
- **Comments wait for their project.** A comment for a project that is not open, or whose
  sequence is gone, stays on disk and is applied when that project opens.
- **Marker format.** Name: `<reviewer>: <first six words>…`; comment: the full text; colour:
  Cerulean; kind: Comment; author: the reviewer; web link: `<link>#c=<comment id>`. Edits and
  deletes from the reviewer find the marker by that link.

## Files

Under `<data dir>/Review/` (folders 0700, files 0600):

| Path | What |
|---|---|
| `config.json` | `port` (8787), `bind` (`lan` or `local`), `publicBaseUrl`, `expireDays` (30) |
| `server.json`, `server.log` | the running server's pid and port; its log |
| `notify` | rewritten by the server after each comment, watched by the app |
| `shares/<id>/share.json` | the share: token, project path, sequence id, frame rate, start timecode, range, expiry, revoked |
| `shares/<id>/events.jsonl` | the comment log (append-only, fsync before the reply) |
| `shares/<id>/applied.json` | ids of the events this machine's app has applied |
| `shares/<id>/small.mp4`, `hd.mp4` | the renders (`*.partial.mp4` while encoding) |

## Commands

| Command | Params | Does |
|---|---|---|
| `review.share` | `{"sequence"?, "range": "entire"\|"inOut", "expiresDays"?}` | render, start the server, return `{link, share, …}` and copy the link |
| `review.list` | `{"all": false}` | shares of the open project (`all`: every share) |
| `review.revoke` | `{"share"}` | the link stops working at once |
| `review.sync` | `{}` | apply waiting comments now (the app also does this by itself) |
| `review.config` | `{"port", "bind", "publicBaseUrl", "expireDays"}` | settings; returns `linkBase` |

The project must be saved first (comments are matched to it by path).

## Delivery

Two progressive H.264/AAC MP4 files with the movie header first (faststart), served with HTTP
Range: **small** (fits 960×540, about 1.4 Mb/s, for phones and cellular) and **hd** (fits
1920×1080, about 6 Mb/s). The page picks small on phone-width screens, Save-Data, cellular or slow
links, and the largest file otherwise; the quality chip switches and is remembered. HLS is not
used: it needs a script player on every browser but Safari.

## Exact frames

The page works in frames of the rendered file. The app maps file frame `f` to sequence time
`range start + rate.tick_of(f)` with the sequence's own rational rate (23.976, 29.97 drop-frame and
59.94 are exact). The page shows the sequence's own timecode (Start Time plus the range offset,
`;` for drop-frame), seeks to the middle of a frame so a decoder's rounding cannot land on its
neighbour, and has one-frame steps. See `crates/review/src/timing.rs`.

## Security

- One unguessable 256-bit token per link (`/r/<64 hex>/`), compared in constant time. A link can
  expire (`expiresDays`) and be revoked (`review.revoke`); the server serves only that share's
  files and comments.
- Reviewer text is cleaned (control and bidi-override characters removed, 60 / 2000 characters)
  and escaped in the page; the page script sets it as text only.
- Bodies are capped at 8 KB and headers at 16 KB, requests have deadlines, connections are capped
  (96, 16 per address; 64 live streams, 6 per address), and writes are rate limited per address.
- A device secret, kept in the reviewer's browser, is stored only as a SHA-256 hash: only the
  device that wrote a comment can edit or delete it.
- Strict headers: a CSP with no inline script, `no-referrer`, `nosniff`, `DENY` framing,
  `noindex`.

### Reaching people off the network

The server binds the LAN by default (`bind: "local"` keeps it on this machine). Links use
`http://<LAN address>:<port>` unless `publicBaseUrl` is set. To share beyond the network, put a
tunnel you run yourself in front of the port (for example Tailscale Funnel or Cloudflare Tunnel)
and set `publicBaseUrl` to its address. FilmCraft does not open ports or set up tunnels. Behind a
tunnel on this machine the server takes the client address from `X-Forwarded-For` /
`CF-Connecting-IP` for rate limits, and only when the connection comes from loopback.

## The page

The server renders a page that works with no script: the browser's own player, the comment list,
and a form with a typed timecode. The scripted page (`apps/filmcraft-review-web`, Rust compiled to
wasm with `wasm-bindgen` and `web-sys`) adds the sequence timecode, one-frame steps, the scrub bar
with a tick per comment, tap-to-seek, live comments (server-sent events), the name asked once per
device, editing and deleting your own comments, and the composer above the on-screen keyboard.

```sh
cargo xtask review-web                                  # → target/review-web
FILMCRAFT_REVIEW_WEB_DIR=$PWD/target/review-web cargo build -p filmcraft
```

`FILMCRAFT_REVIEW_WEB_DIR` is an optional build input, like `CRAFT_FONTS_DIR`: the review server
embeds `boot.js`, `review_web.js` and `review_web_bg.wasm` from it. Unset, or missing any of the
three, the server serves the no-script page. `boot.js` is the one call that starts the
wasm-bindgen module; the browser cannot start a wasm module without it.

## Testing

- `cargo test -p filmcraft-review`: frame mapping at broadcast rates, drop-frame timecode, typed
  timecode, tokens, the store, the comment fold, HTTP parsing (with a bit-flip fuzz), ranges, the
  server end to end (only a live token opens a share, a comment is on disk before the reply,
  hostile comments, rate limits, edit and delete by the writing device only, one server per port).
- `cargo test -p filmcraft-engine review_tests`: a comment becomes one undoable marker at its exact
  tick; comments wait for their project; edits and deletes follow; hostile frames and missing
  sequences; `review.share` renders faststart files and copies the link; `review.config`
  validation.
- `cargo test -p filmcraft-review-web`: the page's decisions (rendition choice, stepping, scrub
  mapping, live event folding, stored values, keyboard offset).

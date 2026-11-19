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
| `config.json` | `port` (8787), `bind` (`lan` or `local`), `publicBaseUrl`, `expireDays` (30), `tailscaleSocket` |
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
| `review.funnel` | `{"socket"?, "use"?}` | Tailscale Funnel status and the steps still to do (read-only); `use: true` points links at the Funnel address |

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
- Strict headers: a CSP whose only inline script is the bootstrap line, allowed by its hash;
  `no-referrer`, `nosniff`, `DENY` framing, `noindex`.

### Reaching people off the network

The server binds the LAN by default (`bind: "local"` keeps it on this machine). Links use
`http://<LAN address>:<port>` unless `publicBaseUrl` is set. To share beyond the network, put a
tunnel you run yourself in front of the port (for example Tailscale Funnel or Cloudflare Tunnel)
and set `publicBaseUrl` to its address. FilmCraft does not open ports or set up tunnels. Behind a
tunnel on this machine the server takes the client address from `X-Forwarded-For` /
`CF-Connecting-IP` for rate limits, and only when the connection comes from loopback.

**Tailscale Funnel.** `review.funnel` reads `tailscale status --json` and
`tailscale funnel status --json` (nothing else: it never signs in, turns Funnel on or changes the
tailnet) and returns the state, this machine's `*.ts.net` name, whether the tailnet allows Funnel
and HTTPS certificates, which public port (if any) forwards to the review server, and `steps`:
the commands still to run, in order. A typical first run:

1. Sign in to Tailscale and, in the admin console, turn on MagicDNS and HTTPS Certificates and
   give the machine the `funnel` node attribute.
2. `tailscale funnel --bg <port>` (or `--https=8443` / `--https=10000` when 443 is taken).
3. `review.funnel {"use": true}`: links now start `https://<machine>.<tailnet>.ts.net[:port]`.

`socket` points the command at a tailscaled that runs on its own socket
(`tailscale --socket=<path>`). The reply's `stop` is the command that turns that Funnel port off.

## The page

The server renders a page that works with no script: the browser's own player, the comment list,
and a form with a typed timecode. The scripted page (`apps/filmcraft-review-web`, Rust compiled to
wasm with `wasm-bindgen` and `web-sys`) adds its own transport under the video (play and pause,
full screen, the sequence timecode, one-frame steps, the scrub bar with a tick per comment), so
nothing covers the picture; tap-to-seek, live comments (server-sent events), the name asked once per
device, and editing and deleting your own comments. The scripted page fits the visible screen: the
video stays at the top and only the comment list scrolls; while the on-screen keyboard is up, the
header and transport step aside and the composer sits right above the keyboard. Wide screens and
phones on their side put the comments beside the video.

```sh
cargo xtask review-web                                  # → target/review-web
FILMCRAFT_REVIEW_WEB_DIR=$PWD/target/review-web cargo build -p filmcraft
```

`FILMCRAFT_REVIEW_WEB_DIR` is an optional build input, like `CRAFT_FONTS_DIR`: the review server
embeds `review_web.js` (wasm-bindgen's generated glue) and `review_web_bg.wasm` from it. Unset,
or missing either, the server serves the no-script page.

**The one line of JavaScript.** A browser cannot start a wasm module without a script call, so the
page carries exactly one inline line,
`import init from "/assets/review_web.js?v=…"; init({ module_or_path: "/assets/review_web_bg.wasm?v=…" });`
(`page::boot_script`). Both URLs carry the file's own version, so a browser never pairs a cached
older module with a newer page. It is a reviewed exception to "no handwritten JavaScript in native Craft
apps" (decided for this feature); everything else on the page is Rust or generated glue. The
page's Content-Security-Policy allows that line by its SHA-256 hash, so no other inline script can
run.

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
  mapping, live event folding, stored values, the page's box around the on-screen keyboard).

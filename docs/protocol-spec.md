# `omarex` Protocol, `tr3x` Compatibility & `libghostty-vt` FFI Specification

> **Status**: Complete, live-verified specification for `omarex` (`crates/rex-proto`, `crates/vt-engine`, `crates/tr3x-bridge`).
> **Target Environment**: Omarchy (Arch Linux `x86_64`) · Rex Server `0.1.0-preview.25` (API rev `24`, min rev `0`) · `tr3x` `0.1.8` · `ghostty` `1.4.0-1` (`libghostty-vt.so.0.1.0`).
> **Methodology**: Evidence-based static & runtime reverse engineering following [REA (Reverse Engineer Anything)](https://github.com/morluto/rea): live read-only server introspection (`/llms.txt`, `/api/help`, `rex api`), live `/api/control` + `/api/data` binary capture, Go ELF metadata/string inspection of `/usr/bin/rex`, full source audit of `~/.config/rex/tr3x.lua` and `macnason/tr3x`, and direct C header + live `ctypes` ABI verification against `/usr/lib/libghostty-vt.so`.

---

## 1. System Inventory & Verification Matrix

| Component | Installed Path / Endpoint | Version / ABI | Verification Performed |
| --- | --- | --- | --- |
| Rex CLI & Server | `/usr/bin/rex` | `0.1.0-preview.25` · API rev `24` (min `0`) · links `go.mitchellh.com/libghostty@v0.0.0-20261123035439-5bbe1d0d0990` | Live `/llms.txt` (364 KB), `/api/help` (343 KB, 77 RPCs + 4 block types), live read-only RPCs, ELF strings & Go schema tags |
| Local Control/Data Socket | `unix:///run/user/1000/rex/server.sock` | `rex.control.v1`, `rex.data.terminal.v1`, `rex.data.bytes.v1` | Live control WS handshake + `session.bootstrap` + `data.connect` + `/api/data` binary stream capture (`11,508` B) |
| Auxiliary Listeners (Observed) | `login-server:///run/user/1000/rex/login-server.sock`, built-in Tailscale HTTPS (`https://*.ts.net`) | Peer UID/GID, Tailscale identity, SSH stdio tunnel | Verified via live `server.status`, `hosts.list`, and `/usr/bin/rex` transport code |
| `tr3x` Sync & Lua Bridge | `~/.local/bin/tr3x`, `~/.config/rex/init.lua`, `~/.config/rex/tr3x.lua` | `tr3x 0.1.8` (syncing 23 live windows in `"T3 Code"` session) | Full read of `tr3x.lua` (524 lines) + upstream `github.com/macnason/tr3x` (`rex.rs`, `actions.rs`, `status.rs`, `scheme.rs`, `paste.rs`, `term.rs`, `link.rs`, `docs/how-it-works.md`) |
| `libghostty-vt` | `/usr/lib/libghostty-vt.so` (`-> .0 -> .0.1.0`), `/usr/include/ghostty/vt.h` + 30 headers | `ghostty 1.4.0-1` (`0.1.0-dev`), `SIMD=true`, `KITTY_GRAPHICS=true`, `OPTIMIZE=RELEASE_FAST` | End-to-end live decode of captured `11,508` B Rex stream via `ghostty_snapshot_decoder_new_buf` (`GHOSTTY_SUCCESS`, `11,300` B snapshot + `208` B `rex1;` APCs); live `ghostty_build_info()` + `ghostty_type_json()` ABI dump |

---

## 2. Transport, Endpoint Discovery & Multi-Host Architecture

### 2.1 HTTP & WebSocket Endpoints

Every Rex server exposes the same HTTP surface over whichever transport carries it:

| Method & Path | Purpose | Notes |
| --- | --- | --- |
| `GET /api/control` | Control WebSocket (`rex.control.v1`) | JSON RPC + ordered event stream. Optional `permessage-deflate` (no context takeover). |
| `GET /api/data` | Data WebSocket (`rex.data.terminal.v1` or `rex.data.bytes.v1`) | Raw binary stream. **Never compressed.** |
| `GET /api/help` | Machine-readable JSON Schema catalog | Lists all `http`, `methods` (77), and `block_types` (4). |
| `GET /llms.txt` | Complete Markdown API & protocol reference | Generated directly by the running server revision. |
| `GET /llms.html` | HTML rendering of `/llms.txt` | Human-browsable reference. |

Denied callers (wrong Unix UID, unauthorized Tailscale user, etc.) receive HTTP `403 Forbidden` with JSON body `{"error": "..."}`.

### 2.2 Supported Endpoint Schemes & Local Resolution

`omarex` must resolve endpoints in the same order as `/usr/bin/rex`:

1. Explicit CLI flag / UI host selector (`-S` / `--endpoint`).
2. `REX_SERVER` environment variable (automatically injected inside Rex terminals).
3. Default local Unix domain socket:
   - `$XDG_RUNTIME_DIR/rex/server.sock` (typically `unix:///run/user/1000/rex/server.sock`).
   - Fallback when `XDG_RUNTIME_DIR` is unset: `/tmp/rex-<uid>/server.sock`.
4. **Home server** pointer (for the shared multi-host registry): `REX_HOME` env var or `~/.config/rex/home.json` (defaults to the local server).

| Scheme | Example | Transport & Auth Mechanism |
| --- | --- | --- |
| `unix://` | `unix:///run/user/1000/rex/server.sock` | Local AF_UNIX stream socket; kernel `SO_PEERCRED` (`principal.kind = "peer"`, `uid`/`gid`/`pid`). |
| `https://` | `https://workstation-rex.tail000000.ts.net` | Built-in Tailscale HTTPS (`tsnet`) or machine `tailscale serve`; Tailscale WhoIs identity (`principal.kind = "tailscale"`, `user_id`/`login_name`/`node`/`tags`). |
| `tailnet://` | `tailnet://<node>` | Resolves via local Tailscale daemon (`tailscaled`) to the node's HTTPS/direct carrier. |
| `ssh://` | `ssh://[user@]host[:port]` | First attempts SSH local port forward / Unix socket forward, or executes `ssh <target> rex __internal dial-stdio` to tunnel the HTTP/WebSocket stream over stdio with remote peer credentials. |
| `host:port` | `127.0.0.1:7777` | Direct TCP/HTTP (or TLS if configured); `principal.kind = "anonymous"` unless fronted by a login server. |
| `login-server://` | `login-server:///run/user/1000/rex/login-server.sock` | System-level Unix socket forwarding authenticated principals (`principal.path` includes `"login-server"`). |
| `<host-label>` | `workstation` | Looked up in the home server's `hosts.list` registry. |

#### Alternative Carriers (`welcome.carriers`)
During the control handshake, `welcome.carriers` may advertise faster non-TCP transports (e.g., QUIC with `{"kind": "quic", "port": <u16>, "alpn": "..."}`). For v1 on Omarchy, `omarex` should support `unix://`, `https://` (Tailscale), and `ssh://` (`rex __internal dial-stdio`) over WebSockets, leaving QUIC carrier support cleanly behind a `rex-proto` transport trait.

### 2.3 Multi-Host Registry (`hosts.*`) & Local vs. Remote Block Detection

- The home server persists the shared host list (in its state directory `hosts.json`) and exposes:
  - `hosts.list` -> `{"hosts": [HostEntry...], "hash": "<sha256>"}`
  - `hosts.put` (`{"host": {...}}`) -> `{"host": {...}, "hash": "<sha256>"}`
  - `hosts.remove` (`{"label": "..."}`) -> `{"hash": "<sha256>"}`
- Any mutation emits the server-wide event `hosts_changed` with `{"hash": "<sha256>"}`. Only re-fetch `hosts.list` when `event.hash != local_hosts_hash`.
- **Host identity check**: Compare each block's `host_id` (from `session.view`) against `server.status.host_id` (or `session.bootstrap.server_status.host_id`) to distinguish local blocks from remote/relayed blocks (used for local file path drops vs. `upload_begin` and `tr3x` remote image paste).

---

## 3. Control WebSocket Protocol (`/api/control`, `rex.control.v1`)

### 3.1 Connection, Compression & Handshake

1. Open WebSocket to `GET /api/control` with header `Sec-WebSocket-Protocol: rex.control.v1`.
2. **Compression security rule**: `permessage-deflate` (without context takeover) is optional. If enabled, the client **MUST NOT** compress the `hello` frame or any frame `< 512` bytes, to prevent compression-oracle leakage of `reconnect_token`.
3. Within **5 seconds** of upgrade, send a `hello` text frame (strictly `< 512` bytes):

```json
{
  "type": "hello",
  "api_revision": 24,
  "api_min_revision": 24,
  "reconnect_token": "<22-char base64url token from prior welcome, omit on fresh connect>",
  "client": {
    "kind": "app",
    "platform": "linux",
    "name": "omarex",
    "version": "0.1.0",
    "label": "omarchy-desktop"
  }
}
```

> [!IMPORTANT]
> **`client.kind` MUST be `"app"`**. The Rex server only broadcasts `client_prompt_requested` (`rex.ui` prompts) to connected clients with `kind == "app"`, and `tr3x` discovers the GUI window via `client.list` filtering for `kinds: ["app"]` to target `session.select`.

4. Server responds with a `welcome` text frame:

```json
{
  "type": "welcome",
  "protocol": "rex.control.v1",
  "limits": {
    "max_message_bytes": 1048576
  },
  "carriers": [],
  "client": {
    "id": "client:01a216bc-xxxx-xxxx-xxxx-xxxxxxxxxxxx",
    "reconnect_token": "AbCdEfGhIjKlMnOpQrStUv",
    "resumed": false
  },
  "features": [],
  "api_revision": 24,
  "api_server_revision": 24,
  "api_min_revision": 0
}
```

- **Revision negotiation**: The negotiated revision is `min(client.api_revision, server.api_server_revision)`. If the ranges `[api_min_revision, api_revision]` do not overlap, the server rejects the handshake (`revision_too_low`).
- **Session resume (`reconnect_token`)**: If the TCP/socket drops without a WebSocket close frame (`1000`), the server marks the client `connection_state: "lost"` and preserves its `client_id`, session attachments, and block ownership for **60 seconds**. Reconnecting with the latest `reconnect_token` yields `client.resumed: true`, preserves `client.id`, emits `block_owner_changed` (`reason: "restore"`), and rotates `reconnect_token` (always store the newest token).
- **Keepalive**: Server sends WebSocket `Ping` every **120s** (60s pong timeout; 3 missed pings -> close `1001 keepalive timeout`).
- **Event backlog cap**: If a slow client lets outbound server events queue beyond **8 MiB**, the server closes with `1008 event backlog exceeded`.

### 3.2 RPC Framing, Error Codes & Execution Ordering

#### Request Frame
```json
{
  "type": "request",
  "id": "req-42",
  "method": "session.view",
  "payload": {
    "session_id": "session:01a216bc-52f2-72c7-aa9a-e94e1d60b77a"
  }
}
```
- `id`: Any client-chosen string unique among currently in-flight requests on this connection.
- **Block methods** use `"method": "<creator_name>.<method>"` (e.g., `"com.superlogical.terminal.write"`) with payload wrapped as:
  `{"session_id": "...", "block_id": "...", "args": { ... }}`.

#### Success Response Frame
```json
{
  "type": "response",
  "id": "req-42",
  "seq": 108,
  "payload": { ... }
}
```

#### Error Response Frame
```json
{
  "type": "response",
  "id": "req-42",
  "seq": 108,
  "error": {
    "code": "not_found",
    "message": "block not found",
    "status": 404
  }
}
```

| Error `code` | HTTP `status` | Meaning & Handling |
| --- | --- | --- |
| `bad_request` | `400` | Invalid schema, unknown/invalid field, or constraint violation. |
| `not_supported` | `400` | Method or prompt kind unsupported (also use in `client.reply` for unknown `prompt.kind`). |
| `revision_too_low` | `400` | Negotiated `api_revision` lacks this method/feature. |
| `refused` | `403` | Permission denied or action refused by peer client. |
| `not_found` | `404` | Block, window, layer, call_id, or resource does not exist. |
| `method_not_found` | `404` | Server or block creator does not implement method. |
| `session_not_found` | `404` | Target `session_id` does not exist (destroyed or invalid). |
| `conflict` | `409` | State conflict (e.g., hand-edited corrupt `hosts.json`, ownership/layout race). |
| `client_unavailable` | `409` | Target `client_id` for `client.perform` disconnected or cannot handle it. |
| `too_large` | `413` | Payload exceeds `limits.max_message_bytes` (1 MiB default). |
| `timeout` | `504` | Downstream action/prompt/perform timed out. |
| `internal` | `500` | Unexpected server failure. |

#### Ordered vs. Unordered Methods (Deadlock-Critical)
The server maintains a **strict FIFO ordered queue** per control connection for layout, focus, ownership, and key/action mutation methods, while executing all other RPCs concurrently:

- **Ordered methods (22)**:
  `block.close`, `block.event`, `client.action`, `client.key`, `client.undo_close`, `focus_history.reverse`, `session.close_layer`, `session.close_window`, `session.detach_block`, `session.disown_block`, `session.float_block`, `session.focus_block`, `session.focus_direction`, `session.focus_next_pane`, `session.focus_next_window`, `session.focus_previous_pane`, `session.focus_previous_window`, `session.focus_window`, `session.move_block`, `session.move_window`, `session.new_layer`, `session.new_split`, `session.new_window`, `session.own_block`, `session.set_layer_state`, `session.swap_blocks`, `session.transfer_window`, `session.zoom_block`.
- **Unordered methods (execute concurrently even while an ordered call is blocked)**:
  `client.reply`, `session.view`, `session.list`, `data.connect`, `client.keymap`, `com.superlogical.terminal.*`, `hosts.*`, `server.*`, etc.

> [!IMPORTANT]
> Because `client.action` is **ordered** and may block waiting for a `client_prompt_requested` (`rex.ui`) or `client_call_requested` response, `client.reply` is explicitly **unordered**. `omarex` must **never** serialize outbound `client.reply` calls behind an awaiting `client.action` future—doing so will deadlock any interactive `tr3x` Lua action!

### 3.3 Event Framing & Snapshot Synchronization

Every asynchronous server push uses a monotonically increasing per-connection `seq` (`1, 2, 3, ...`):

```json
{
  "type": "event",
  "seq": 109,
  "payload": {
    "type": "session_event",
    "payload": {
      "session_id": "session:01a216bc-52f2-72c7-aa9a-e94e1d60b77a",
      "event": {
        "type": "block_event",
        "payload": {
          "block_id": "block:01a216bc-5341-7c6d-a2f6-4844536132ce",
          "creator_name": "com.superlogical.terminal",
          "name": "program_status_changed",
          "payload": { ... }
        }
      }
    }
  }
}
```

#### Exact Snapshot Reconciliation Algorithm (`rex-proto`)
1. While an initial snapshot call (`session.bootstrap`, `session.attach`, or `session.view`) is in flight, **buffer** incoming `event` frames in order.
2. When the `response` arrives with `response.seq = S`:
   - Apply the snapshot state.
   - **Discard** buffered events with `event.seq <= S`.
   - **Apply** buffered events with `event.seq > S` idempotently.
3. For `session_view_changed { "revision": R }`:
   - Events can arrive out of revision order; **only** request `session.view` if `R > local_session.revision`.
   - Coalesce bursty `session_view_changed` notifications so at most one `session.view` RPC is in flight per session (if a higher `R'` arrives while in flight, re-fetch once on completion).
4. For `event_dropped`:
   - Server substitutes this at the dropped event's `seq` when an event exceeded `max_message_bytes`. Re-fetch the affected `session.view` / `session.list_blocks` / `hosts.list` according to `event_type` and `session_id`.

### 3.4 1-RTT Startup Flow (`session.bootstrap`) & `session.view` Layout Schema

Instead of issuing an N+1 RPC storm on startup, `omarex` opens its entire workspace in **1 RTT** using `session.bootstrap` (in parallel with `client.keymap` and `hosts.list`):

```json
{
  "type": "request",
  "id": "boot-1",
  "method": "session.bootstrap",
  "payload": {
    "event_scope": "all",
    "preferred_session_id": "<last_viewed_session_id_optional>",
    "data_tickets": [
      {
        "creator_name": "com.superlogical.terminal",
        "data_ref_id": "terminal",
        "subprotocol": "rex.data.terminal.v1"
      }
    ]
  }
}
```

- `event_scope: "all"` is required to receive `block_event` across all sessions (for sidebar status dots / notifications / `tr3x` badges) as well as `client_joined`/`changed`/`left` and `client_performed`.
- Response fields: `client_id`, `server_status` (includes `host_id`, `pid`, `instance_id`, `api_revision`, `tailscale`, `listeners`), `attach_all` (sessions list), `selected_session` (full `session.view`), `attached_sessions`, and `data_tickets` (ready-to-dial 16-byte tickets for every terminal block in the attached session(s)!).
- Note: Opening a `/api/data` connection for a block in a session requires being attached to that session via `session.attach` or `session.bootstrap` (`session.attach_all` alone is insufficient for `data.connect`).

#### `session.view` Schema (Live-Verified from 23-Window T3 Code Session)
```json
{
  "revision": 412,
  "session_id": "session:01a216bc-52f2-72c7-aa9a-e94e1d60b77a",
  "label": "T3 Code",
  "active_window_id": "window:01a216bc-5329-74b7-8d08-26ca9f808eb6",
  "windows": [
    {
      "window_id": "window:01a216bc-5329-74b7-8d08-26ca9f808eb6",
      "label": "Run and Optimize Inkling-2-Mini on Omarchy",
      "focused_block_id": "block:01a216bc-531c-781a-84b4-fc98fdf64fb4",
      "tiled_zoomed_block_id": null,
      "layers": [
        {
          "layer_id": "layer:01a216bc-5329-74bd-a876-500219635eee",
          "kind": "tiled",
          "state": "normal",
          "bounds": [0, 0, 1, 1],
          "layout": {
            "rect": [0, 0, 1, 1],
            "block_id": "block:01a216bc-531c-781a-84b4-fc98fdf64fb4"
          },
          "blocks": [
            {
              "block_id": "block:01a216bc-531c-781a-84b4-fc98fdf64fb4",
              "label": "t3:1baa1939/f68600ef-cf19-4e5a-94d6-a3f88abfbd3e",
              "creator_name": "com.superlogical.terminal",
              "flavor": "com.superlogical.terminal.shell",
              "host_id": "d686093a-e969-4803-86a8-6a6dd1b4e79d"
            }
          ]
        }
      ]
    }
  ]
}
```
- **Layers**: Ordered bottom-to-top (z-order). `kind` is `"tiled"` (base layer, `bounds: [0,0,1,1]`) or `"floating"` (`bounds: [x, y, w, h]` normalized `0.0..=1.0`, `state: "normal" | "minimized" | "maximized"`).
- **Layout tree**: Recursive split node (`direction: "horizontal" | "vertical"`, `ratio: f64`, `split_id`, `children: [left, right]`, `rect: [x, y, w, h]`) or leaf node (`block_id`, `rect: [x, y, w, h]`).
- **Zoom**: If `window.tiled_zoomed_block_id` is present and non-null, render only that block filling the tiled layer (`[0, 0, 1, 1]`). Floating layers still render above it.

### 3.5 Complete Event Catalog

#### Server-Wide Events (14 types)
| Event `type` | Payload Fields | Trigger & Client Action |
| --- | --- | --- |
| `client_joined` | `client` (`client_id`, `info`, `principal`, `connected_at`, `connection_state`, `session_ids`) | New client connected (`event_scope: "all"`). |
| `client_changed` | `client` | Client changed label/actions or reconnected with new info. |
| `client_left` | `client_id` | Client closed or 60s reconnect grace expired. |
| `client_prompt_requested` | `call_id`, `action?`, `prompt` (`kind`, `title`, `placeholder?`, `value?`, `confirm?`, `multiple?`, `allow_other?`, `items?`), `timeout_ms` | Lua `rex.ui` prompt. Render native modal/palette; reply via `client.reply`. |
| `client_prompt_ended` | `call_id`, `reason` (`answered` \| `timeout` \| `cancelled`) | Immediately close modal matching `call_id` without replying. |
| `client_perform_requested` | `call_id`, `action`, `args?`, `origin?`, `session_id?`, `block_id?`, `requester` | Peer called `client.perform` targeting us (e.g. `session.select`, `pane.paste`). Execute and answer quickly via `client.reply`. |
| `client_performed` | `client_id`, `action`, `requester`, `outcome` (`performed`\|`refused`\|`failed`\|`timeout`), `error?` | Audit notification when any `client.perform` finishes (`event_scope: "all"`). |
| `client_call_requested` | `call_id`, `server`, `method`, `args?` | Multi-server Lua action proxy: run RPC on named remote server and return via `client.reply` within 10s. |
| `client_script_events_dropped` | `session_id`, `client_id`, `event_name` | Client Lua script fell behind on session events. |
| `event_dropped` | `event_type`, `session_id?`, `block_id?`, `creator_name?`, `block_event_name?`, `size` | Oversized event dropped; re-fetch affected state. |
| `hosts_changed` | `hash` | Call `hosts.list` if `hash != local_hash`. |
| `keymap_changed` | `generation`, `hash` | Call `client.keymap` if `hash != local_hash`. |
| `session_created` | `session` (`session_id`, `label`) | Add session to sidebar. |
| `session_destroyed` | `session_id` | Remove session; tear down its data streams. |
| `session_moved` | `session_id`, `before_session_id?` | Reorder session list (omitted `before_session_id` = moved to end). |
| `tailscale_status_changed` | `state`, `dns_name?`, `auth_url?`, `account?`, `tailnet_name?`, `allow_tailnet_members`, `tags?`, `https?`, `control_url?`, `error?` | Update Tailscale UI indicator. |

#### Session Events (14 types, wrapped in `session_event { session_id, event }`)
| Event `type` | Payload Fields | Handling |
| --- | --- | --- |
| `active_window_changed` | `window_id?` | Switch active tab in session. |
| `block_created` | `block_id` | Block joined session (may not be placed in layout yet). |
| `block_closed` | `block_id` | Close data stream and release `GhosttyTerminal` for `block_id`. |
| `block_owner_changed` | `block_id`, `owner?`, `previous?`, `reason` (`create`\|`take`\|`request`\|`disown`\|`lost`\|`disconnect`\|`restore`) | Update ownership badge; if we became owner, flush desired resize/theme if needed. |
| `block_event` | `block_id`, `creator_name`, `name`, `payload?` | Dispatch to block-type handler (see below). |
| `client_connected` | `client_id`, `principal`, `connected_at` | Update session client presence. |
| `client_connection_changed` | `client_id`, `principal`, `connection_state` (`connected`\|`lost`), `connection_lost_at?` | Update presence indicator. |
| `client_disconnected` | `client_id`, `principal` | Remove from session presence. |
| `creator_changed` | *(empty)* | Refresh `block_creator.list` if cached. |
| `session_label_changed` | `label` | Update session title in sidebar. |
| `session_view_changed` | `revision` | If `revision > local_revision`, call `session.view`. |
| `window_created` | `window_id`, `label` | Optimistically register window (authoritative layout arrives via `session_view_changed`). |
| `window_closed` | `window_id` | Remove window tab. |
| `window_label_changed` | `window_id`, `label` | Update tab/sidebar title immediately. |

#### Block Events (`block_event` for `com.superlogical.terminal`, 15 types)
| Event `name` | Key Payload Fields | Client Usage |
| --- | --- | --- |
| `bell` | *(empty)* | Visual pane flash / optional system bell / urgent tab dot. |
| `child_exited` | `exit_code` | Show exited badge/banner on pane (`[exited <code>]`). |
| `clipboard_written` | `id`, `kind`, `size`, `truncated?` | OSC 52 write; read via `com.superlogical.terminal.clipboard_read` and set Wayland clipboard (when policy permits). |
| `desktop_notification` | `title`, `body` | OSC 9 / 777; show desktop notification (`notify-send` / D-Bus) if unfocused. |
| `ports_changed` | `ports`, `status` | Update listening-ports pill on pane/window. |
| `process_changed` | `foreground`, `child`, `last_exit` | Update pane subtitle / running command indicator. |
| `program_resume_changed` | `records` | OSC 7577 resume metadata updated. |
| `program_status_changed` | `record` (`id`, `state`, `kind?`, `progress?`, `app?`, `title?`, `message?`, `updated_at`) | **Core `tr3x` & agent indicator**: drives sidebar/tab attention dots, progress rings, and `app` glyph. |
| `program_status_removed` | `id` | Clear status record for `id`. |
| `progress_report` | `state` (`remove`\|`set`\|`error`\|`indeterminate`\|`pause`), `progress?` (`0..=100`) | OSC 9;4 progress bar on tab/pane. |
| `pwd_changed` | `pwd`, `owner?` | OSC 7 working directory update; used for new split CWD and breadcrumb. |
| `selection_copied` | `text` | Server-side copy feedback. |
| `size_changed` | `columns`, `rows`, `cell_width_px`, `cell_height_px`, `owner?` | Mirrors in-band `resize` APC for non-streaming observers. |
| `title_changed` | `title`, `owner?` | OSC 0/2 terminal title update. |
| `url_clicked` | `url` | Open link via `xdg-open` if appropriate. |

*(Other block creators emit `com.superlogical.onboarding.step_changed` and `com.superlogical.tunnel.connection_requested`.)*

### 3.6 Key RPC Method Reference (Rev 24)

#### Core Session, Client & Data Methods
| Method | Required Input | Key Optional Input | Output Fields |
| --- | --- | --- | --- |
| `session.bootstrap` | `event_scope` (`"session"`\|`"all"`) | `preferred_session_id`, `attach_session_ids`, `data_tickets` | `client_id`, `server_status`, `attach_all`, `selected_session`, `attached_sessions`, `data_tickets` |
| `session.attach` | `session_id` | — | `session_view` (or view fields + attached state) |
| `session.detach` | `session_id` | — | `{}` |
| `session.view` | `session_id` | — | `revision`, `session_id`, `label`, `active_window_id`, `windows` |
| `session.list` | — | — | `sessions: [{session_id, label}]` |
| `session.create` | — | `label`, `initial_windows` | `session_id`, `initial_windows` |
| `session.destroy` | `session_id` | — | `{}` (**Never** call unilaterally on empty view!) |
| `session.new_window` | `session_id` | `window_label`, `layout`, `focus` | `window_id`, `block_ids`, `revision` |
| `session.new_split` | `session_id`, `direction`, `side`, `layout` | `anchor_id`, `window_id`, `ratio`, `focus` | `anchor_id`, `block_ids`, `split_ids`, `revision` |
| `session.new_layer` | `session_id`, `bounds`, `layout` | `window_id`, `focus` | `layer_id`, `block_ids`, `revision` |
| `session.float_block` | `session_id`, `block_id`, `bounds` | — | `layer_id`, `revision` |
| `session.set_layer_state` | `session_id`, `layer_id`, `state` | `focus` | `revision` |
| `session.resize_pane` | `session_id`, `direction`, `before_block_id`, `after_block_id`, `ratio` | — | `revision` |
| `session.resize_layer` | `session_id`, `layer_id`, `bounds` | — | `revision` |
| `session.zoom_block` | `session_id` | `block_id`, `window_id`, `zoom` | `block_id`, `zoomed`, `revision` |
| `session.focus_window` / `focus_block` | `session_id`, `window_id` / `block_id` | — | `revision` |
| `session.close_window` / `close_layer` | `session_id`, `window_id` / `layer_id` | `undoable` (keeps blocks alive ~60s for `client.undo_close`) | `revision` |
| `session.own_block` | `session_id` | `block_ids` (empty = all), `mode` (`"take"`\|`"request"`) | `blocks` |
| `session.disown_block` | `session_id` | `block_ids` | `blocks` |
| `session.describe_block` | `session_id`, `block_id` | — | full block metadata (`label`, `owner`, `creator_name`, `flavor`, `host_id`, ...) |
| `session.transfer_window` | `source_session_id`, `target_session_id`, `window_id` | `focus` | `revision` |
| `block.close` | `session_id`, `block_id` | `undoable` | `{}` |
| `block.event` | `session_id`, `block_id`, `event` (`{type, payload}`) | — | `{}` |
| `data.connect` | `session_id`, `block_id`, `data_ref_id` | `subprotocol` | `ticket` (22-char base64url), `expires_in_ms` (`60000`) |
| `client.keymap` | — | `actions`, `server_actions`, `defaults`, `overrides` | `bindings`, `diagnostics`, `actions`, `modes`, `config` |
| `client.key` | `session_id`, `key` | `block_id`, `prefix` | `handled` (bool) |
| `client.action` | `action` | `args`, `origin`, `session_id`, `block_id`, `servers`, `server` | `result`, `client_actions`, `ordered_after` |
| `client.reply` | `call_id` | `result`, `error` (`{code, message}`) | `{}` |
| `client.perform` | `client_id`, `action` | `args`, `origin`, `session_id`, `block_id`, `timeout_ms` | `result`, `performed_by` |
| `client.list` | — | `kinds`, `session_id` | `clients` |
| `client.undo_close` | `session_id` | — | restored window/block ids + `revision` |
| `hosts.list` / `hosts.put` / `hosts.remove` | *(see §2.3)* | — | `hosts`, `hash` |
| `server.status` | — | — | `pid`, `instance_id`, `host_id`, `started_at`, `exit_empty`, `session_count`, `tailscale`, `api_revision`, `api_min_revision`, `config`, `listeners` |
| `server.config_reload` | — | — | reloaded config summary (emits `keymap_changed`) |

#### All 20 `com.superlogical.terminal.*` Block Methods
Invoked with `payload: { "session_id": "...", "block_id": "...", "args": { ... } }`:

| Method | Purpose | Output Fields |
| --- | --- | --- |
| `resize` | Set terminal grid (`columns`, `rows`, `cell_width_px`, `cell_height_px`) | `{}` |
| `set_theme` | Set default colors (`foreground`, `background`, `cursor`, `palette`, `scheme`) | `{}` |
| `write` | Write raw UTF-8/bytes (`data`) directly to PTY | `bytes_written` |
| `clear` | Clear scrollback & viewport | `{}` |
| `reset` | Hard terminal reset (RIS) | `{}` |
| `size` | Query current + desired dimensions & owner | `columns`, `rows`, `cell_width_px`, `cell_height_px`, `owner`, `desired` |
| `modes` | Query active screen (`primary`\|`alternate`) and DEC/ANSI modes | `screen`, `modes` |
| `input_mode` | Query TTY termios flags | `echo`, `canonical`, `signals` |
| `title` | Query OSC 0/2 title | `title`, `owner` |
| `pwd` | Query OSC 7 working directory | `pwd`, `owner` |
| `process` | Query foreground & child processes | `last_exit`, `child`, `foreground`, `ancestors`, `home` |
| `ports` | Query listening TCP/UDP ports of process tree | `status`, `ports`, `processes`, `truncated`, `id` |
| `program_status` | Query active OSC 7501 records | `records` |
| `program_resume_records` | Query active OSC 7577 records | `records` |
| `format` | Render viewport/scrollback as plain/ANSI/HTML text | `content`, `id`, `total`, `eof` |
| `clipboard_read` | Read OSC 52 clipboard blob by `id`/`offset` | `id`, `data`, `total`, `eof` |
| `blob_read` | Read chunked binary/text blob by `id` | `id`, `data`, `total`, `eof` |
| `list_dir` | List directory entries on terminal's host | `path`, `home`, `status`, `entries`, `truncated` |
| `upload_begin` | Reserve file upload batch -> `rex.data.bytes.v1` ticket | `ticket`, `expires_in_ms` |
| `download_begin` | Open regular file for download -> `rex.data.bytes.v1` ticket | `ticket`, `expires_in_ms`, `not_modified`, `path`, `name`, `size`, `modified_at`, `version` |

---

## 4. Data WebSocket Protocol (`/api/data`)

### 4.1 Subprotocols & Handshake Binary Frames

Every `/api/data` connection carries **one stream** for one `(session_id, block_id, data_ref_id)` and is **never compressed**. All integers in handshake frames are **unsigned little-endian**.

| Subprotocol | Direction | Use Case |
| --- | --- | --- |
| `rex.data.terminal.v1` | Server -> Client only | Terminal `GHOSTSNP` v1 snapshot + live VT bytes + authenticated `rex1;` APCs. Server ignores all client frames after `WELCOME`. |
| `rex.data.bytes.v1` | Bidirectional | File upload (`upload_begin`), file download (`download_begin`), `com.superlogical.fs.*`, and `com.superlogical.tunnel` TCP relays (`data_ref_id: "conn:<name>"`). |

#### Handshake Message Layouts (Single Binary WebSocket Message Each)

```
HELLO (Client -> Server, 17 bytes, send within 5s of WS open):
Offset  Size  Type      Field
0       1     u8        0x01
1       16    [u8; 16]  ticket_bytes  (raw 16 B from unpadded base64url ticket)

RESUME (Client -> Server, 25 bytes, send within 5s of WS open):
Offset  Size  Type      Field
0       1     u8        0x02
1       16    [u8; 16]  reconnect_token (latest 16 B token from prior WELCOME)
17      8     u64 LE    bytes_received  (exact total stream bytes consumed across all conns)

WELCOME (Server -> Client, exactly 32 bytes):
Offset  Size  Type      Field
0       1     u8        0x81
1       16    [u8; 16]  reconnect_token (save immediately; rotates on every RESUME)
17      3     [u8; 3]   reserved        (0x00, 0x00, 0x00)
20      4     u32 LE    resume_ttl_ms   (typically 60000 = 60s)
24      8     u64 LE    channel_nonce   (always non-zero; stays constant across RESUMEs)

REJECT (Server -> Client, 2 bytes, followed by WS Close "data handshake rejected"):
Offset  Size  Type      Field
0       1     u8        0x82
1       1     u8        reason
```

#### `REJECT` Reason Codes
| Byte | Meaning | Recovery Action |
| --- | --- | --- |
| `0x01` | Ticket/token unknown, expired (>60s), reused, wrong user, or session gone | Call `data.connect` for a fresh ticket + `HELLO`. |
| `0x02` | Duplicate active stream for same `(client, block, data_ref_id)` | Use `RESUME` (which atomically replaces a dropped conn) or close existing conn first. |
| `0x03` | Stream unavailable (block closed, or `RESUME.bytes_received` too low / evicted) | Check if block closed; otherwise fetch new ticket + `HELLO`. |
| `0x04` | Server limit exceeded (reserved) | Backoff and retry. |
| `0x05` | Malformed handshake, >5s timeout, or `RESUME.bytes_received` > server sent | Bug/desync — fetch new ticket + `HELLO`. |
| `0x06` | Internal server error | Backoff and re-ticket. |
| `0x07` | Subprotocol mismatch against ticket/token | Fix `Sec-WebSocket-Protocol` header. |

*Note*: A `HELLO` consumes its ticket even on `REJECT`. After a refused `RESUME`, the server ends the waiting stream—always recover with a new `data.connect` ticket + `HELLO`.

### 4.2 Terminal Stream Layout (`rex.data.terminal.v1`) — Live-Verified

After `WELCOME`, the server sends binary WebSocket frames of `1..=65,536` bytes (`64 KiB`). Frame boundaries are arbitrary; treat them as a single contiguous byte stream:

```
┌─────────────────────────────────────────────────────────────────────────┐
│ Phase 1: Ghostty GHOSTSNP v1 Binary Snapshot                            │
│   [0..8]   Magic: b"GHOSTSNP"                                           │
│   [8..10]  Version: u16 LE = 1                                          │
│   [10..]   Sequence of CRC32C records:                                  │
│              tag: u16 LE | payload_len: u32 LE | crc32c: u32 LE | data  │
│            Observed live record sequence:                               │
│              1(TERMINAL) -> 2(SCREEN) -> 3(PAGE)* -> 7(CONTINUATION)*   │
│              -> 5(READY) -> [4(HISTORY) / Kitty image records]*         │
│              -> 6(FINISH)                                               │
├─────────────────────────────────────────────────────────────────────────┤
│ Phase 2: Live VT Byte Stream + Authenticated Rex Control APCs           │
│   Raw PTY VT output interleaved at parser ground states with:           │
│     ESC _ rex1;<channel_nonce_decimal>;<json> ESC \                     │
│     (\x1b_rex1;...;\x1b\\)                                              │
│   Immediately after FINISH (offset 11300 in live probe):                │
│     1. {"type":"resize","cols":98,"rows":32,"cell_width_px":8,          │
│         "cell_height_px":16,"owner":null}                               │
│     2. {"type":"theme","foreground":"#ffffff","background":"#121212"}   │
│   Followed by live PTY bytes, future resize/theme APCs, or child_exited │
└─────────────────────────────────────────────────────────────────────────┘
```

#### Live Capture Proof (Recorded on `block:01a216bc-5341-7c6d-a2f6-4844536132ce`)
- Captured `11,508` bytes from `/api/data` after a `32`-byte `WELCOME` (`resume_ttl_ms = 60000`, non-zero `channel_nonce`).
- Passed directly into `ghostty_snapshot_decoder_new_buf()` + `ghostty_snapshot_decoder_decode()` on `/usr/lib/libghostty-vt.so`:
  - Result: `GHOSTTY_SUCCESS (0)`.
  - `GHOSTTY_SNAPSHOT_DECODER_DATA_SOURCE_OFFSET` returned **`11,300` bytes** exact snapshot boundary.
  - Restored live `98x32` terminal state cleanly.
  - Remaining **`208` bytes** (`[11300..11508]`) were two exact `ESC _ rex1;<nonce>;<json> ESC \` frames (`resize` + `theme`), with `<nonce>` matching `WELCOME.channel_nonce` verbatim.

#### Authenticated `rex1;` Control APCs (Security-Critical)
Format: `\x1b_rex1;<decimal_u64_nonce>;<utf8_json>\x1b\\`

1. **Nonce Verification**: Parse `<decimal_u64_nonce>` as `u64` and compare with `WELCOME.channel_nonce` (constant-time or exact equality). **If the nonce does not match, silently ignore the APC** — a malicious/untrusted terminal program emitted a spoofed escape sequence!
2. **Delivery Order**: Never regex-scan raw stream buffers out-of-band (which breaks on split frames or inside DCS/OSC/Kitty payloads). Instead, feed all post-snapshot bytes into `ghostty_terminal_vt_write()` and register `GHOSTTY_TERMINAL_OPT_UNKNOWN_SEQUENCE` (`GHOSTTY_TERMINAL_UNKNOWN_SEQUENCE_APC`) with `GHOSTTY_TERMINAL_OPT_UNKNOWN_MAX_BYTES >= 65536`. `libghostty-vt` invokes your callback at the exact VT stream position!
3. **Message Types**:
   - `resize`: `{"type":"resize","cols":u16,"rows":u16,"cell_width_px":u32,"cell_height_px":u32,"owner":"client:..."|null}`
     -> Call `ghostty_terminal_resize(term, cols, rows, cell_width_px, cell_height_px)`. Always sent right after snapshot and on every dimension change (not on owner-only changes).
   - `theme`: `{"type":"theme","foreground":"#RRGGBB","background":"#RRGGBB","cursor":"#RRGGBB","palette":["0=RRGGBB",...],"scheme":"light"|"dark"}`
     -> Each `theme` message **replaces** the prior theme completely (omitted fields revert to terminal built-in defaults; colors may be `RRGGBB` or `#RRGGBB`). Apply via `GHOSTTY_TERMINAL_OPT_COLOR_FOREGROUND/BACKGROUND/CURSOR/PALETTE` and `COLOR_SCHEME`.
   - `child_exited`: `{"type":"child_exited","exit_code":i32}`
     -> Mark pane as exited; stream remains open (no further PTY output).
   - Unknown `type`: Ignore gracefully for forward compatibility.

### 4.3 Stream Resume & Close Semantics

- **Exact byte counting**: Increment `bytes_received: u64` strictly for every post-`WELCOME` stream byte **consumed by your application** across all connections of this stream (never count `WELCOME` itself, and never count unread bytes sitting in a WebSocket client buffer).
- **Resuming**: On unexpected disconnect or close `1001 keepalive timeout`, open a fresh WS to `GET /api/data` with the same subprotocol and send `RESUME(latest_reconnect_token, bytes_received)`. Resume requires **no** control connection and **no** new ticket. The server replies with a new `WELCOME` (new `reconnect_token`, **same** `channel_nonce`) and continues seamlessly from byte `bytes_received`.
- **Clean termination**: Closing `/api/data` with WS status **`1000`** immediately terminates the server-side stream. Any other close code or TCP drop leaves the stream buffered for `resume_ttl_ms` (60s).

| Close Status | Reason | Meaning & Action |
| --- | --- | --- |
| `1000` | *(empty)* | Normal stream end. Do not resume. |
| `1001` | `keepalive timeout` | Missed pings. **Attempt `RESUME`.** |
| `1001` | `data provider canceled` | Server shutting down or client detached/left. Wait for control reconnect, then re-ticket. |
| `1008` | `data provider unavailable` | Unknown `data_ref_id` or unsupported subprotocol. Do not retry. |
| `1011` | `data provider failed` | Internal block stream error. Re-ticket with backoff. |
| `1013` | `data reconnect required` | Output overflowed resume buffer while away. **Call `data.connect` + `HELLO`** (fresh snapshot). |
| `4000` | `block closed` | Block closed permanently. Tear down terminal; do not reconnect. |
| `1002`/`1008`/`1011`/`1013` | `data handshake rejected` | Preceded by `REJECT` binary frame (check reason byte). |

### 4.4 Bidirectional Binary Streams (`rex.data.bytes.v1`)

Used for drag-and-drop file uploads (`com.superlogical.terminal.upload_begin`), file downloads (`download_begin`), `com.superlogical.fs.*`, and TCP tunnels (`com.superlogical.tunnel`, `data_ref_id: "conn:<name>"`, owner-only):

1. Call `upload_begin` / `download_begin` (returns `ticket` + `expires_in_ms`) or `data.connect` with `subprotocol: "rex.data.bytes.v1"`.
2. Open `GET /api/data` with `Sec-WebSocket-Protocol: rex.data.bytes.v1` and perform the same 17-byte `HELLO` -> 32-byte `WELCOME` handshake.
3. Exchange raw binary frames for the file/socket payload.
   - **Upload (`upload_begin`)**: Send file bytes, then close/half-close the stream; once all files in a batch complete, the Rex terminal types their remote paths as if pasted.
   - **Download (`download_begin`)**: Read binary frames until EOF (`1000` close). Relative paths resolve against the foreground process CWD; only regular files are allowed.
   - *Carrier note*: Pure WebSockets cannot independently half-close the send side while keeping the receive side open (as verified by `/usr/bin/rex` string `"the carrier cannot half-close a stream"` — true TCP-style half-close requires the QUIC carrier or unidirectional stream completion). For unidirectional file uploads/downloads over WebSocket, closing with `1000` after flushing upload bytes or reading until `1000` on download completes the transfer.

---

## 5. Input, Ownership, Theme & Terminal Escape Protocol Contract

### 5.1 Keyboard, Mouse, Paste & Focus Input

#### Preferred Keyboard Path: `client.key`
Always send user keypresses (when not intercepted by a local `omarex` client-only binding) through `client.key` so **server-side Lua keybindings (`init.lua` / `tr3x.lua`) get first chance** before the key reaches the terminal PTY:

```json
{
  "type": "request",
  "id": "k-1",
  "method": "client.key",
  "payload": {
    "session_id": "session:<uuid>",
    "block_id": "block:<uuid>",
    "key": {
      "key": "key_i",
      "mods": ["ctrl", "super"],
      "consumed_mods": [],
      "text": "",
      "unshifted_codepoint": 105,
      "composing": false,
      "action": "press"
    },
    "prefix": []
  }
}
```

- `key.key`: Physical W3C `KeyboardEvent.code` in `snake_case` (`key_a`..`key_z`, `digit_0`..`digit_9`, `bracket_left`, `bracket_right`, `enter`, `escape`, `backspace`, `tab`, `space`, `arrow_up`, `f1`..`f24`, `intl_backslash`, `unidentified`, etc.) — **layout-independent** so German/DE and international keyboards work cleanly!
- `key.text`: Layout-translated UTF-8 character(s) produced by the keypress (from `winit` / XKB).
- `key.unshifted_codepoint`: Unicode scalar value of the key without Shift/Ctrl (essential for Kitty Keyboard Protocol & international layouts).
- `key.consumed_mods`: Modifiers consumed by XKB to produce `text` (e.g., `["shift"]` or `["alt"]` for AltGr `@` on DE keyboards — prevents Rex/libghostty from double-applying Alt as ESC prefix!).
- `key.action`: `"press"`, `"repeat"`, or `"release"`.
- `prefix`: Optional array of earlier `key` objects when forwarding a multi-key chord via `client.key.forward`.
- Response: `{"handled": true}` if consumed by Lua/terminal.

#### Direct Block Input Events: `block.event`
Clients may send **only** 5 event types via `block.event { session_id, block_id, event: { type, payload } }`:

| `event.type` | Exact `payload` Fields | Notes |
| --- | --- | --- |
| `key` | Same `key` object as `client.key` | Bypasses Lua keymap; use only when intentionally bypassing bindings. |
| `paste` | `{"text": "..."}` | Server automatically wraps with bracketed paste (`\x1b[200~...\x1b[201~`) if mode 2004 is enabled and strips unsafe sequences. |
| `focus` | `{"action": "in" \| "out"}` | Server emits `\x1b[I` / `\x1b[O` when DEC mode 1004 (focus reporting) is active. |
| `mouse` | `{"action": "press"\|"release"\|"move", "button": "left"\|"middle"\|"right"\|"none"\|..., "buttons": [...], "mods": [...], "x": f64, "y": f64, "pixel": bool}` | Forwarded to PTY only when terminal mouse tracking is active; otherwise handled locally for selection/links. |
| `wheel` | `{"x": f64, "y": f64, "pixel": bool, "delta_x": f64, "delta_y": f64, "delta_z": f64, "delta_mode": "pixel"\|"line"\|"page", "mods": [...]}` | Forwarded when mouse tracking / alternate scroll is active; otherwise scrolls local `GhosttyTerminal` viewport. |

*(Fallback raw input: `com.superlogical.terminal.write { args: { data: "..." } }` writes directly to the PTY without keymap or key-encoder processing.)*

### 5.2 Block Ownership, Resize & Theme Handoff

- **Ownership model**: Each block has at most one `owner` (`client_id`).
  - `session.own_block { session_id, block_ids, mode: "take" | "request" }`:
    - `"request"` claims ownership only if currently unowned (safe default on focus/connect).
    - `"take"` forcibly transfers ownership to this client (used on explicit user resize, typing/interaction if configured, and `tr3x`'s `t3_fit` action).
  - `session.disown_block { session_id, block_ids }`: Releases ownership (used when minimizing/hiding a floating layer or closing a view without killing the block).
- **Desired vs. Applied `resize` & `set_theme`**:
  - Calling `com.superlogical.terminal.resize` or `set_theme` **applies immediately** if the caller owns the block (or if the block is unowned); otherwise the server records the caller's values as **desired** and applies them automatically if/when ownership transfers to that caller.
  - On Omarchy live theme reload (`~/.config/omarchy/current/theme/`), `omarex` updates local UI chrome AND calls `com.superlogical.terminal.set_theme` on owned terminal blocks with:
    ```json
    {
      "foreground": "#cdd6f4",
      "background": "#1e1e2e",
      "cursor": "#f5e0dc",
      "palette": ["0=45475a", "1=f38ba8", "..."],
      "scheme": "dark"
    }
    ```

### 5.3 Terminal Escape Sequences & Protocols Used by Rex & `tr3x`

| Protocol / Sequence | Wire Format | Rex Server Behavior | `omarex` Client Requirement |
| --- | --- | --- | --- |
| **OSC 7501** (Program Status) | Set: `ESC ] 7501 ; state=<s1>:kind=<k>:progress=<0..100>:id=<id>:app=<app>:title=<b64>:msg=<b64> ST`<br>Query: `ESC ] 7501 ; ? ST` -> `ESC ] 7501 ; ?:states=idle,working,done,blocked,error:kinds=permission,question,auth ST` | Parsed server-side; stored per block; broadcast via `program_status_changed` / `program_status_removed` and `com.superlogical.terminal.program_status`. Advertised in `xterm-rex` terminfo (`Pst=\E]7501;%p1%s\E\\`). | Subscribe to `program_status_changed`/`removed`; query `program_status` on lazy inspect if needed; render status dot, `kind` attention badge (`permission`/`question`/`auth`), progress ring, and `app` icon in sidebar/tab/pane chrome. |
| **OSC 7577** (Program Resume) | `ESC ] 7577 ; ... ST` | Emits `program_resume_changed` and exposes `program_resume_records`. | Store metadata for session/thread resume UX. |
| **OSC 8** (Hyperlinks) | `ESC ] 8 ; params ; uri ST` | Passed through stream & snapshot into `libghostty-vt` grid. | Detect `GhosttyCell.hyperlink` bit (`bit 45`); resolve URI via `ghostty_grid_ref_hyperlink_uri()`; underline on `Ctrl+hover` and open on `Ctrl+click`. |
| **OSC 9 / 777** (Notifications) & **OSC 9;4** (Progress) | `ESC ] 9 ; body ST`, `ESC ] 777 ; notify ; title ; body ST`, `ESC ] 9 ; 4 ; state ; progress ST` | Emits `desktop_notification` & `progress_report` block events. | Trigger Linux desktop notification when app/pane is unfocused; render tab/pane progress bar for `progress_report`. |
| **OSC 52 & Mode 5522 / OSC 5522** (Clipboard & Rich Image Paste) | Mode `CSI ? 5522 h` enables paste-event protocol; `ESC ] 5522 ; type=read ... ST` negotiates MIME types (`image/png`, `text/plain`, `text/uri-list`). | Emits `clipboard_written` for OSC 52 (`clipboard_read` to fetch). `tr3x` enables `?5522h` to receive pasted PNG images cleanly over remote/local sessions. | Support Wayland clipboard copy/paste (`wl-clipboard` / `arboard`). When pasting an image into a terminal with mode 5522 active, support `upload_begin` + `pane.paste` / OSC 5522 MIME handoff. |
| **Mode 2031 / CSI ? 996 n / CSI ? 997 n & OSC 11** (Color Scheme) | `CSI ? 2031 h`, query `CSI ? 996 n` -> reply `CSI ? 997 ; 1 n` (dark) or `CSI ? 997 ; 2 n` (light). | Rex server answers `CSI ? 996 n` and pushes `CSI ? 997 ; 1\|2 n` on changes using the owning client's `set_theme.scheme`. **Note (`tr3x/src/scheme.rs`)**: Rex server does *not* currently answer OSC 11 queries—it relies on Mode 2031 (`996n`/`997n`). | Always populate **both** `background` AND explicit `"scheme": "dark" \| "light"` in `com.superlogical.terminal.set_theme` on connect and whenever Omarchy theme switches! |
| **Kitty Keyboard Protocol** (`CSI > 1 u`) | `tr3x` enables disambiguate mode (`flags = 1`) so `Shift+Enter` -> `CSI 13 ; 2 u`, `Super+Backspace` -> `CSI 127 ; 9 u`. | Server translates `client.key` / `block.event(key)` via its embedded `libghostty` encoder according to active terminal flags. | Send accurate `key`, `mods`, `consumed_mods`, `text`, `unshifted_codepoint`, `action` via `client.key` (and if encoding locally, use `ghostty_key_encoder_*` fed with `GHOSTTY_TERMINAL_DATA_KITTY_KEYBOARD_FLAGS`). |
| **DECSTBM Scroll Regions & Mode 2026** | `CSI top ; bottom r`, `CSI ? 2026 h/l` (synchronized output). | Handled natively by `libghostty-vt`. | Enable `GHOSTTY_TERMINAL_OPT_RENDER_HOLD` so `omarex` holds frame swaps during Mode 2026 atomic TUI repaints (eliminating tearing/flicker in `tr3x` chat redraws). |
| **Kitty Graphics Protocol** (`APC G ...`) | `tr3x` sends `f=100,t=f` (PNG via file medium) for provider logos, project icons, attachments, and HTML preview renders. | Rex server (`kitty_rewrite.go`) rewrites file-medium graphics into inline data in the stream & `GHOSTSNP` snapshot so remote/sandboxed clients receive the PNG bytes. | **MANDATORY**: Register `GHOSTTY_SYS_OPT_DECODE_PNG` at startup and set non-zero `KITTY_IMAGE_STORAGE_LIMIT` (e.g., `128 MiB`) on both `GhosttySnapshotDecoder` and `GhosttyTerminal`! Render placements from `GhosttyRenderStateImageIterator` (`gpu-renderer`). |

---

## 6. Custom UI, Keymap, Lua & `tr3x` First-Class Compatibility Contract

### 6.1 Keymap Registration & Layering (`client.keymap`)

There is **no** `client.register` RPC in Rex rev 24. A GUI client registers its identity in `hello.client` and registers its native actions and default keybindings by calling `client.keymap`:

```json
{
  "type": "request",
  "id": "km-1",
  "method": "client.keymap",
  "payload": {
    "actions": [
      { "name": "session.select", "kind": "client", "title": "Select Session / Window / Block", "category": "Navigation" },
      { "name": "pane.paste", "kind": "client", "title": "Paste into Pane", "category": "Edit" },
      { "name": "pane.split", "kind": "client", "title": "Split Pane", "category": "Layouts" },
      { "name": "pane.close", "kind": "client", "title": "Close Pane", "category": "Layouts" },
      { "name": "pane.zoom", "kind": "client", "title": "Toggle Pane Zoom", "category": "Layouts" },
      { "name": "pane.float", "kind": "client", "title": "Float / Tile Pane", "category": "Layouts" },
      { "name": "pane.focus", "kind": "client", "title": "Focus Pane", "category": "Navigation" },
      { "name": "client.tab.goto", "kind": "client", "title": "Go to Tab", "category": "Navigation" },
      { "name": "client.font.increase", "kind": "client", "title": "Increase Font Size", "category": "View" },
      { "name": "client.font.decrease", "kind": "client", "title": "Decrease Font Size", "category": "View" },
      { "name": "client.font.reset", "kind": "client", "title": "Reset Font Size", "category": "View" },
      { "name": "client.sidebar.toggle", "kind": "client", "title": "Toggle Sidebar", "category": "View" },
      { "name": "client.palette.open", "kind": "client", "title": "Command Palette", "category": "View" },
      { "name": "client.config.reload", "kind": "client", "title": "Reload Rex Config", "category": "System" },
      { "name": "client.mode.enter", "kind": "client", "title": "Enter Key Mode", "category": "Keymap" },
      { "name": "client.mode.exit", "kind": "client", "title": "Exit Key Mode", "category": "Keymap" },
      { "name": "client.mode.exit_all", "kind": "client", "title": "Exit All Key Modes", "category": "Keymap" }
    ],
    "defaults": [ /* omarex built-in default bindings */ ],
    "overrides": [ /* optional user local overrides */ ]
  }
}
```

- **Server response**:
  - `bindings`: Merged list `[{ "keys": ["ctrl+cmd+i"], "action": "t3_inbox", "args"?: {...}, "mode"?: "...", "source": "tr3x.lua:430" }]`.
  - `actions`: Unified command-palette catalog combining `kind: "client"`, `kind: "lua"` (from `init.lua`/`tr3x.lua`), and `kind: "block"` (e.g., `com.superlogical.terminal.clear`, `reset`, etc.) — `60` actions observed live on this machine!
  - `modes`: Custom key modes `[{ "name", "exclusive": bool, "blocked": bool }]` (if defined in `init.lua`).
  - `diagnostics`: Lua parse/binding warnings (surface cleanly in `omarex` diagnostics/toast, never crash).
  - `config`: `{ "path": "~/.config/rex/init.lua", "exists": true, "generation": 2, "instance_id": "...", "hash": "<sha256>" }`.
- **Hot reload**: On `keymap_changed { generation, hash }`, re-call `client.keymap` whenever `hash != last_config_hash`.
- **Special action `client.key.forward`**: When a binding resolves to `client.key.forward`, forward the triggering key (plus any prefix keys in the chord) directly to `client.key`.
- **Linux `cmd` <-> `super` modifier compatibility (CRITICAL for `tr3x`)**:
  - Live `tr3x.lua` registers all 7 bindings using `cmd` (e.g., `ctrl+cmd+i`, `ctrl+cmd+n`, `ctrl+cmd+[`).
  - On Linux / Wayland / Hyprland (`winit`), the Windows/Super key is `ModifiersState::SUPER`.
  - `omarex`'s keymap matcher (`tr3x-bridge`) **MUST** treat `cmd` and `super` as equivalent (or map Wayland `Super` to `cmd` when matching `client.keymap` bindings and when emitting `client.key` `mods` if needed, with a user-configurable option in case Hyprland binds `Super` chords).

### 6.2 Dual Client-Action Execution Contract

A Lua script or external CLI (`tr3x`, `rex -C <id> do ...`) can instruct `omarex` to perform a client action through **two distinct protocol paths** — `omarex` **MUST implement both**:

1. **Inline queue returned by `client.action`** (used by `rex.client.queue("session.select", target)` inside `tr3x.lua`):
   ```json
   {
     "result": null,
     "client_actions": [
       {
         "action": "session.select",
         "args": {
           "session_id": "session:01a216bc-52f2-72c7-aa9a-e94e1d60b77a",
           "window_id": "window:01a216bc-5329-74b7-8d08-26ca9f808eb6",
           "block_id": "block:01a216bc-531c-781a-84b4-fc98fdf64fb4"
         }
       }
     ],
     "ordered_after": 124
   }
   ```
   Execute every entry in `client_actions` sequentially in order (after applying state up to `ordered_after` if set).

2. **Server-pushed `client_perform_requested` event** (used by `rex -C <client_id> do session.select ...` and `tr3x` background commands like `tr3x/src/paste.rs` -> `pane.paste` or `tr3x/src/rex.rs` -> `session.select`):
   - Inspect `action`, `args`, `session_id`, `block_id`, and `requester.principal` (authorize using `principal`, **never** `requester.info`).
   - Execute the action immediately and answer with `client.reply`:
     - Success: `{"call_id": "...", "result": {}}`
     - Unsupported / refused: `{"call_id": "...", "error": {"code": "not_supported" | "refused", "message": "..."}}`
   - Answer quickly: the server queues subsequent `client_perform_requested` calls behind the active one.

#### Minimum Required Client Actions for Full `tr3x` Compatibility
- **`session.select`**: Accepts `{ "session_id"?: string, "window_id"?: string, "block_id"?: string }`. Switches active session (attaching if needed), activates `window_id` (calling `session.focus_window` if needed), focuses `block_id`, unminimizes its floating layer if minimized, and brings the `omarex` window to attention.
- **`pane.paste`**: Accepts optional `session_id`/`block_id` (defaults to focused block) and replays clipboard/image paste into that terminal (invoked by `tr3x` remote image paste bridge).
- Standard workspace actions: `pane.split`, `pane.close`, `pane.zoom`, `pane.float`, `pane.focus`, `client.tab.goto` (supports `-1` for last tab), `client.font.*`, `client.sidebar.toggle`, `client.config.reload`, `client.mode.*`.

### 6.3 Native `rex.ui` Prompt Engine (`client_prompt_requested` / `client_prompt_ended`)

`tr3x` drives interactive thread switching, snooze pickers, rename dialogs, delete confirmations, model pickers, project pickers, and inbox answering **entirely through Rex's `rex.ui.input` and `rex.ui.pick` protocol**:

1. Server broadcasts `client_prompt_requested` to all connected `kind: "app"` clients:
   ```json
   {
     "call_id": "prompt-7",
     "action": "t3_inbox",
     "timeout_ms": 60000,
     "prompt": {
       "kind": "pick",
       "title": "Go to Thread That Needs You",
       "placeholder": "Filter threads…",
       "confirm": "Open",
       "multiple": false,
       "allow_other": false,
       "items": [
         {
           "id": "thread-uuid-1",
           "title": "Fix GPU atlas packing",
           "subtitle": "omarex · main",
           "icon": "branch",
           "detail": "permission",
           "selected": false
         }
       ]
     }
   }
   ```
2. **Rendering rules**:
   - `kind == "input"`: Single-line text input prefilled with `value`, hint `placeholder`, submit button labeled `confirm` (default `"OK"`).
   - `kind == "pick"`: Fuzzy-filterable modal list displaying `title`, `subtitle`, `icon` (left), `detail` (right-aligned badge/text).
     - If `multiple == true`: Space toggles selection (`item.selected` sets initial checked state); Enter submits `ids` (may be empty `[]`).
     - If `allow_other == true` (only when `multiple == false`): User may submit custom typed text not matching any item.
   - Unknown `kind`: Immediately reply `{"call_id": "...", "error": {"code": "not_supported", "message": "unknown prompt kind"}}`.
   - Start a local timer for `timeout_ms` to auto-close if expired.
3. **Replying via `client.reply`** (unordered RPC!):
   - `input` submitted: `{"call_id": "...", "result": {"text": "<string>"}}`
   - `pick` item(s) chosen: `{"call_id": "...", "result": {"ids": ["<id1>", ...]}}`
   - `pick` with `allow_other` custom text: `{"call_id": "...", "result": {"text": "<string>"}}`
   - User pressed `Esc` / dismissed: `{"call_id": "...", "result": {"cancelled": true}}`
4. **Race / Cancellation (`client_prompt_ended`)**:
   - If another client answers first or the prompt times out, the server sends `client_prompt_ended { call_id, reason }`. Immediately close the modal matching `call_id` **without** sending `client.reply`.

#### Standard `rex.ui` Icon Set (All 16 Names)
`omarex` must map all 16 official icon names to crisp vector/glyph icons (with a clean fallback glyph for unknown future names):
`document`, `folder`, `branch`, `terminal`, `code`, `server`, `database`, `package`, `globe`, `link`, `person`, `tag`, `clock`, `star`, `gear`, `plus`.

### 6.4 Multi-Server Action Proxy (`client_call_requested`)

When `omarex` passes `servers` + `server` to `client.action` (invoking a Lua action from the home server against a remote server's session), the home server emits `client_call_requested { call_id, server, method, args }`. `omarex` must forward `(method, args)` to its open control connection for `server`, and send the result or error back on the originating connection via `client.reply { call_id, result | error }` within **10 seconds**.

### 6.5 Complete `tr3x` (`0.1.8`) Actions, Bindings & Conventions

#### Default Keybindings (7, Live-Verified from `tr3x.lua`)
| Chord (`tr3x.lua`) | Action | Line | Notes |
| --- | --- | --- | --- |
| `ctrl+cmd+i` | `t3_inbox` | `tr3x.lua:430` | Pick/jump to thread needing attention |
| `ctrl+cmd+n` | `t3_new` | `tr3x.lua:431` | Create new T3 thread |
| `ctrl+cmd+o` | `t3_project` | `tr3x.lua:432` | Open project picker -> thread |
| `ctrl+cmd+a` | `t3_answer` | `tr3x.lua:433` | Answer permission/question prompt in thread |
| `ctrl+cmd+[` | `t3_parent` | `tr3x.lua:434` | Jump to parent thread of subagent |
| `ctrl+cmd+]` | `t3_child` | `tr3x.lua:435` | Jump to subagent child thread |
| `ctrl+cmd+t` | `t3_fit` | `tr3x.lua:480` | Call `session.own_block(mode:"take")` across T3 sessions |

#### All 35 `tr3x` Lua Actions (Live-Verified from `client.keymap`)
- **Thread lifecycle & triage**: `t3_settle` (*Settle Thread*), `t3_reopen_settled` (*Reopen Last Settled*), `t3_pin` (*Pin Thread*), `t3_unpin` (*Unpin Thread*), `t3_rename` (*Rename Thread…*), `t3_retitle` (*Regenerate Thread Title*), `t3_fork` (*Fork Thread*), `t3_archive` (*Archive Thread*), `t3_delete` (*Delete Thread…*), `t3_stop` (*Stop Thread*), `t3_force_stop` (*Force Stop Thread*), `t3_model` (*Change Model…*).
- **Snooze**: `t3_snooze` (*Snooze Thread…* — `rex.ui.pick`), `t3_snooze_hour` (*Snooze Thread for 1 Hour*), `t3_snooze_3h` (*Snooze Thread for 3 Hours*), `t3_snooze_tomorrow` (*Snooze Thread Until Tomorrow*).
- **Approval & inbox**: `t3_inbox` (*Go to Thread That Needs You*), `t3_approve` (*Approve*), `t3_approve_always` (*Approve Always*), `t3_decline` (*Decline*), `t3_answer` (*Answer…*), `t3_answer_next` (*Answer Next…*).
- **Navigation & creation**: `t3_parent` (*Go to Thread This Subagent Works For*), `t3_child` (*Go to Subagent*), `t3_new` (*New Thread*), `t3_project` (*Open Project…*), `t3_find` (*Find Thread…*), `t3_open` (*Open Thread in T3 Code*).
- **Git / PRs**: `t3_pr` (*Show Pull Request*), `t3_pr_create` (*Create Pull Request*).
- **Multi-device & maintenance**: `t3_fit` (*Fit to This Device*), `t3_pair` (*Pair Another Machine…*), `t3_update` (*Update tr3x, T3 and Rex…*), `t3_sessions_per_machine` (*Sessions: One per Machine*), `t3_sessions_merged` (*Sessions: One for All Machines*).

#### `tr3x` Tags, Naming & UX Conventions (`crates/tr3x-bridge`)
1. **Block label format**: `t3:<env8>/<thread_id>` (or `t3#<hash>` when the full tag exceeds 128 chars). Parse this prefix to detect `tr3x`-managed panes and group/badge them cleanly in the sidebar.
2. **Session naming**: `"T3 Code"` (merged mode) or `"T3 Code · <machine>"` (per-machine mode).
3. **New-tab hijack**: `tr3x sync` watches for any new unlabelled shell window created in a `"T3 Code"` session and automatically converts it into a new `tr3x` thread picker/session (`tr3x new`). Therefore, pressing `New Window` inside `"T3 Code"` simply calls standard `session.new_window` with a shell block — `tr3x` seamlessly takes over!
4. **Floating layers**: `tr3x` uses `session.new_layer` + `session.disown_block` for quick ephemeral drawers/previews; `omarex` must render floating layers above tiled layers with drag/resize (`session.resize_layer`) and minimize/restore (`session.set_layer_state`).
5. **OSC 7501 `app` provider badges**: `tr3x` emits OSC 7501 with `app` set to one of: `claude`, `codex`, `cursor`, `grok`, `gemini`, `opencode`, `pi`, `antigravity`, `t3code`, alongside `state` (`idle`, `working`, `done`, `blocked`, `error`), `kind` (`permission`, `question`, `auth`), and optional `progress` (`0..100`). Render distinct provider glyphs and pulsing/attention indicators directly in the sidebar and tab bar.
6. **Zero duplicate state**: Never spawn `tr3x` subprocesses from `omarex` to duplicate sync logic—interact solely through Rex's `client.keymap`, `client.action`, `client.key`, `rex.ui`, and OSC 7501 events.

---

## 7. `libghostty-vt` C API & FFI Contract on Omarchy (`ghostty 1.4.0-1`)

### 7.1 Installed Files & CRITICAL Arch Packaging Bug

- **Shared library**: `/usr/lib/libghostty-vt.so` -> `libghostty-vt.so.0` -> `/usr/lib/libghostty-vt.so.0.1.0` (`655` exported C symbols).
- **Umbrella header**: `/usr/include/ghostty/vt.h` (includes all 30 sub-headers under `/usr/include/ghostty/vt/`).
- **Sub-headers**: `allocator.h`, `build_info.h`, `color.h`, `focus.h`, `formatter.h`, `grid_ref.h`, `io.h`, `key.h`, `key/{encoder,event}.h`, `kitty_graphics.h`, `modes.h`, `mouse.h`, `mouse/{encoder,event}.h`, `osc.h`, `paste.h`, `point.h`, `render.h`, `result.h`, `screen.h`, `search.h`, `selection.h`, `sgr.h`, `snapshot.h`, `style.h`, `sys.h`, `terminal.h`, `types.h`, `wasm.h`.

> [!CAUTION]
> **Broken `/usr/share/pkgconfig/libghostty-vt.pc` on Arch/Omarchy**:
> The installed `.pc` file contains a stale build-root prefix (`prefix=/src/ghostty/src/ghostty-1.4.0/build/usr`), causing `pkg-config --cflags --libs libghostty-vt` to emit non-existent include/lib paths!
> `crates/vt-engine/build.rs` **MUST** validate that any `pkg-config` prefix actually exists on disk, and otherwise probe `/usr/include/ghostty/vt.h` + `/usr/lib/libghostty-vt.so` directly (with env overrides `GHOSTTY_VT_INCLUDE_DIR` / `GHOSTTY_VT_LIB_DIR`), emitting `cargo:rustc-link-search=native=/usr/lib` and `cargo:rustc-link-lib=dylib=ghostty-vt`.

### 7.2 Live-Verified Build Info & ABI Manifest

Queried live from `/usr/lib/libghostty-vt.so` via `ghostty_build_info()` and `ghostty_type_json()`:

- **Target ABI**: `x86_64-linux-gnu`, little-endian, `pointer_size = 8`, `usize_size = 8`, `max_alignment = 16`.
- **Feature flags**: `SIMD = true`, `KITTY_GRAPHICS = true`, `TMUX_CONTROL_MODE = false`, `OPTIMIZE = 3 (RELEASE_FAST)`, `version = "0.1.0-dev"`.
- **Versioning pattern**: Every extensible C input/output struct begins with `size_t size;` — callers **MUST** initialize `.size = std::mem::size_of::<T>()` before calling any C function (otherwise `GHOSTTY_INVALID_VALUE` is returned).

#### Exact Packed `GhosttyCell` Layout (`u64`, 8 bytes, align 8)
Using `GHOSTTY_RENDER_STATE_ROW_DATA_CELLS_RAW` returns a contiguous `const GhosttyCell*` slice for the entire row in **1 FFI call**, allowing zero-FFI cell decoding in Rust:

```
Bits [0..=1]   (2 bits):  content_tag (GhosttyCellContentTag: 0=CODEPOINT, 1=CODEPOINT_GRAPHEME, 2=BG_COLOR_PALETTE, 3=BG_COLOR_RGB)
Bits [2..=25]  (24 bits): content union:
                            CODEPOINT / CODEPOINT_GRAPHEME -> bits [2..=22] (u21 Unicode scalar)
                            BG_COLOR_PALETTE               -> bits [2..=9]  (u8 palette index)
                            BG_COLOR_RGB                   -> r=[2..=9], g=[10..=17], b=[18..=25]
Bits [26..=41] (16 bits): style_id (u16; 0 = default style; non-zero -> query style once via row_cells / cache)
Bits [42..=43] (2 bits):  wide (GhosttyCellWide: 0=NARROW, 1=WIDE, 2=SPACER_TAIL, 3=SPACER_HEAD)
Bit  [44]      (1 bit):   protected (bool)
Bit  [45]      (1 bit):   hyperlink (bool — true if OSC 8 hyperlink attached)
Bits [46..=47] (2 bits):  semantic_content (0=OUTPUT, 1=INPUT, 2=PROMPT)
Bits [48..=63] (16 bits): reserved/zero
```

#### Key Struct Sizes & Offsets (from `ghostty_type_json()`)
- **`GhosttyStyle`** (`72` bytes, align `8`): `size@0 (8)`, `fg_color@8 (16)`, `bg_color@24 (16)`, `underline_color@40 (16)`, `bold@56`, `italic@57`, `faint@58`, `blink@59`, `inverse@60`, `invisible@61`, `strikethrough@62`, `overline@63`, `underline@64 (i32: 0=NONE,1=SINGLE,2=DOUBLE,3=CURLY,4=DOTTED,5=DASHED)`.
- **`GhosttyStyleColor`** (`16` bytes, align `8`): `tag@0 (i32: 0=NONE, 1=PALETTE, 2=RGB)`, `value@8 (8: u8 palette OR GhosttyColorRgb {r,g,b})`.
- **`GhosttyRenderStateColors`** (`792` bytes, align `8`): `size@0 (8)`, `background@8 (3)`, `foreground@11 (3)`, `cursor@14 (3)`, `cursor_has_value@17 (1)`, `palette@18 ([GhosttyColorRgb; 256] = 768 B)`.
- **`GhosttyRenderStateRowId`** (`16` bytes): opaque `bits` token for per-row GPU instance cache validation.
- **`GhosttyTerminalScrollbar`** (`24` bytes): `total: u64`, `offset: u64`, `len: u64`.
- **`GhosttySelection`** (`64` bytes): `size@0 (8)`, `start@8 (GhosttyGridRef, 24 B)`, `end@32 (GhosttyGridRef, 24 B)`, `rectangle@56 (bool)`.

### 7.3 Process-Global Initialization (`sys.h`) — MANDATORY for Kitty PNGs

`libghostty-vt` does **not** bundle a PNG decoder; it invokes a caller-supplied callback via `ghostty_sys_set()` when decoding Kitty `f=100` PNG payloads (both during `GHOSTSNP` decode and live `vt_write`):

```c
// /usr/include/ghostty/vt/sys.h
typedef bool (*GhosttySysDecodePngFn)(
    void* userdata,
    const GhosttyAllocator* allocator,
    const uint8_t* png_bytes,
    size_t png_len,
    GhosttySysImage* out_image // { uint32_t width, height; uint8_t* data; size_t data_len; } RGBA8
);
GhosttyResult ghostty_sys_set(GhosttySysOption option, const void* value);
```

- Call `ghostty_sys_set(GHOSTTY_SYS_OPT_DECODE_PNG, &decode_png_fn)` **once at startup** before creating any snapshot decoder or terminal. Allocate `out_image->data` using the provided `allocator` (or `libc::malloc` if `allocator == NULL`), returning tightly packed RGBA8 (`width * height * 4` bytes).
- Optionally install `GHOSTTY_SYS_OPT_LOG` (`ERROR`/`WARNING`/`INFO`/`DEBUG`) to route `libghostty-vt` diagnostics into `tracing`.

### 7.4 Streaming Snapshot Decode Pipeline (`snapshot.h`)

To prevent blocking startup or pane switch on large scrollback/image snapshots:

```c
// /usr/include/ghostty/vt/snapshot.h
GhosttyResult ghostty_snapshot_decoder_new(const GhosttyAllocator* alloc, GhosttySnapshotDecoder* out, GhosttySnapshotDecoderSource source);
GhosttyResult ghostty_snapshot_decoder_new_buf(const GhosttyAllocator* alloc, GhosttySnapshotDecoder* out, const uint8_t* buf, size_t len);
GhosttyResult ghostty_snapshot_decoder_set(GhosttySnapshotDecoder dec, GhosttySnapshotDecoderOption opt, const void* val);
GhosttyResult ghostty_snapshot_decoder_get(GhosttySnapshotDecoder dec, GhosttySnapshotDecoderData data, void* out);
GhosttyResult ghostty_snapshot_decoder_ready(GhosttySnapshotDecoder dec, GhosttyTerminal* out_term);
GhosttyResult ghostty_snapshot_decoder_next(GhosttySnapshotDecoder dec);
void          ghostty_snapshot_decoder_free(GhosttySnapshotDecoder dec);
```

1. **Before `ready()`**, configure options:
   - `GHOSTTY_SNAPSHOT_DECODER_OPT_COMPRESS_HISTORY = true` (keeps historical scrollback pages compressed in RAM — huge memory win on 23+ T3 windows!).
   - `GHOSTTY_SNAPSHOT_DECODER_OPT_KITTY_IMAGE_STORAGE_LIMIT = 134_217_728` (128 MiB; default may be 0/disabled).
2. **Progressive streaming**:
   - Feed `/api/data` chunks through a custom `GhosttySnapshotDecoderSource` (`read` callback) — OR accumulate until `ready()` returns `GHOSTTY_SUCCESS`.
   - As soon as `ready(&term)` returns `GHOSTTY_SUCCESS`, **`term` is immediately renderable** (active viewport & primary/alt screens are populated!).
   - Continue calling `ghostty_snapshot_decoder_next(dec)` incrementally (e.g., on the background I/O worker) to hydrate scrollback history and Kitty images until it returns **`GHOSTTY_NO_VALUE`** (signaling the `FINISH` record was consumed).
   - Query `GHOSTTY_SNAPSHOT_DECODER_DATA_SOURCE_OFFSET` (`uint64_t`) to find the exact byte offset where the `GHOSTSNP` ends and Phase 2 (`rex1;` APCs + live VT) begins, then call `ghostty_snapshot_decoder_free(dec)`.

### 7.5 Terminal Lifecycle, Effects & Stream Feeding (`terminal.h`)

```c
// /usr/include/ghostty/vt/terminal.h
GhosttyResult ghostty_terminal_new(const GhosttyAllocator* alloc, GhosttyTerminal* out, GhosttyTerminalOptions opts);
GhosttyResult ghostty_terminal_set(GhosttyTerminal term, GhosttyTerminalOption opt, const void* val);
GhosttyResult ghostty_terminal_get(GhosttyTerminal term, GhosttyTerminalData data, void* out);
void          ghostty_terminal_vt_write(GhosttyTerminal term, const uint8_t* data, size_t len);
GhosttyResult ghostty_terminal_resize(GhosttyTerminal term, uint16_t cols, uint16_t rows, uint32_t cell_w_px, uint32_t cell_h_px);
void          ghostty_terminal_scroll_viewport(GhosttyTerminal term, GhosttyTerminalScrollViewport behavior);
GhosttyResult ghostty_terminal_compress(GhosttyTerminal term);
void          ghostty_terminal_free(GhosttyTerminal term);
```

Configure on every `GhosttyTerminal` immediately after `snapshot_decoder_ready()`:
- `GHOSTTY_TERMINAL_OPT_USERDATA`: pointer to pane context struct.
- `GHOSTTY_TERMINAL_OPT_UNKNOWN_SEQUENCE` + `GHOSTTY_TERMINAL_OPT_UNKNOWN_MAX_BYTES = 65536`: intercepts `GHOSTTY_TERMINAL_UNKNOWN_SEQUENCE_APC` frames starting with `rex1;`, validates `channel_nonce`, and executes `resize`/`theme`/`child_exited` in exact stream order.
- `GHOSTTY_TERMINAL_OPT_RENDER_HOLD`: callback notified when Mode 2026 synchronized output begins/ends (defer GPU swap up to ~16ms while held).
- `GHOSTTY_TERMINAL_OPT_KITTY_IMAGE_STORAGE_LIMIT`: `134_217_728` (128 MiB).
- `GHOSTTY_TERMINAL_OPT_COLOR_FOREGROUND` / `BACKGROUND` / `CURSOR` / `PALETTE` / `COLOR_SCHEME`: set initially from Omarchy theme and updated on every verified `rex1;` `theme` APC.
- **Do NOT set `WRITE_PTY`** to write back to Rex automatically for server-answered queries (Rex's server-side `libghostty` already answers DA1/DA2/DSR/OSC 7501/Mode 2031 queries on the PTY). Leave `WRITE_PTY = NULL` so the client never sends duplicate replies to remote programs!
- Periodically call `ghostty_terminal_compress(term)` on inactive/background panes to compact scrollback memory.

### 7.6 Thread-Safe 2-Phase GPU Rendering (`render.h` & `kitty_graphics.h`)

`omarex` runs VT ingestion (`vt_write`) on a Tokio/background worker and rendering on the `winit`/`wgpu` thread. Use `libghostty-vt`'s split update API so the terminal mutex is held for **microseconds only**:

```c
// /usr/include/ghostty/vt/render.h
GhosttyResult ghostty_render_state_new(const GhosttyAllocator* alloc, GhosttyRenderState* out);
GhosttyResult ghostty_render_state_set(GhosttyRenderState rs, GhosttyRenderStateOption opt, const void* val); // e.g. OVERSCAN
GhosttyResult ghostty_render_state_begin_update(GhosttyRenderState rs, GhosttyTerminal term); // HOLD terminal lock
GhosttyResult ghostty_render_state_end_update(GhosttyRenderState rs);                          // NO terminal lock needed!
GhosttyResult ghostty_render_state_get(GhosttyRenderState rs, GhosttyRenderStateData data, void* out);
GhosttyResult ghostty_render_state_row_iterator_new(const GhosttyAllocator* alloc, GhosttyRenderStateRowIterator* out);
bool          ghostty_render_state_row_iterator_next(GhosttyRenderStateRowIterator it);
bool          ghostty_render_state_row_iterator_next_dirty(GhosttyRenderStateRowIterator it);
GhosttyResult ghostty_render_state_row_get(GhosttyRenderStateRowIterator it, GhosttyRenderStateRowData data, void* out);
GhosttyResult ghostty_render_state_row_cells_new(const GhosttyAllocator* alloc, GhosttyRenderStateRowCells* out);
bool          ghostty_render_state_row_cells_next(GhosttyRenderStateRowCells cells);
GhosttyResult ghostty_render_state_row_cells_get(GhosttyRenderStateRowCells cells, GhosttyRenderStateRowCellsData data, void* out);
GhosttyResult ghostty_render_state_image_iterator_new(const GhosttyAllocator* alloc, GhosttyRenderStateImageIterator* out);
bool          ghostty_render_state_image_iterator_next(GhosttyRenderStateImageIterator it);
GhosttyResult ghostty_render_state_image_get(GhosttyRenderStateImageIterator it, GhosttyRenderStateImageData data, void* out);
GhosttyResult ghostty_render_state_clean(GhosttyRenderState rs);
```

#### Fast Per-Frame Render Loop (`crates/vt-engine` -> `crates/gpu-renderer`)
1. *(Optional, once)* Set `GHOSTTY_RENDER_STATE_OPT_OVERSCAN` (`{ above: 1, below: 1 }`) for smooth sub-row fractional pixel scrolling.
2. Lock `pane.terminal`:
   - `ghostty_render_state_begin_update(rs, term)`
   - Read `GHOSTTY_TERMINAL_DATA_SCROLLBAR` (`GhosttyTerminalScrollbar`) and `VIEWPORT_ACTIVE`
   - Unlock `pane.terminal`.
3. `ghostty_render_state_end_update(rs)` (lock-free; computes row dirty flags & image placements).
4. Query `GHOSTTY_RENDER_STATE_DATA_DIRTY`:
   - `FALSE (0)` -> **Skip row re-upload completely** (0 GPU work for idle panes!).
   - `PARTIAL (1)` or `FULL (2)` -> Read `COLORS` (`GhosttyRenderStateColors`), cursor fields (`CURSOR_VISIBLE`, `CURSOR_VISUAL_STYLE`, `CURSOR_VIEWPORT_X/Y`, `CURSOR_BLINKING`, `CURSOR_VIEWPORT_WIDE_TAIL`), and iterate dirty rows via `row_iterator_next_dirty()`.
   - For each row: check `ROW_DATA_ID` (`16 B`) against cached GPU row buffer; read `ROW_DATA_CELLS_RAW` (`&[GhosttyCell]`) + `ROW_DATA_SELECTION` (`start_x`, `end_x`). Only query `GhosttyRenderStateRowCells` (`STYLE`, `GRAPHEMES_UTF8`) for cells with `style_id != 0` or `content_tag == CODEPOINT_GRAPHEME` (and cache resolved `GhosttyStyle` per `style_id`!).
5. **Kitty Graphics**: Compare `GHOSTTY_RENDER_STATE_DATA_IMAGE_PLACEMENTS_GENERATION` (`u64`) against cached generation. When changed, iterate `IMAGE_ITERATOR` reading `IMAGE_ID`, `IMAGE_GENERATION`, `LAYER`, `Z`, `VIEWPORT_COL/ROW`, `GRID_COLS/ROWS`, `CELL_OFFSET_X/Y`, `SOURCE_X/Y/WIDTH/HEIGHT`, `PIXEL_WIDTH/HEIGHT`. Upload unseen `(image_id, image_generation)` textures via `ghostty_kitty_graphics_get_image(kg, image_id)` + `ghostty_kitty_graphics_image_get(...)` (`DATA`, `WIDTH`, `HEIGHT`, `FORMAT`).
6. Call `ghostty_render_state_clean(rs)`.

### 7.7 Selection, Search, Hyperlinks & Encoders (`selection.h`, `search.h`, `grid_ref.h`, `key/`, `mouse/`, `paste.h`, `focus.h`)

| Feature | Exact `libghostty-vt` Symbols | Usage in `omarex` |
| --- | --- | --- |
| **Selection Gestures** | `ghostty_selection_gesture_new/free`, `ghostty_selection_gesture_event_new/set/free`, `ghostty_selection_gesture_handle(gesture, term, ev)` | Native single/double/triple-click (cell/word/line/semantic-output), drag, rectangular selection (`Alt+drag`), and edge autoscroll (`AUTOSCROLL_TICK`) matching Ghostty feel. |
| **Copy Selected Text** | `ghostty_terminal_selection_format_len`, `ghostty_terminal_selection_format` (`unwrap`, `trim`, `emit: PLAIN`) | Extracts unwrapped/trimmed UTF-8 selection for Wayland clipboard. |
| **Scrollback Search** | `ghostty_search_new/free`, `ghostty_search_set(NEEDLE\|SELECT_NEXT\|SELECT_PREV\|SELECT_SCROLL)`, `ghostty_search_tick`, `ghostty_search_get(STATUS\|TOTAL_MATCHES\|SELECTED_INDEX\|VIEWPORT_MATCHES)` | Incremental non-blocking search across compressed scrollback with live viewport match highlights. |
| **OSC 8 Hyperlink Lookup** | `ghostty_terminal_grid_ref(term, point, &ref)`, `ghostty_grid_ref_hyperlink_uri(&ref, buf, len, &out_len)` | Resolves hovered cell's hyperlink URI on `Ctrl+hover`/`Ctrl+click`. |
| **Key / Mouse / Focus / Paste Encoders** | `ghostty_key_encoder_*`, `ghostty_mouse_encoder_*`, `ghostty_focus_encode`, `ghostty_paste_encode` / `ghostty_paste_is_safe` | Used for local PTY testing/benchmarks and validating safe paste (`ghostty_paste_is_safe`); live Rex sessions primarily route through `client.key` and `block.event`. |

---

## 8. Implementation Checklist for Downstream Crates

- **`crates/rex-proto`**:
  - Control WS (`rex.control.v1`): `hello` (`kind:"app"`, `api_revision:24`, never compress `hello` / `<512B`), `reconnect_token` 60s resume, ordered/unordered dispatch (`client.reply` unordered!), `seq` snapshot reconciliation, `session.bootstrap` 1-RTT startup, `session_view_changed` `revision` gating.
  - Data WS (`rex.data.terminal.v1` & `rex.data.bytes.v1`): 17B `HELLO` / 25B `RESUME` / 32B `WELCOME` / 2B `REJECT`, exact consumed-byte counter, `channel_nonce` verification, `1000` clean close vs `1001` resume vs `1013` re-ticket vs `4000` drop.
  - Multi-host transports: `unix://`, `https://` (Tailscale), `ssh://` (`rex __internal dial-stdio`), `hosts.*` + `hosts_changed`.
- **`crates/vt-engine`**:
  - `build.rs`: bypass broken `/usr/share/pkgconfig/libghostty-vt.pc` prefix; probe `/usr/include/ghostty/vt.h` + `/usr/lib/libghostty-vt.so` directly.
  - Startup: register `GHOSTTY_SYS_OPT_DECODE_PNG` (via `png` crate -> RGBA8).
  - Snapshot streaming: `COMPRESS_HISTORY=true`, `KITTY_IMAGE_STORAGE_LIMIT=128 MiB`, `ready()` -> immediate render -> `next()` until `GHOSTTY_NO_VALUE` -> `SOURCE_OFFSET`.
  - Live stream: `UNKNOWN_SEQUENCE` APC handler verifying `rex1;<channel_nonce>;<json>`, `RENDER_HOLD` (Mode 2026), `WRITE_PTY=NULL`, 2-phase `GhosttyRenderState` with `CELLS_RAW` + `ROW_DATA_ID` + `OVERSCAN` + `IMAGE_ITERATOR`.
- **`crates/tr3x-bridge`**:
  - `client.keymap` registration + `keymap_changed` hash refresh + `super`<->`cmd` normalization on Linux.
  - Dual action dispatcher (`client.action.client_actions` AND `client_perform_requested`) supporting `session.select` and `pane.paste`.
  - Native `rex.ui` modal (`input` & `pick`, `multiple`, `allow_other`, all 16 icons, `timeout_ms`, `client_prompt_ended`, unordered `client.reply`) + multi-server `client_call_requested` proxy.
  - OSC 7501 status tracking, `t3:<env8>/<thread>` / `t3#<hash>` tag parsing, provider `app` glyphs, and `set_theme` with explicit `scheme: "dark" | "light"` for Mode 2031 (`CSI ? 997 n`).

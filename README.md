# Gnome MCP Bridge — Desktop GUI

A native macOS/Linux window that gives you a real eyes-on view of a
running [Gnome File Bridge](https://github.com/Trans4m-Tech/Gnome-MCP)
server. Reads the audit log, lets you invoke tools, surfaces pending
approvals you can grant from the UI, and shows live `/healthz`.

Built with [egui](https://github.com/emilk/egui) — single binary, no
JavaScript toolchain, no Electron.

## Run

```sh
# in one terminal: start the GFB backend
gfb serve --transport http --port 18765 --agent kimi-desktop

# in another: launch the GUI
cargo run --release
```

The GUI auto-loads the bearer token from
`~/.gfb/agents/kimi-desktop.token` and starts polling
`http://127.0.0.1:18765/healthz` every 5 seconds.

## What the panels do

| Panel | Source | What you see |
|---|---|---|
| **Top bar** | `/healthz` | Green check + body when the server is up |
| **Connection** (left) | you type | Server URL + bearer token (password-masked) |
| **Audit log** (center) | reads `~/.gfb/audit.log` directly | Newest-first table; click a row for diff + before/after hashes |
| **Pending approvals** | reads `~/.gfb/pending/` directly | One-click `Approve` button per entry |
| **Tools palette** | `POST /rpc tools/list` | Pick a tool, edit JSON args, hit `Send`, see the response |

The audit-log and pending readers go directly to disk (no HTTP
endpoint exposes them — bullets, not headlines: the audit log is
append-only on disk by design and the GUI is local to the same
machine).

## Why not a web frontend?

This is the operational UI for a backend service — it lives next to
the server, doesn't need cross-origin auth, doesn't need to scale to
many clients. egui ships as a single ~25 MB binary, opens in 50 ms,
and renders at 60 fps over native OpenGL. No npm install, no service
worker, no `Electron is using significant memory` warning.

## License

MIT or Apache-2.0, your choice.

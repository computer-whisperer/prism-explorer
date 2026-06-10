# prism-explorer

A color-managed file explorer for Wayland, built with
[Damascene](https://github.com/computer-whisperer/damascene). HDR-aware
image previews via the [achromat] decode stack, text previews, and an
IO layer built for big, slow filesystems.

[achromat]: https://github.com/computer-whisperer/achromat

```
prism-explorer [DIRECTORY]    # defaults to $HOME
```

## Controls

| Key | Action |
| --- | --- |
| arrows / `hjkl` | move selection (list: ←/`h` parent, →/`l` enter dir) |
| `g` | toggle list / thumbnail grid |
| Enter / double-click | open directory · open file (`xdg-open`) |
| Backspace | parent directory |
| Home / End | first / last entry |
| `r` / F5 | refresh listing |
| `.` | toggle hidden files |

## Architecture

The primary browsing target is a large CephFS mount, which sets the one
hard rule: **no filesystem call ever runs on the UI thread.**

- **`crates/explorer-io`** — the background IO layer. A worker pool
  over a three-tier priority queue (selected-file preview > visible-row
  stat > background sweep) with generation-based cancellation:
  navigating away drops every queued job and aborts the in-flight
  streaming listing. Directory reads stream names + `d_type` in growing
  batches (first paint after ~64 entries); per-entry stat happens
  lazily, only for rows the list actually realizes.
- **`crates/explorer-previews`** — the preview-handler framework.
  `claims(path)` runs on the UI thread from the file name alone (no
  IO); `load(path)` runs on a worker. Built-ins: color-managed images
  (achromat: JPEG XR, JXL, AVIF, EXR, Radiance, PNG, JPEG, WebP — full
  CICP/ICC handling, HDR luminance anchoring), known text/code types,
  and a sniffing fallback that separates unknown text from binary with
  one bounded read.
- **`crates/explorer-thumbs`** — the on-disk thumbnail cache. Unlike
  the freedesktop spec's 8-bit sRGB PNGs, entries store linear-light
  f16 tagged with primaries and reference luminance, so an HDR
  thumbnail re-renders exactly like a fresh decode. Keyed by source
  path + mtime + size; atomic writes; corruption is a miss, not an
  error; LRU byte-budget sweep at launch. Lives on local disk
  (`~/.cache/prism-explorer/thumbs`), never the filesystem being
  browsed.
- **`crates/explorer`** — the damascene app: places sidebar (XDG dirs +
  network mounts from `/proc/mounts`), breadcrumbs, virtualized list
  and thumbnail-grid views (`virtual_list`, fine at 100k entries),
  resizable preview pane. Selection keeps stable entry ids across
  streaming resorts; preview decodes are latest-wins (holding an arrow
  key through a directory queues one decode, not fifty); grid
  thumbnails decode only for realized cells and are RAM-capped by an
  LRU however large the directory is.
- **`crates/explorer/src/host.rs`** — the explorer's own winit host
  loop: a resident multi-window process on one shared wgpu device,
  built from damascene-winit-wgpu's exposed host layers (`WindowGfx`
  per-window bring-up, the `SurfaceColor` HDR negotiation driver, the
  pure input mappers). Each window pairs a damascene `Runner` with its
  own `App`; HDR/SDR negotiates per window and re-negotiates live on
  output moves. This is what lets one warm process (thumbnail cache,
  glyph atlases, compiled shaders, D-Bus services) spin off browser
  windows and — next — portal FileChooser dialogs.

HDR output negotiates per-window (`ColorPreferences::hdr_extended`);
image previews render with full panel headroom (`NoLimit`
dynamic-range-limit, BT.2390 remastering above it). The toolbar badge
shows what the host negotiated.

## D-Bus

A running explorer serves **`org.freedesktop.FileManager1`** — the
"show this in the file manager" interface browsers and download
managers call. `ShowItems` navigates to the item's directory and
selects it; `ShowFolders` opens the folder. Install
`data/org.freedesktop.FileManager1.service` to
`~/.local/share/dbus-1/services/` to have calls launch the explorer
when it isn't running.

It also serves **`org.freedesktop.impl.portal.FileChooser`** — the
portal *backend* behind every portal-using app's open/save dialog.
Each request becomes a picker window (the full explorer page plus
accept/cancel chrome and a filename field in save mode) in the
already-warm process; `OpenFile`, `SaveFile`, and `SaveFiles` are
implemented, with per-request `Request.Close` cancellation. Install
`data/prism.portal` to `/usr/share/xdg-desktop-portal/portals/` and
prefer it in `~/.config/xdg-desktop-portal/portals.conf` (see the file
header). While the portal name is held the process stays resident
after its last window closes, ready for the next dialog.

Not yet honored: `filters`/`choices` (the picker lists everything),
`multiple` (one URI comes back), modality to the caller's window.

## Roadmap

- Portal polish: filters, multi-select, overwrite confirmation in save
  mode, a D-Bus-activatable zero-window service mode
- More preview handlers (PDF, video, audio, archives, fonts); search;
  file operations; syntax highlighting

## License

MIT or Apache-2.0, at your option.

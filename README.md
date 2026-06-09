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
| arrows / `jk` | move selection |
| Enter / double-click | open directory · open file (`xdg-open`) |
| Backspace | parent directory |
| Home / End | first / last entry |
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
- **`crates/explorer`** — the damascene app: places sidebar (XDG dirs +
  network mounts from `/proc/mounts`), breadcrumbs, virtualized list
  view (`virtual_list`, fine at 100k entries), resizable preview pane.
  Selection keeps stable entry ids across streaming resorts; preview
  decodes are latest-wins (holding an arrow key through a directory
  queues one decode, not fifty).

HDR output negotiates per-output (`ColorPreferences::hdr_extended`);
image previews render with full panel headroom (`NoLimit`
dynamic-range-limit, BT.2390 remastering above it). The toolbar badge
shows what the host negotiated.

## Roadmap

- On-disk HDR-preserving thumbnail cache (the freedesktop spec's 8-bit
  sRGB PNGs throw away exactly what this explorer is for)
- XDG portal `FileChooser` backend — explorer-quality open/save dialogs
  for every portal-using app — plus `org.freedesktop.FileManager1`
- Grid view with image thumbnails; more preview handlers (PDF, video,
  audio, archives, fonts); search; file operations

## License

MIT or Apache-2.0, at your option.

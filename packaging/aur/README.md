# Arch packaging — `prism-explorer`

> **⚠️ WORK IN PROGRESS — this package does not build yet.** achromat moved from
> a path dep to a pinned git dependency, so the two-source sibling layout
> described below is stale. Pending a decision on how to package achromat.

Source-build package for prism-explorer. It compiles the GUI binary and installs
it alongside the desktop entry (`xdg-open`/`inode/directory` integration), the
xdg-desktop-portal FileChooser declaration, and the licenses.

## The achromat coupling (read this first)

Unlike `rumble-damascene`, prism-explorer is **not** a single-source package. Its
workspace pulls the still-unpublished sibling crate `achromat` via a relative
path (`achromat = { path = "../achromat" }`) plus a `[patch.crates-io]` for
achromat's vendored `jpegxr`. So the `PKGBUILD` fetches **two** git repos and
lays them out as siblings in `$srcdir`, which is exactly the layout `../achromat`
expects:

```
$srcdir/
  prism-explorer/   # the workspace root (top Cargo.toml + [patch])
  achromat/         # ../achromat, with vendor/jpegxr inside
```

Consequences:

- **Both repos are pinned by commit** (`_commit`, `_achromat_commit`). Those
  commits must be pushed and the repos reachable for `makepkg`. Update both when
  cutting a release; once the repos carry tags, switch the sources to
  `#tag=v$pkgver`.
- **`Cargo.lock` is pinned against achromat's committed state.** If achromat has
  uncommitted changes locally, commit them and re-pin `_achromat_commit` first —
  a `--frozen` build fails if the lock and the pinned achromat tree disagree.
- The clean end-state is publishing `achromat` to crates.io, after which this
  collapses to a rumble-style single-source package (drop the second source, the
  `[patch.crates-io]`, and the sibling layout).

## What gets installed

| File | Destination |
| --- | --- |
| `prism-explorer` binary | `/usr/bin/` |
| `data/prism-explorer.desktop` | `/usr/share/applications/` |
| `data/prism.portal` | `/usr/share/xdg-desktop-portal/portals/` |
| `LICENSE-MIT`, `LICENSE-APACHE` | `/usr/share/licenses/prism-explorer/` |

**Not** installed: `data/org.freedesktop.FileManager1.service`. Its path
(`/usr/share/dbus-1/services/org.freedesktop.FileManager1.service`) collides with
any other file manager shipping the same D-Bus activation file — a hard pacman
file conflict. Install it per-user instead if you want prism activatable as the
FileManager1 owner (see the file header). A running prism claims the name
dynamically either way.

## Post-install (per-user, not done by the package)

```bash
# Make prism the directory handler for xdg-open and "open folder" buttons:
xdg-mime default prism-explorer.desktop inode/directory

# Optional — prefer prism's portal FileChooser backend:
#   ~/.config/xdg-desktop-portal/portals.conf
#   [preferred]
#   org.freedesktop.impl.portal.FileChooser=prism;gtk
```

## Build & test

```bash
cd packaging/aur
makepkg -f                       # local build
namcap PKGBUILD *.pkg.tar.zst    # lint deps / paths
makepkg --printsrcinfo > .SRCINFO
```

A clean-chroot build (`extra-x86_64-build` from devtools) is the real test — it
catches missing `makedepends` and surfaces whether the pinned commits are
actually reachable without your local checkouts.

## Notes

- **`options=('!lto')`** — mirrors rumble; distro-wide LTO injection has been a
  breakage source. Drop it if LTO builds clean.
- **Vulkan ICD** — `vulkan-icd-loader` is the hard dep; the driver
  (`vulkan-radeon` / `nvidia-utils` / `vulkan-intel`) is GPU-specific and not
  pinned.
- **`clang`** is a makedepend for `jpegxr`'s bindgen step; the vendored C codec
  links statically, so it adds no runtime dependency.
- **`depends`** is a best-effort list from the link map — re-check with `namcap`
  and `ldd target/release/prism-explorer` after a build.

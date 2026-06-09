//! Background filesystem IO for prism-explorer.
//!
//! The explorer's primary target is a large, slow CephFS mount, which
//! sets the one hard rule here: **no filesystem call ever runs on the
//! UI thread** — not `readdir`, not `stat`, not `open`. A single stat
//! against a cold Ceph MDS can stall for hundreds of milliseconds, and
//! a synchronous call in `build()` freezes the frame.
//!
//! The pieces:
//!
//! - [`Pool`] — a worker pool over a three-tier priority queue
//!   ([`Tier`]): user-blocking work (the preview for the selected
//!   file), then work for visible rows (stat, thumbnails), then the
//!   background sweep. Navigation bumps a generation counter, which
//!   drops every queued job from the previous directory so workers
//!   never burn Ceph latency on a view the user already left.
//! - [`listing`] — streaming directory reads: names and d_type kinds
//!   only (no per-entry stat), emitted in growing batches so a 50k-file
//!   directory starts rendering after the first few dozen entries.
//! - [`stat`] — per-entry metadata, fetched lazily for rows that are
//!   actually on screen.
//!
//! The pool runs plain closures and has no opinion about channels or
//! UI frameworks; callers capture their own result sender plus a
//! [`Notifier`] poke for the host event loop.

mod pool;

pub mod listing;
pub mod stat;

pub use pool::{Notifier, Pool, Tier};

use std::ffi::OsString;

/// What a directory entry is, as far as we know without following
/// symlinks. From `d_type` during the listing pass; [`stat`] resolves
/// symlink targets later.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    Dir,
    File,
    /// A symlink whose target hasn't been resolved yet (listing pass),
    /// or that turned out to be broken (stat pass).
    Symlink,
    /// FIFOs, sockets, devices.
    Other,
}

/// One directory entry as the listing pass sees it: a name and a
/// `d_type` kind. Deliberately no metadata — that's a separate, lazy,
/// per-entry stat.
#[derive(Debug, Clone)]
pub struct RawEntry {
    pub name: OsString,
    pub kind: EntryKind,
}

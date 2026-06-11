//! prism-explorer's library half — everything the `prism-explorer`
//! binary wires together, plus the canned scenes ([`fixtures`]) that
//! the `dump_bundles` artifact bin and the lint tests share.

pub mod app;
mod binary_surface;
pub mod filechooser;
pub mod filemanager1;
pub mod fixtures;
mod fmt;
pub mod host;
pub mod model;
mod picker;
pub mod places;

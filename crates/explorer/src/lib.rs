//! prism-explorer's library half — everything the `prism-explorer`
//! binary wires together, plus the canned scenes ([`fixtures`]) that
//! the `dump_bundles` artifact bin and the lint tests share.

pub mod app;
pub mod apps;
mod binary_surface;
pub mod browser;
pub mod filechooser;
pub mod filemanager1;
pub mod instance;
pub mod fixtures;
mod fmt;
pub mod host;
pub mod model;
pub mod ops;
mod picker;
pub mod places;
mod preview_policy;
pub mod settings;
pub mod state;
mod sysclip;

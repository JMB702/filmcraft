//! Review links: Jeff shares a rendered sequence as a private link, reviewers watch it on a phone and
//! leave comments at exact frames, and each comment becomes a marker on the source sequence.
//!
//! - [`model`]: the share record and the comment events, shared with the wasm review page.
//! - [`timing`]: the exact mapping from a frame of the rendered file to sequence ticks and timecode.
//! - [`text`]: limits and clean-up for reviewer text, and HTML escaping.
//! - [`token`]: link tokens, ids and device secrets (random generation needs Unix; the device hash
//!   needs feature `server`).
//! - [`store`] (feature `server`): the files under `<data dir>/Review/` (shares, the append-only
//!   comment log, the app's ledger of applied comments).
//! - [`server`] (feature `server`): the HTTP server reviewers reach. It runs in its own process
//!   (`filmcraft --review-server`) so comments are taken while FilmCraft is closed.
//! - [`tunnel`] (feature `server`): read-only Tailscale Funnel status, for links that reach
//!   reviewers off the local network.
//!
//! The server never touches a project or the live control channel: it only appends to the comment
//! log. The app reads the log and makes the markers (`review.sync` in the engine).

#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable))]

pub mod model;
pub mod text;
pub mod timing;
pub mod token;

#[cfg(feature = "server")]
pub mod server;
#[cfg(feature = "server")]
pub mod store;
#[cfg(feature = "server")]
pub mod tunnel;

pub use model::{Comment, CommentEvent, EventKind, Rendition, ShareInfo, ShareRecord};

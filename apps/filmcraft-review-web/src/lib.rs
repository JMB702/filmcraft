//! The review-link page's behaviour, in Rust compiled to wasm (no handwritten JavaScript).
//!
//! The server renders a page that works on its own: watch, read, and post with a typed timecode.
//! This module adds what needs a script:
//! - the sequence's own timecode under the video, and one-frame steps for an exact-frame comment;
//! - a scrub bar with a tick per comment; tapping a comment seeks to it;
//! - live comments from other reviewers (server-sent events);
//! - the name asked once and remembered on the device, and a device secret so a reviewer can edit
//!   or delete their own comments;
//! - the file the screen and the connection can carry, switchable, and a lighter one by itself
//!   when playback keeps freezing; retries after a network error; comments written with no
//!   connection kept and sent when it returns;
//! - a thumb-sized play row, the scrub bar and full screen under the video, so nothing covers the
//!   picture;
//! - a page that fits the visible screen: the video stays at the top, only the comments scroll,
//!   and the composer sits right above the on-screen keyboard.
//!
//! [`logic`] holds the decisions and builds everywhere (its tests run natively); the DOM glue
//! (`dom`) is wasm32-only. Build with `cargo xtask review-web`.

#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable))]

pub mod logic;

#[cfg(target_arch = "wasm32")]
mod dom;

//! Headless playback benchmark; see `bench/playback.rs` (`cargo xtask bench-playback`).

#![allow(dead_code)]

#[path = "bench/fixtures.rs"]
mod fixtures;
#[path = "bench/layers.rs"]
mod layers;
#[path = "bench/playback.rs"]
mod playback;

fn main() {
    playback::cli_main();
}

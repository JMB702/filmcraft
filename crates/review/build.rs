//! Builds in the review page's wasm module when `FILMCRAFT_REVIEW_WEB_DIR` names the folder
//! `cargo xtask review-web` wrote (an optional build input, like `CRAFT_FONTS_DIR`). Unset or
//! empty, the server serves the page without the module: it still plays the video and takes
//! comments with a typed timecode.

use std::path::Path;

/// The files `cargo xtask review-web` writes, in the order the page loads them.
const FILES: [&str; 3] = ["boot.js", "review_web.js", "review_web_bg.wasm"];

fn main() {
    println!("cargo:rerun-if-env-changed=FILMCRAFT_REVIEW_WEB_DIR");
    let Ok(out) = std::env::var("OUT_DIR") else {
        println!("cargo:warning=OUT_DIR is not set");
        return;
    };
    let mut code =
        String::from("/// The review page's wasm module and its loader, built in from `FILMCRAFT_REVIEW_WEB_DIR`.\npub static WEB: &[(&str, &[u8])] = &[\n");
    if let Some(dir) = std::env::var_os("FILMCRAFT_REVIEW_WEB_DIR").filter(|d| !d.is_empty()) {
        let dir = Path::new(&dir);
        let found: Vec<_> = FILES.iter().map(|n| (n, dir.join(n))).filter(|(_, p)| p.is_file()).collect();
        if found.len() == FILES.len() {
            for (name, p) in found {
                println!("cargo:rerun-if-changed={}", p.display());
                let abs = p.canonicalize().unwrap_or(p);
                code.push_str(&format!("    ({name:?}, include_bytes!({:?})),\n", abs.display().to_string()));
            }
        } else {
            println!("cargo:warning=FILMCRAFT_REVIEW_WEB_DIR={} lacks some of {FILES:?}; the review page is built without its wasm module", dir.display());
        }
    }
    code.push_str("];\n");
    if let Err(e) = std::fs::write(Path::new(&out).join("web_assets.rs"), code) {
        println!("cargo:warning=could not write web_assets.rs: {e}");
    }
}

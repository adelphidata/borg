use std::path::Path;

/// Prepares `apps/web/dist` for the `rust_embed` derive in `webui::assets`.
///
/// Two things the derive cannot do for itself:
///
/// 1. **The folder must exist.** `rust_embed` fails to compile without it, and
///    the folder is a build artifact — absent on a clean checkout, and wiped by
///    every `vite build` (`emptyOutDir`), which is why a committed `.gitkeep`
///    does not survive. Creating it here runs before the macro expands, so
///    `cargo build` works with or without the SPA; an empty one serves
///    `assets::not_bundled`.
/// 2. **Changes must invalidate the build.** `rust_embed` bakes the files in at
///    compile time but does not register them as build inputs, so a rebuilt SPA
///    would otherwise leave cargo believing it had nothing to do — and ship the
///    previous UI. Walking the directory emits the missing dependencies.
fn main() {
    let dist = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../apps/web/dist");
    let _ = std::fs::create_dir_all(&dist);
    println!("cargo:rerun-if-changed={}", dist.display());
    watch(&dist);
}

fn watch(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return; // No dist yet — assets::not_bundled() covers that case.
    };
    for entry in entries.flatten() {
        let path = entry.path();
        println!("cargo:rerun-if-changed={}", path.display());
        if path.is_dir() {
            watch(&path);
        }
    }
}

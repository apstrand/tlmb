// The list lives with the code that grants and calls these commands; see
// `src/commands.rs` for why it is included rather than imported.
include!("src/commands.rs");

fn main() {
    println!("cargo:rerun-if-changed=src/commands.rs");
    // Declaring the app's commands is what lets a capability name them. The
    // window's page is loopback-served, so without one Tauri refuses them all.
    tauri_build::try_build(
        tauri_build::Attributes::new()
            .app_manifest(tauri_build::AppManifest::new().commands(&COMMANDS)),
    )
    .expect("tauri-build failed");
}

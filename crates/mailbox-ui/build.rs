fn main() {
    // Declaring the app's commands is what lets a capability name them. The
    // window's page is loopback-served, so without one Tauri refuses them all.
    tauri_build::try_build(tauri_build::Attributes::new().app_manifest(
        tauri_build::AppManifest::new().commands(&[
            "update_status",
            "update_check",
            "update_install",
            "update_restart",
        ]),
    ))
    .expect("tauri-build failed");
}

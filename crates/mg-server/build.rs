fn main() {
    // UI edits must invalidate the Tauri asset bundle in Cargo builds too.
    println!("cargo:rerun-if-changed=ui");

    tauri_build::try_build(tauri_build::Attributes::new())
        .expect("failed to build Tauri desktop resources");
}

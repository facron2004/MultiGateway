fn main() {
    // rust-embed reads ui/ at compile time, but cargo does not know those files are
    // inputs; without this a UI-only change silently rebuilds with stale assets.
    println!("cargo:rerun-if-changed=ui");

    // cfg(windows) here would follow the host. Cross-compiling a macOS binary from
    // Windows must not try to attach a Win32 resource.
    let target = std::env::var("TARGET").unwrap_or_default();
    if !target.contains("windows") {
        return;
    }
    let mut res = winres::WindowsResource::new();
    res.set_icon("ui/favicon.ico");
    res.set("FileDescription", "MultiGateway - Multi-Account AI Reverse Proxy Gateway");
    res.set("ProductName", "MultiGateway");
    res.set("OriginalFilename", "MultiGateway.exe");
    res.set("LegalCopyright", "Copyright (c) 2026 MultiGateway Authors");
    let _ = res.compile();
}

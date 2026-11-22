//! Embeds the app icon in the Windows executable (Explorer, shortcuts, the
//! taskbar before the window sets its own). No-op on other targets.
//! Regenerate the icon with `scripts/make-windows-icon.py`.

fn main() {
    println!("cargo:rerun-if-changed=assets/windows/termy.rc");
    println!("cargo:rerun-if-changed=assets/windows/termy.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        embed_resource::compile("assets/windows/termy.rc", embed_resource::NONE)
            .manifest_optional()
            .expect("embedding the Windows icon (needs rc.exe from the Windows SDK, or windres)");
    }
}

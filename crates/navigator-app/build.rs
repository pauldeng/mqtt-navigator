//! Windows: embeds the app icon and the version information in `mqtt-navigator.exe`.
//!
//! gpui loads the window and taskbar icon from the icon resource with ID 1 of the exe
//! (`LoadImageW` in gpui-pre-windows). Explorer and the shortcuts use the same icon.
//! Other platforms: this script does nothing.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=../../assets/icon/icon.ico");
    #[cfg(windows)]
    {
        let mut res = winresource::WindowsResource::new();
        res.set_icon_with_id("../../assets/icon/icon.ico", "1")
            .set("ProductName", "MQTT Navigator")
            .set("FileDescription", "MQTT Navigator")
            .set("OriginalFilename", "mqtt-navigator.exe")
            .set(
                "LegalCopyright",
                "Copyright (c) MQTT Navigator contributors",
            );
        res.compile_for("mqtt-navigator").expect(
            "cannot embed the Windows resources (needs the Windows SDK resource compiler, rc.exe)",
        );
    }
}

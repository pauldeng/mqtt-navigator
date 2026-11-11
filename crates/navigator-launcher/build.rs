//! Windows: embeds the app icon and the version information in `mqtt-navigator-launcher.exe`.
//! The shortcuts and the installer start the launcher, so Explorer shows its icon.
//! Other platforms: this script does nothing.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=../../assets/icon/icon.ico");
    #[cfg(windows)]
    {
        let mut res = winresource::WindowsResource::new();
        res.set_icon_with_id("../../assets/icon/icon.ico", "1")
            .set("ProductName", "MQTT Navigator")
            .set("FileDescription", "MQTT Navigator launcher")
            .set("OriginalFilename", "mqtt-navigator-launcher.exe")
            .set(
                "LegalCopyright",
                "Copyright (c) MQTT Navigator contributors",
            );
        res.compile_for("mqtt-navigator-launcher").expect(
            "cannot embed the Windows resources (needs the Windows SDK resource compiler, rc.exe)",
        );
    }
}

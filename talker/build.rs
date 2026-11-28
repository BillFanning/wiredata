//! Embeds a Windows version resource in each talker program, so its
//! Properties → Details shows a name and version (listener ADR-080), and its
//! icon, which Explorer, the taskbar and shortcuts show (listener ADR-085).
//! Nothing is done for any other target.

/// Each program, the name Windows shows for it, and its icon.
const PROGRAMS: &[(&str, &str, &str)] = &[
    ("talker", "Talker", "../assets/icons/talker.ico"),
    ("talker-gui", "Talker", "../assets/icons/talker.ico"),
];

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    for &(binary, name, icon) in PROGRAMS {
        println!("cargo:rerun-if-changed={icon}");
        let mut resource = winresource::WindowsResource::new();
        resource
            .set("ProductName", name)
            .set("FileDescription", name)
            .set("InternalName", binary)
            .set("OriginalFilename", &format!("{binary}.exe"))
            .set_icon(icon);
        if let Err(error) = resource.compile_for(binary) {
            panic!("could not embed the version resource in {binary}.exe: {error}");
        }
    }
}

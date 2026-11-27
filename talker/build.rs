//! Embeds a Windows version resource in each talker program, so its
//! Properties → Details shows a name and version (listener ADR-080). Nothing
//! is done for any other target.

/// Each program, and the name Windows shows for it.
const PROGRAMS: &[(&str, &str)] = &[("talker", "Talker"), ("talker-gui", "Talker")];

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    for &(binary, name) in PROGRAMS {
        let mut resource = winresource::WindowsResource::new();
        resource
            .set("ProductName", name)
            .set("FileDescription", name)
            .set("InternalName", binary)
            .set("OriginalFilename", &format!("{binary}.exe"));
        if let Err(error) = resource.compile_for(binary) {
            panic!("could not embed the version resource in {binary}.exe: {error}");
        }
    }
}

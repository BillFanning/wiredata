//! Each Windows program carries a version resource (listener ADR-080). The
//! test finds the resource section in the .exe and checks that the strings
//! Properties → Details reads are in it, as UTF-16. It does not check how
//! Windows lays them out, which is the resource compiler's to get right.
//! Each also carries its own icon (listener ADR-085): the test looks for the
//! icon file's 256 px image among the resources, which shows the right icon
//! was embedded, not how Windows picks a size from it.
#![cfg(windows)]

/// The `.rsrc` section of a PE file, found through its section table.
fn resource_section(exe: &[u8]) -> &[u8] {
    let word = |at: usize| u16::from_le_bytes([exe[at], exe[at + 1]]) as usize;
    let dword = |at: usize| u32::from_le_bytes(exe[at..at + 4].try_into().unwrap()) as usize;
    let pe = dword(0x3C);
    assert_eq!(&exe[pe..pe + 4], b"PE\0\0", "not a PE file");
    let sections = word(pe + 6);
    let table = pe + 24 + word(pe + 20);
    (0..sections)
        .map(|i| table + i * 40)
        .find(|&header| exe[header..header + 8].starts_with(b".rsrc"))
        .map(|header| {
            let (size, start) = (dword(header + 16), dword(header + 20));
            &exe[start..start + size]
        })
        .expect("the .exe has a resource section")
}

fn utf16(text: &str) -> Vec<u8> {
    text.encode_utf16().flat_map(u16::to_le_bytes).collect()
}

/// The 256 px image in an `.ico` file: the entry whose width byte is 0.
fn largest_icon_image(ico: &[u8]) -> &[u8] {
    let word = |at: usize| u16::from_le_bytes([ico[at], ico[at + 1]]) as usize;
    let dword = |at: usize| u32::from_le_bytes(ico[at..at + 4].try_into().unwrap()) as usize;
    (0..word(4))
        .map(|i| 6 + i * 16)
        .find(|&entry| ico[entry] == 0)
        .map(|entry| {
            let (size, start) = (dword(entry + 8), dword(entry + 12));
            &ico[start..start + size]
        })
        .expect("the .ico has a 256 px image")
}

#[test]
fn each_program_carries_its_own_icon() {
    for (exe, icon_path) in [
        (
            env!("CARGO_BIN_EXE_talker"),
            concat!(env!("CARGO_MANIFEST_DIR"), "/../assets/icons/talker.ico"),
        ),
        (
            env!("CARGO_BIN_EXE_talker-gui"),
            concat!(env!("CARGO_MANIFEST_DIR"), "/../assets/icons/talker.ico"),
        ),
    ] {
        let program = std::fs::read(exe).unwrap();
        let icon = std::fs::read(icon_path).unwrap();
        let image = largest_icon_image(&icon);
        assert!(
            resource_section(&program)
                .windows(image.len())
                .any(|w| w == image),
            "{exe} does not carry the 256 px image in {icon_path}"
        );
    }
}

#[test]
fn each_program_carries_its_name_and_version() {
    for (exe, binary, name) in [
        (env!("CARGO_BIN_EXE_talker"), "talker", "Talker"),
        (env!("CARGO_BIN_EXE_talker-gui"), "talker-gui", "Talker"),
    ] {
        let bytes = std::fs::read(exe).unwrap();
        let resource = resource_section(&bytes);
        for text in [
            "VS_VERSION_INFO",
            env!("CARGO_PKG_VERSION"),
            name,
            &format!("{binary}.exe"),
        ] {
            let wanted = utf16(text);
            assert!(
                resource.windows(wanted.len()).any(|w| w == wanted),
                "{binary}.exe has no {text:?} in its version resource"
            );
        }
    }
}

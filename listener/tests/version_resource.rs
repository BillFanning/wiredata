//! Each Windows program carries a version resource (listener ADR-080). The
//! test finds the resource section in the .exe and checks that the strings
//! Properties → Details reads are in it, as UTF-16. It does not check how
//! Windows lays them out, which is the resource compiler's to get right.
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

#[test]
fn each_program_carries_its_name_and_version() {
    for (exe, binary, name) in [
        (env!("CARGO_BIN_EXE_listener"), "listener", "Listener"),
        (
            env!("CARGO_BIN_EXE_listener-gui"),
            "listener-gui",
            "Listener",
        ),
        (env!("CARGO_BIN_EXE_linemon"), "linemon", "Linemon"),
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

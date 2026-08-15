//! Release-asset integrity checks. These deliberately inspect the real checked-in
//! files so a one-pixel or duplicated placeholder cannot satisfy the build gate.

#[test]
fn wallpapers_are_distinct_real_sized_pngs() {
    let wallpapers: [&[u8]; 3] = [
        include_bytes!("../../kernel/assets/usr/share/wallpapers/blue.png"),
        include_bytes!("../../kernel/assets/usr/share/wallpapers/green.png"),
        include_bytes!("../../kernel/assets/usr/share/wallpapers/dark.png"),
    ];
    for bytes in wallpapers {
        assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n");
        let width = u32::from_be_bytes(bytes[16..20].try_into().unwrap());
        let height = u32::from_be_bytes(bytes[20..24].try_into().unwrap());
        assert!(
            width >= 800 && height >= 600,
            "wallpaper is only {width}x{height}"
        );
    }
    assert_ne!(wallpapers[0], wallpapers[1]);
    assert_ne!(wallpapers[0], wallpapers[2]);
    assert_ne!(wallpapers[1], wallpapers[2]);
}

#[test]
fn terminal_fonts_are_nonblank_distinct_atlases() {
    let compact = include_bytes!("../../term/assets/fonts/unifont-mono-14.pbm");
    let vga = include_bytes!("../../term/assets/fonts/pc-vga-16.pbm");
    for bytes in [compact.as_slice(), vga.as_slice()] {
        let text = std::str::from_utf8(bytes).expect("PBM must be ASCII");
        assert!(text.starts_with("P1\n"));
        assert!(text.contains("256 codepoint slots"));
        assert!(text.bytes().filter(|byte| *byte == b'1').count() > 500);
    }
    assert_ne!(compact.as_slice(), vga.as_slice());
}

#[test]
fn timezone_assets_are_real_and_distinct_tzif_payloads() {
    let zones: [&[u8]; 4] = [
        include_bytes!("../../kernel/assets/etc/zoneinfo/UTC"),
        include_bytes!("../../kernel/assets/etc/zoneinfo/America_New_York"),
        include_bytes!("../../kernel/assets/etc/zoneinfo/Europe_London"),
        include_bytes!("../../kernel/assets/etc/zoneinfo/Asia_Tokyo"),
    ];
    for bytes in zones {
        assert!(bytes.len() >= 100, "zoneinfo payload is a placeholder");
        assert_eq!(&bytes[..4], b"TZif");
    }
    for left in 0..zones.len() {
        for right in left + 1..zones.len() {
            assert_ne!(zones[left], zones[right]);
        }
    }
}

#[test]
fn keymap_assets_are_distinct_pmkm_payloads() {
    let maps: [&[u8]; 3] = [
        include_bytes!("../../display-server/assets/keymaps/us-qwerty.bin"),
        include_bytes!("../../display-server/assets/keymaps/uk-qwerty.bin"),
        include_bytes!("../../display-server/assets/keymaps/dvorak.bin"),
    ];
    for bytes in maps {
        assert_eq!(&bytes[..4], b"PMKM");
        assert_eq!(u16::from_le_bytes(bytes[4..6].try_into().unwrap()), 1);
        assert!(u16::from_le_bytes(bytes[6..8].try_into().unwrap()) >= 50);
    }
    assert_ne!(maps[0], maps[1]);
    assert_ne!(maps[0], maps[2]);
    assert_ne!(maps[1], maps[2]);
}

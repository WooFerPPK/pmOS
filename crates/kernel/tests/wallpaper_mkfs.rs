#![cfg(feature = "native-platform")]

use kernel::fs::opfs::block::MockBlockDevice;
use kernel::fs::opfs::mkfs::{
    default_blue_wallpaper, default_dark_wallpaper, default_green_wallpaper, mkfs,
};
use kernel::vfs::Filesystem;

#[test]
fn fresh_mkfs_installs_complete_wallpaper_bundle() {
    let mut fs = mkfs(Box::new(MockBlockDevice::new(4096))).expect("fresh wallpaper-capable mkfs");
    let root = fs.root();
    let usr = fs.lookup(root, "usr").unwrap();
    let share = fs.lookup(usr, "share").unwrap();
    let wallpapers = fs.lookup(share, "wallpapers").unwrap();

    for (name, expected) in [
        ("blue.png", default_blue_wallpaper()),
        ("green.png", default_green_wallpaper()),
        ("dark.png", default_dark_wallpaper()),
    ] {
        let ino = fs.lookup(wallpapers, name).unwrap();
        let stat = fs.stat(ino).unwrap();
        assert_eq!(stat.mode & 0o777, 0o644);
        assert_eq!(stat.size, expected.len() as u64);
        let mut actual = vec![0; expected.len()];
        let read = fs.read(ino, 0, &mut actual).unwrap();
        assert_eq!(read, expected.len());
        assert_eq!(actual, expected);
    }
}

#![cfg(feature = "native-platform")]

use kernel::fs::opfs::block::MockBlockDevice;
use kernel::fs::opfs::mkfs::{
    default_zoneinfo_america_new_york, default_zoneinfo_asia_tokyo, default_zoneinfo_europe_london,
    default_zoneinfo_utc, mkfs,
};
use kernel::vfs::{Filesystem, NodeType};

#[test]
fn fresh_mkfs_installs_exact_bundled_zoneinfo_payloads() {
    let mut fs = mkfs(Box::new(MockBlockDevice::new(4096))).expect("fresh mkfs");
    let root = fs.root();
    let etc = fs.lookup(root, "etc").unwrap();
    let zoneinfo = fs.lookup(etc, "zoneinfo").unwrap();
    assert_eq!(fs.stat(zoneinfo).unwrap().ty, NodeType::Directory);

    for (name, expected) in [
        ("UTC", default_zoneinfo_utc()),
        ("America_New_York", default_zoneinfo_america_new_york()),
        ("Europe_London", default_zoneinfo_europe_london()),
        ("Asia_Tokyo", default_zoneinfo_asia_tokyo()),
    ] {
        let ino = fs.lookup(zoneinfo, name).unwrap();
        let stat = fs.stat(ino).unwrap();
        assert_eq!(stat.ty, NodeType::RegularFile);
        assert_eq!(stat.mode, 0o644);
        assert_eq!(stat.size as usize, expected.len());
        let mut actual = vec![0; expected.len()];
        let read = fs.read(ino, 0, &mut actual).unwrap();
        assert_eq!(read, expected.len());
        assert_eq!(actual, expected);
        assert_eq!(&actual[..4], b"TZif");
    }

    let superblock = fs.superblock();
    assert_eq!(
        superblock.data_block_count - superblock.data_block_free,
        1_113,
        "zoneinfo adds one directory block and four data blocks"
    );
}

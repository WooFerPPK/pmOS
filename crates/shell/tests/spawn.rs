use sh::SpawnWireManifest;
use shell::{encode_with_spawn_timezone, PreferenceSource};
use std::collections::{BTreeMap, VecDeque};
use std::io;

struct SequenceSource {
    snapshots: VecDeque<io::Result<Option<Vec<u8>>>>,
    reads: usize,
}

impl PreferenceSource for SequenceSource {
    fn read(&mut self) -> io::Result<Option<Vec<u8>>> {
        self.reads += 1;
        self.snapshots.pop_front().unwrap_or(Ok(None))
    }
}

fn read_u16(blob: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes(blob[offset..offset + 2].try_into().unwrap())
}

fn take_text(blob: &[u8], offset: &mut usize, length: usize) -> String {
    let value = String::from_utf8(blob[*offset..*offset + length].to_vec()).unwrap();
    *offset += length;
    value
}

fn decode_argv_env(blob: &[u8]) -> (Vec<String>, BTreeMap<String, String>) {
    let path_len = read_u16(blob, 12) as usize;
    let cwd_len = read_u16(blob, 14) as usize;
    let argc = read_u16(blob, 16) as usize;
    let envc = read_u16(blob, 18) as usize;
    let mut offset = abi::ext::spawn_v1::HEADER_LEN + path_len + cwd_len;
    let mut argv = Vec::new();
    for _ in 0..argc {
        let length = read_u16(blob, offset) as usize;
        offset += 2;
        argv.push(take_text(blob, &mut offset, length));
    }
    let mut environment = BTreeMap::new();
    for _ in 0..envc {
        let key_len = read_u16(blob, offset) as usize;
        let value_len = read_u16(blob, offset + 2) as usize;
        offset += 4;
        let key = take_text(blob, &mut offset, key_len);
        let value = take_text(blob, &mut offset, value_len);
        environment.insert(key, value);
    }
    (argv, environment)
}

#[test]
fn launcher_spawn_preserves_fields_and_overrides_only_timezone() {
    let mut source = SequenceSource {
        snapshots: [Ok(Some(
            b"[timezone]\niana = \"America/New_York\"\n".to_vec(),
        ))]
        .into(),
        reads: 0,
    };
    let argv = vec!["settings".to_string(), "about".to_string()];
    let environment = vec![
        ("HOME".to_string(), "/home/user".to_string()),
        ("TZ".to_string(), "Europe/London".to_string()),
    ];
    let blob = encode_with_spawn_timezone(
        &mut source,
        &SpawnWireManifest {
            path: "/bin/settings",
            argv: &argv,
            env: &environment,
            stdin_fd: Some(10),
            stdout_fd: Some(11),
            stderr_fd: Some(12),
            extra_fds: &[(13, 5)],
            cwd: Some("/home/user"),
            caps: Some(abi::cap::initial::SETTINGS.0),
        },
    )
    .unwrap();
    assert_eq!(source.reads, 1);
    assert_eq!(read_u16(&blob, 16), 2);
    assert_eq!(i32::from_le_bytes(blob[24..28].try_into().unwrap()), 10);
    assert_eq!(i32::from_le_bytes(blob[28..32].try_into().unwrap()), 11);
    assert_eq!(i32::from_le_bytes(blob[32..36].try_into().unwrap()), 12);
    assert_eq!(
        u64::from_le_bytes(blob[40..48].try_into().unwrap()),
        abi::cap::initial::SETTINGS.0
    );
    assert!(blob.ends_with(&[13, 0, 0, 0, 5, 0, 0, 0]));
    let (decoded_argv, decoded_env) = decode_argv_env(&blob);
    assert_eq!(decoded_argv, argv);
    assert_eq!(
        decoded_env.get("HOME").map(String::as_str),
        Some("/home/user")
    );
    assert_eq!(
        decoded_env.get("TZ").map(String::as_str),
        Some("America/New_York")
    );
}

#[test]
fn successive_launcher_spawns_read_latest_timezone_and_default_safely() {
    let mut source = SequenceSource {
        snapshots: [
            Ok(Some(b"[timezone]\niana = \"Asia/Tokyo\"\n".to_vec())),
            Ok(Some(b"malformed".to_vec())),
            Ok(Some(b"[timezone]\niana = \"Europe/Paris\"\n".to_vec())),
            Ok(None),
        ]
        .into(),
        reads: 0,
    };
    let argv = Vec::new();
    let environment = Vec::new();
    let manifest = SpawnWireManifest {
        path: "/bin/term",
        argv: &argv,
        env: &environment,
        stdin_fd: None,
        stdout_fd: None,
        stderr_fd: None,
        extra_fds: &[],
        cwd: None,
        caps: Some(abi::cap::initial::ORDINARY_APP.0),
    };
    let expected = ["Asia/Tokyo", "UTC", "UTC", "UTC"];
    for timezone in expected {
        let blob = encode_with_spawn_timezone(&mut source, &manifest).unwrap();
        let (_, decoded_env) = decode_argv_env(&blob);
        assert_eq!(decoded_env.get("TZ").map(String::as_str), Some(timezone));
    }
    assert_eq!(source.reads, expected.len());
}

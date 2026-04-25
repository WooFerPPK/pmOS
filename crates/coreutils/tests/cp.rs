//! Integration tests for the `cp` coreutil binary.
//!
//! Driven through `std::process::Command` so the tests see the exact
//! bytes the userland binary emits. Temp files are placed under
//! `std::env::temp_dir()` with a per-test directory keyed by the
//! test name and process id; each test cleans up its directory on
//! success (a failing test leaves its scratch tree intact for
//! debugging).

use std::env;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

const CP: &str = env!("CARGO_BIN_EXE_cp");

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn scratch_dir(tag: &str) -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = env::temp_dir().join(format!(
        "pmos-cp-{}-{}-{}",
        tag,
        std::process::id(),
        n
    ));
    fs::create_dir_all(&dir).expect("create scratch dir");
    dir
}

fn write_file(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
    let path = dir.join(name);
    let mut f = fs::File::create(&path).expect("create temp file");
    f.write_all(bytes).expect("write temp file");
    path
}

fn cleanup(dir: &Path) {
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn copies_file_content_verbatim() {
    let dir = scratch_dir("verbatim");
    let mut payload: Vec<u8> = Vec::with_capacity(128);
    payload.extend_from_slice(b"the quick brown fox");
    payload.push(0x00);
    payload.extend_from_slice(b" jumps over ");
    payload.push(0xC3);
    payload.push(0xA9);
    payload.extend_from_slice(b" the lazy dog\n");
    while payload.len() < 100 {
        payload.push(b'.');
    }
    let src = write_file(&dir, "src.bin", &payload);
    let dst = dir.join("dst.bin");

    let out = Command::new(CP)
        .arg(&src)
        .arg(&dst)
        .output()
        .expect("spawn cp");

    assert!(out.status.success(), "exit status: {:?}", out.status);
    assert!(out.stdout.is_empty(), "stdout = {:?}", out.stdout);
    assert!(out.stderr.is_empty(), "stderr = {:?}", out.stderr);
    let copied = fs::read(&dst).expect("read dst");
    assert_eq!(copied, payload);
    cleanup(&dir);
}

#[test]
fn overwrites_existing_dst() {
    let dir = scratch_dir("overwrite");
    let src = write_file(&dir, "src.txt", b"new content");
    let dst = write_file(&dir, "dst.txt", b"old content");

    let out = Command::new(CP)
        .arg(&src)
        .arg(&dst)
        .output()
        .expect("spawn cp");

    assert!(out.status.success(), "exit status: {:?}", out.status);
    assert!(out.stderr.is_empty(), "stderr = {:?}", out.stderr);
    let copied = fs::read(&dst).expect("read dst");
    assert_eq!(copied, b"new content");
    cleanup(&dir);
}

#[test]
fn missing_src_exits_one_and_stderr_has_path() {
    let dir = scratch_dir("missing");
    let missing = dir.join("does-not-exist.txt");
    let dst = dir.join("dst.txt");

    let out = Command::new(CP)
        .arg(&missing)
        .arg(&dst)
        .output()
        .expect("spawn cp");

    assert_eq!(out.status.code(), Some(1), "exit status: {:?}", out.status);
    assert!(out.stdout.is_empty(), "stdout = {:?}", out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("cp:"), "stderr = {stderr:?}");
    assert!(
        stderr.contains(missing.to_str().unwrap()),
        "stderr = {stderr:?}"
    );
    assert!(!dst.exists(), "dst should not have been created");
    cleanup(&dir);
}

#[test]
fn wrong_arg_count_exits_one_with_usage() {
    let out = Command::new(CP).output().expect("spawn cp");
    assert_eq!(out.status.code(), Some(1), "exit status: {:?}", out.status);
    assert!(out.stdout.is_empty(), "stdout = {:?}", out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("usage"), "stderr = {stderr:?}");
    assert!(stderr.contains("cp <src> <dst>"), "stderr = {stderr:?}");

    let out_one = Command::new(CP).arg("only-one").output().expect("spawn cp");
    assert_eq!(
        out_one.status.code(),
        Some(1),
        "exit status: {:?}",
        out_one.status
    );
    let stderr_one = String::from_utf8_lossy(&out_one.stderr);
    assert!(stderr_one.contains("usage"), "stderr = {stderr_one:?}");
    assert!(
        stderr_one.contains("cp <src> <dst>"),
        "stderr = {stderr_one:?}"
    );
}

#[test]
fn dash_r_copies_empty_directory() {
    let dir = scratch_dir("dash-r-empty");
    let src = dir.join("src");
    fs::create_dir(&src).expect("create src");
    let dst = dir.join("dst");

    let out = Command::new(CP)
        .arg("-r")
        .arg(&src)
        .arg(&dst)
        .output()
        .expect("spawn cp");

    assert!(out.status.success(), "exit status: {:?}", out.status);
    assert!(out.stderr.is_empty(), "stderr = {:?}", out.stderr);
    let dst_meta = fs::symlink_metadata(&dst).expect("stat dst");
    assert!(dst_meta.is_dir(), "dst should be a directory");
    let entries: Vec<_> = fs::read_dir(&dst)
        .expect("read dst")
        .collect::<Result<Vec<_>, _>>()
        .expect("collect entries");
    assert!(entries.is_empty(), "dst should be empty");
    cleanup(&dir);
}

#[test]
fn dash_r_copies_nested_tree() {
    let dir = scratch_dir("dash-r-nested");
    let src = dir.join("src");
    let sub = src.join("sub");
    let nested = sub.join("nested");
    fs::create_dir_all(&nested).expect("create nested tree");
    let top_bytes: &[u8] = b"top-level\n";
    let sub_bytes: &[u8] = b"\x00\x01\x02 mid\n";
    let nested_bytes: &[u8] = b"deep payload";
    write_file(&src, "top.txt", top_bytes);
    write_file(&sub, "mid.txt", sub_bytes);
    write_file(&nested, "deep.bin", nested_bytes);

    let dst = dir.join("dst");
    let out = Command::new(CP)
        .arg("-R")
        .arg(&src)
        .arg(&dst)
        .output()
        .expect("spawn cp");

    assert!(out.status.success(), "exit status: {:?}", out.status);
    assert!(out.stderr.is_empty(), "stderr = {:?}", out.stderr);
    assert_eq!(fs::read(dst.join("top.txt")).expect("read top"), top_bytes);
    assert_eq!(
        fs::read(dst.join("sub").join("mid.txt")).expect("read mid"),
        sub_bytes
    );
    assert_eq!(
        fs::read(dst.join("sub").join("nested").join("deep.bin")).expect("read deep"),
        nested_bytes
    );
    cleanup(&dir);
}

#[test]
fn dash_r_into_existing_dst_dir() {
    let dir = scratch_dir("dash-r-into-existing");
    let src = dir.join("src");
    fs::create_dir(&src).expect("create src");
    write_file(&src, "child.txt", b"hello");
    let dst = dir.join("dst");
    fs::create_dir(&dst).expect("create dst");

    let out = Command::new(CP)
        .arg("-r")
        .arg(&src)
        .arg(&dst)
        .output()
        .expect("spawn cp");

    assert!(out.status.success(), "exit status: {:?}", out.status);
    assert!(out.stderr.is_empty(), "stderr = {:?}", out.stderr);
    let landed = dst.join("src");
    assert!(landed.is_dir(), "expected dst/src/ to exist as a dir");
    assert_eq!(
        fs::read(landed.join("child.txt")).expect("read child"),
        b"hello"
    );
    cleanup(&dir);
}

#[test]
fn dash_r_clobbering_files_inside_dst_works() {
    let dir = scratch_dir("dash-r-clobber");
    let src = dir.join("src");
    fs::create_dir(&src).expect("create src");
    write_file(&src, "f.txt", b"new content");
    let dst = dir.join("dst");
    fs::create_dir(&dst).expect("create dst");
    write_file(&dst, "f.txt", b"old content");

    let out = Command::new(CP)
        .arg("-r")
        .arg(&src)
        .arg(&dst)
        .output()
        .expect("spawn cp");

    assert!(out.status.success(), "exit status: {:?}", out.status);
    assert!(out.stderr.is_empty(), "stderr = {:?}", out.stderr);
    assert_eq!(
        fs::read(dst.join("src").join("f.txt")).expect("read landed"),
        b"new content"
    );

    let out2 = Command::new(CP)
        .arg("-r")
        .arg(&src)
        .arg(&dst)
        .output()
        .expect("spawn cp second time");
    assert!(out2.status.success(), "exit status: {:?}", out2.status);
    assert!(out2.stderr.is_empty(), "stderr = {:?}", out2.stderr);
    assert_eq!(
        fs::read(dst.join("src").join("f.txt")).expect("re-read landed"),
        b"new content"
    );
    cleanup(&dir);
}

#[test]
fn directory_src_without_dash_r_errors_as_before() {
    let dir = scratch_dir("dir-no-flag");
    let src = dir.join("src");
    fs::create_dir(&src).expect("create src");
    let dst = dir.join("dst");

    let out = Command::new(CP)
        .arg(&src)
        .arg(&dst)
        .output()
        .expect("spawn cp");

    assert_eq!(out.status.code(), Some(1), "exit status: {:?}", out.status);
    assert!(out.stdout.is_empty(), "stdout = {:?}", out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("cp:"), "stderr = {stderr:?}");
    assert!(!dst.exists(), "dst should not have been created");
    cleanup(&dir);
}

#[test]
fn dash_n_skips_existing_dst_file() {
    let dir = scratch_dir("dash-n-existing");
    let src = write_file(&dir, "src.txt", b"new content");
    let dst = write_file(&dir, "dst.txt", b"old");

    let out = Command::new(CP)
        .arg("-n")
        .arg(&src)
        .arg(&dst)
        .output()
        .expect("spawn cp");

    assert!(out.status.success(), "exit status: {:?}", out.status);
    assert!(out.stdout.is_empty(), "stdout = {:?}", out.stdout);
    assert!(out.stderr.is_empty(), "stderr = {:?}", out.stderr);
    let preserved = fs::read(&dst).expect("read dst");
    assert_eq!(preserved, b"old");
    cleanup(&dir);
}

#[test]
fn dash_n_writes_to_nonexistent_dst() {
    let dir = scratch_dir("dash-n-fresh");
    let src = write_file(&dir, "src.txt", b"fresh content");
    let dst = dir.join("dst.txt");
    assert!(!dst.exists(), "precondition: dst must not exist");

    let out = Command::new(CP)
        .arg("-n")
        .arg(&src)
        .arg(&dst)
        .output()
        .expect("spawn cp");

    assert!(out.status.success(), "exit status: {:?}", out.status);
    assert!(out.stdout.is_empty(), "stdout = {:?}", out.stdout);
    assert!(out.stderr.is_empty(), "stderr = {:?}", out.stderr);
    let copied = fs::read(&dst).expect("read dst");
    assert_eq!(copied, b"fresh content");
    cleanup(&dir);
}

#[test]
fn dash_n_with_dash_r_skips_existing_files_in_tree() {
    let dir = scratch_dir("dash-rn");
    let src = dir.join("src");
    fs::create_dir(&src).expect("create src");
    write_file(&src, "kept.txt", b"src kept");
    write_file(&src, "fresh.txt", b"src fresh");
    let dst = dir.join("dst");
    fs::create_dir(&dst).expect("create dst");
    let landed = dst.join("src");
    fs::create_dir(&landed).expect("pre-create dst/src");
    write_file(&landed, "kept.txt", b"existing");

    let out = Command::new(CP)
        .arg("-r")
        .arg("-n")
        .arg(&src)
        .arg(&dst)
        .output()
        .expect("spawn cp");

    assert!(out.status.success(), "exit status: {:?}", out.status);
    assert!(out.stdout.is_empty(), "stdout = {:?}", out.stdout);
    assert!(out.stderr.is_empty(), "stderr = {:?}", out.stderr);
    assert_eq!(
        fs::read(landed.join("kept.txt")).expect("read kept"),
        b"existing",
        "existing file inside dst tree must be preserved"
    );
    assert_eq!(
        fs::read(landed.join("fresh.txt")).expect("read fresh"),
        b"src fresh",
        "missing file inside dst tree must be copied"
    );
    cleanup(&dir);
}

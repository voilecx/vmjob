use std::collections::BTreeMap;
use std::io;

use super::*;

/// A directory of its own under the system temporary directory, removed when dropped.
struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("vmjob-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap_or_else(|e| unreachable!("{e}"));
        Self(dir)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

const LIMITS: Limits = Limits { entries: 100, bytes: 1 << 20, depth: 8 };

/// A tar of `entries`: (path, kind, content), built header by header so a
/// test can write what a hostile guest would.
fn tar_of(entries: &[(&str, tar::EntryType, &[u8])]) -> Vec<u8> {
    let mut out = Vec::new();
    for (path, kind, content) in entries {
        let mut header = tar::Header::new_gnu();
        // `set_path` refuses `..`; a hostile guest does not, so write the name raw.
        let name = header.as_gnu_mut().map_or_else(|| unreachable!(), |gnu| &mut gnu.name);
        name[..path.len()].copy_from_slice(path.as_bytes());
        header.set_entry_type(*kind);
        header.set_size(content.len() as u64);
        header.set_mode(0o644);
        if *kind == tar::EntryType::Symlink {
            header.set_link_name("/etc/passwd").unwrap_or_else(|e| unreachable!("{e}"));
        }
        header.set_cksum();
        out.extend_from_slice(header.as_bytes());
        out.extend_from_slice(content);
        out.resize(out.len().div_ceil(512) * 512, 0);
    }
    out.resize(out.len() + 1024, 0);
    out
}

#[test]
fn the_output_and_the_exit_status_come_back() {
    let dest = Scratch::new("out-ok");
    let disk = tar_of(&[
        ("a", tar::EntryType::Directory, b""),
        ("a/b.txt", tar::EntryType::Regular, b"hello"),
        ("c/d/e.bin", tar::EntryType::Regular, b"\x00\x01"),
        (".vmjob/exit", tar::EntryType::Regular, b"3\n"),
    ]);
    // The disk is larger than the tar: the zeroes after it end the archive.
    let mut padded = disk.clone();
    padded.resize(disk.len() + (1 << 16), 0);
    let unpacked =
        read_output(io::Cursor::new(padded.as_slice()), Some(&dest.0), LIMITS).unwrap_or_else(|e| unreachable!("{e}"));
    assert_eq!(unpacked, Unpacked { exit_code: Some(3), files: 2, bytes: 7, overflow: false });
    assert_eq!(fs::read(dest.0.join("a/b.txt")).ok().as_deref(), Some(&b"hello"[..]));
    assert!(dest.0.join("c/d/e.bin").is_file());
    assert!(!dest.0.join(".vmjob").exists());
}

#[test]
fn a_hostile_output_is_refused() {
    let cases: Vec<(&str, Vec<u8>)> = vec![
        ("parent", tar_of(&[("../escape", tar::EntryType::Regular, b"x")])),
        ("nested parent", tar_of(&[("a/../../escape", tar::EntryType::Regular, b"x")])),
        ("absolute", tar_of(&[("/etc/escape", tar::EntryType::Regular, b"x")])),
        ("symlink", tar_of(&[("link", tar::EntryType::Symlink, b"")])),
        ("hard link", tar_of(&[("link", tar::EntryType::Link, b"")])),
        ("device", tar_of(&[("dev", tar::EntryType::Char, b"")])),
        ("fifo", tar_of(&[("fifo", tar::EntryType::Fifo, b"")])),
        ("twice", tar_of(&[("f", tar::EntryType::Regular, b"1"), ("f", tar::EntryType::Regular, b"2")])),
        ("file over dir", tar_of(&[("f", tar::EntryType::Regular, b"1"), ("f/g", tar::EntryType::Regular, b"2")])),
        ("exit", tar_of(&[(".vmjob/exit", tar::EntryType::Regular, b"zero")])),
        ("deep", tar_of(&[("1/2/3/4/5/6/7/8/9", tar::EntryType::Regular, b"x")])),
    ];
    for (name, disk) in cases {
        let dest = Scratch::new("out-bad");
        let out = dest.0.join("out");
        fs::create_dir(&out).unwrap_or_else(|e| unreachable!("{e}"));
        let result = read_output(io::Cursor::new(disk.as_slice()), Some(&out), LIMITS);
        assert!(result.is_err(), "{name}: accepted");
        assert!(!dest.0.join("escape").exists(), "{name}: wrote outside");
        // A refused output leaves nothing, not even what came before the refusal.
        let left: Vec<_> = fs::read_dir(&out).map(|d| d.flatten().map(|e| e.file_name()).collect()).unwrap_or_default();
        assert!(left.is_empty(), "{name}: left {left:?}");
    }
}

#[test]
fn bounds_stop_an_output_that_is_too_large() {
    let dest = Scratch::new("out-big");
    let big = vec![7u8; 2048];
    let disk = tar_of(&[("a", tar::EntryType::Regular, &big), ("b", tar::EntryType::Regular, &big)]);
    let limits = Limits { bytes: 3000, ..LIMITS };
    assert!(read_output(io::Cursor::new(disk.as_slice()), Some(&dest.0), limits).is_err());
    let many: Vec<(String, tar::EntryType, &[u8])> =
        (0..5).map(|i| (format!("f{i}"), tar::EntryType::Regular, &b"x"[..])).collect();
    let refs: Vec<(&str, tar::EntryType, &[u8])> = many.iter().map(|(p, k, c)| (p.as_str(), *k, *c)).collect();
    let limits = Limits { entries: 3, ..LIMITS };
    assert!(read_output(io::Cursor::new(tar_of(&refs).as_slice()), Some(&Scratch::new("out-many").0), limits).is_err());
}

#[test]
fn without_a_destination_only_the_exit_status_is_read() {
    let disk = tar_of(&[("a.txt", tar::EntryType::Regular, b"x"), (".vmjob/exit", tar::EntryType::Regular, b"0")]);
    let unpacked = read_output(io::Cursor::new(disk.as_slice()), None, LIMITS).unwrap_or_else(|e| unreachable!("{e}"));
    assert_eq!(unpacked, Unpacked { exit_code: Some(0), files: 0, bytes: 0, overflow: false });
}

#[test]
fn the_input_disk_holds_the_job_and_the_tree() {
    let scratch = Scratch::new("in");
    let input = scratch.0.join("input");
    fs::create_dir_all(input.join("dir")).unwrap_or_else(|e| unreachable!("{e}"));
    fs::write(input.join("top.txt"), b"top").unwrap_or_else(|e| unreachable!("{e}"));
    fs::write(input.join("dir/inner.txt"), b"inner").unwrap_or_else(|e| unreachable!("{e}"));
    let job = GuestJob { command: vec!["/bin/true".into()], env: BTreeMap::new(), output: true };
    let disk = scratch.0.join("input.tar");
    write_input(&disk, Some(&input), &job).unwrap_or_else(|e| unreachable!("{e}"));
    let bytes = fs::read(&disk).unwrap_or_else(|e| unreachable!("{e}"));
    assert_eq!(bytes.len() % 512, 0);
    let mut names = Vec::new();
    let mut archive = tar::Archive::new(bytes.as_slice());
    for entry in archive.entries().unwrap_or_else(|e| unreachable!("{e}")) {
        let mut entry = entry.unwrap_or_else(|e| unreachable!("{e}"));
        let name = entry.path().unwrap_or_else(|e| unreachable!("{e}")).to_string_lossy().replace('\\', "/");
        if name == GUEST_JOB {
            let back: GuestJob = serde_json::from_reader(&mut entry).unwrap_or_else(|e| unreachable!("{e}"));
            assert_eq!(back, job);
        }
        names.push(name);
    }
    for want in [GUEST_JOB, "top.txt", "dir/inner.txt"] {
        assert!(names.iter().any(|n| n.trim_end_matches('/') == want), "{want} in {names:?}");
    }
}

#[test]
fn an_input_with_its_own_vmjob_entry_is_refused() {
    let scratch = Scratch::new("in-bad");
    let input = scratch.0.join("input");
    fs::create_dir_all(input.join(".vmjob")).unwrap_or_else(|e| unreachable!("{e}"));
    let job = GuestJob { command: vec!["x".into()], env: BTreeMap::new(), output: false };
    assert!(write_input(&scratch.0.join("t"), Some(&input), &job).is_err());
}

#[test]
fn an_overflow_marker_is_reported() {
    let disk =
        tar_of(&[(".vmjob/exit", tar::EntryType::Regular, b"0"), (".vmjob/overflow", tar::EntryType::Regular, b"")]);
    let unpacked = read_output(io::Cursor::new(disk.as_slice()), None, LIMITS).unwrap_or_else(|e| unreachable!("{e}"));
    assert!(unpacked.overflow);
}

#[test]
fn an_oversized_extension_header_is_refused_before_it_is_read() {
    let dest = Scratch::new("out-pax");
    // A PAX header that claims a megabyte of extension records.
    let mut header = tar::Header::new_ustar();
    header.set_path("././@PaxHeader").unwrap_or_else(|e| unreachable!("{e}"));
    header.set_entry_type(tar::EntryType::XHeader);
    header.set_size(1 << 20);
    header.set_cksum();
    let mut disk = header.as_bytes().to_vec();
    disk.resize(disk.len() + (1 << 20) + 1024, b'a');
    let error = read_output(io::Cursor::new(disk.as_slice()), Some(&dest.0), LIMITS).expect_err("accepted");
    assert!(error.0.contains("extension"), "{error}");
}

#[test]
fn an_output_does_not_replace_what_the_destination_holds() {
    let dest = Scratch::new("out-exists");
    fs::write(dest.0.join("a"), b"before").unwrap_or_else(|e| unreachable!("{e}"));
    let disk = tar_of(&[("a", tar::EntryType::Regular, b"after"), ("b", tar::EntryType::Regular, b"new")]);
    assert!(read_output(io::Cursor::new(disk.as_slice()), Some(&dest.0), LIMITS).is_err());
    assert_eq!(fs::read(dest.0.join("a")).ok().as_deref(), Some(&b"before"[..]));
    assert!(!dest.0.join("b").exists());
}

#[test]
fn only_a_non_empty_regular_file_is_attached() {
    let scratch = Scratch::new("check-file");
    let file = scratch.0.join("cache.img");
    fs::write(&file, [0_u8; 512]).unwrap_or_else(|e| unreachable!("{e}"));
    assert!(check_file(&file).is_ok());
    let empty = scratch.0.join("empty.img");
    fs::write(&empty, []).unwrap_or_else(|e| unreachable!("{e}"));
    assert!(check_file(&empty).is_err());
    let made = scratch.0.join("made.img");
    fs::create_dir(&made).unwrap_or_else(|e| unreachable!("{e}"));
    assert!(check_file(&made).is_err(), "a directory Kubernetes made for a missing file");
    assert!(check_file(&scratch.0.join("absent.img")).is_err());
}

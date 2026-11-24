//! PID 1 of a vmjob guest, installed as `/sbin/vmjob-init` in the root
//! filesystem image.
//!
//! It mounts the kernel filesystems and a tmpfs job directory `/j`, unpacks
//! the input disk (`/dev/vdb`) there, runs the command of
//! [`vmjob_api::GUEST_JOB`] in it, writes a tar of `/j/out` and
//! [`vmjob_api::GUEST_EXIT`] to the output disk (`/dev/vdc`), and powers the
//! virtual machine off. If it fails before the command's status is written,
//! it powers off all the same: the runner reports a guest that wrote no status.
//!
//! The kernel command line may set `vmjob.tmpfs=<size>` (a tmpfs `size=`,
//! default `75%`) for `/j`.

use std::fs::{self, File};
use std::io::{self, Seek as _, SeekFrom, Write};
use std::path::Path;
use std::process::Command;

use anyhow::Context as _;
use vmjob_api::{GUEST_EXIT, GUEST_JOB, GUEST_OVERFLOW as OVERFLOW, GuestJob};

const JOB_DIR: &str = "/j";
const INPUT_DISK: &str = "/dev/vdb";
const OUTPUT_DISK: &str = "/dev/vdc";

fn main() {
    if let Err(error) = run() {
        eprintln!("vmjob-init: {error:#}");
    }
    system::power_off();
}

fn run() -> anyhow::Result<()> {
    let cmdline = fs::read_to_string("/proc/cmdline").unwrap_or_default();
    system::mount_all(&tmpfs_size(&cmdline)).context("mounting")?;
    let input = File::open(INPUT_DISK).with_context(|| format!("opening {INPUT_DISK}"))?;
    let mut archive = tar::Archive::new(io::BufReader::new(input));
    archive.set_preserve_permissions(true);
    archive.unpack(JOB_DIR).context("unpacking the input")?;
    let job: GuestJob =
        serde_json::from_slice(&fs::read(Path::new(JOB_DIR).join(GUEST_JOB))?).context("reading the job")?;
    let out = Path::new(JOB_DIR).join("out");
    fs::create_dir_all(&out)?;

    let code = run_command(&job);
    eprintln!("vmjob-init: the command exited {code}");
    let disk =
        fs::OpenOptions::new().write(true).open(OUTPUT_DISK).with_context(|| format!("opening {OUTPUT_DISK}"))?;
    if let Err(error) = write_output(&disk, job.output.then_some(out.as_path()), code, false) {
        eprintln!("vmjob-init: the output does not fit the output disk: {error:#}");
        write_output(&disk, None, code, true).context("writing the exit status")?;
    }
    disk.sync_all()?;
    Ok(())
}

/// The command's exit status; a signal is 128 plus its number, a command that
/// could not start is 127.
fn run_command(job: &GuestJob) -> i32 {
    let Some((program, args)) = job.command.split_first() else { return 127 };
    let mut command = Command::new(program);
    command
        .args(args)
        .current_dir(JOB_DIR)
        .env_clear()
        .env("PATH", "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin")
        .env("HOME", JOB_DIR)
        .env("TMPDIR", "/tmp")
        .envs(&job.env);
    match command.status() {
        Ok(status) => status.code().unwrap_or_else(|| 128 + system::signal_of(status)),
        Err(error) => {
            eprintln!("vmjob-init: starting {program}: {error}");
            127
        }
    }
}

/// A tar of `out` (if any), the exit status and, with `overflow`, its marker,
/// from the start of `disk`.
fn write_output(mut disk: &File, out: Option<&Path>, code: i32, overflow: bool) -> anyhow::Result<()> {
    disk.seek(SeekFrom::Start(0))?;
    let mut tar = tar::Builder::new(io::BufWriter::new(disk));
    tar.follow_symlinks(false);
    if let Some(out) = out {
        for entry in fs::read_dir(out)? {
            let entry = entry?;
            let path = entry.path();
            if entry.file_type()?.is_dir() {
                tar.append_dir_all(entry.file_name(), &path)?;
            } else {
                tar.append_path_with_name(&path, entry.file_name())?;
            }
        }
    }
    let mut add = |name: &str, content: &[u8]| -> io::Result<()> {
        let mut header = tar::Header::new_ustar();
        header.set_size(content.len() as u64);
        header.set_mode(0o644);
        header.set_entry_type(tar::EntryType::Regular);
        tar.append_data(&mut header, name, content)
    };
    add(GUEST_EXIT, format!("{code}\n").as_bytes())?;
    if overflow {
        add(OVERFLOW, b"")?;
    }
    let mut writer = tar.into_inner()?;
    writer.flush()?;
    Ok(())
}

/// `vmjob.tmpfs=` of the kernel command line, or `75%`; only a size tmpfs
/// accepts gets through.
fn tmpfs_size(cmdline: &str) -> String {
    cmdline
        .split_whitespace()
        .find_map(|arg| arg.strip_prefix("vmjob.tmpfs="))
        .filter(|size| !size.is_empty() && size.bytes().all(|b| b.is_ascii_digit() || b"kmgKMG%".contains(&b)))
        .unwrap_or("75%")
        .to_owned()
}

#[cfg(target_os = "linux")]
mod system {
    use nix::mount::{MsFlags, mount};
    use nix::sys::reboot::{RebootMode, reboot};

    pub fn mount_all(tmpfs_size: &str) -> anyhow::Result<()> {
        let quiet = MsFlags::MS_NOSUID | MsFlags::MS_NODEV | MsFlags::MS_NOEXEC;
        // A kernel built with CONFIG_DEVTMPFS_MOUNT has mounted /dev before
        // init runs: a filesystem that is already there is not an error.
        let once = |kind: &str, at: &str, flags: MsFlags, data: Option<&str>| match mount(
            Some(kind),
            at,
            Some(kind),
            flags,
            data,
        ) {
            Ok(()) | Err(nix::errno::Errno::EBUSY) => Ok(()),
            Err(e) => Err(anyhow::anyhow!("mounting {kind} at {at}: {e}")),
        };
        once("proc", "/proc", quiet, None)?;
        once("sysfs", "/sys", quiet, None)?;
        once("devtmpfs", "/dev", MsFlags::MS_NOSUID, Some("mode=0755"))?;
        once("tmpfs", "/tmp", MsFlags::MS_NOSUID | MsFlags::MS_NODEV, Some("mode=1777"))?;
        let job = format!("mode=0755,size={tmpfs_size}");
        once("tmpfs", super::JOB_DIR, MsFlags::MS_NOSUID | MsFlags::MS_NODEV, Some(job.as_str()))?;
        Ok(())
    }

    pub fn power_off() -> ! {
        nix::unistd::sync();
        let _ = reboot(RebootMode::RB_POWER_OFF);
        // PID 1 must not return; there is nothing left to do if the power-off failed.
        loop {
            std::thread::park();
        }
    }

    pub fn signal_of(status: std::process::ExitStatus) -> i32 {
        use std::os::unix::process::ExitStatusExt as _;
        status.signal().unwrap_or(0)
    }
}

#[cfg(not(target_os = "linux"))]
mod system {
    pub fn mount_all(_: &str) -> anyhow::Result<()> {
        anyhow::bail!("vmjob-init runs in a Linux guest only")
    }

    pub fn power_off() -> ! {
        std::process::exit(1)
    }

    pub fn signal_of(_: std::process::ExitStatus) -> i32 {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_tmpfs_size_comes_from_the_command_line_when_it_is_one() {
        assert_eq!(tmpfs_size("console=ttyS0 quiet"), "75%");
        assert_eq!(tmpfs_size("quiet vmjob.tmpfs=6G"), "6G");
        assert_eq!(tmpfs_size("vmjob.tmpfs=50%"), "50%");
        assert_eq!(tmpfs_size("vmjob.tmpfs=1G,mode=4777"), "75%");
    }

    #[test]
    fn the_output_tar_holds_the_tree_the_status_and_the_overflow_marker() {
        let dir = std::env::temp_dir().join(format!("vmjob-init-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("out/sub")).expect("dir");
        fs::write(dir.join("out/sub/a.txt"), b"a").expect("file");
        let disk_path = dir.join("disk");
        let disk = File::options().read(true).write(true).create(true).truncate(true).open(&disk_path).expect("disk");
        disk.set_len(1 << 20).expect("len");
        write_output(&disk, Some(&dir.join("out")), 7, false).expect("output");
        let read = |overflow: bool| {
            let mut names = Vec::new();
            let mut archive = tar::Archive::new(File::open(&disk_path).expect("open"));
            for entry in archive.entries().expect("entries") {
                names.push(entry.expect("entry").path().expect("path").to_string_lossy().replace('\\', "/"));
            }
            assert_eq!(names.iter().any(|n| n == OVERFLOW), overflow);
            names
        };
        let names = read(false);
        assert!(names.iter().any(|n| n == "sub/a.txt") && names.iter().any(|n| n == GUEST_EXIT), "{names:?}");
        write_output(&disk, None, 7, true).expect("status");
        let names = read(true);
        assert!(!names.iter().any(|n| n == "sub/a.txt"), "{names:?}");
        let _ = fs::remove_dir_all(&dir);
    }
}

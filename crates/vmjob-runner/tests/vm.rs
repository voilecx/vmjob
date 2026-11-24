//! Real virtual machines through the runner binary, on a Linux host with KVM.
//!
//! Skipped unless `VMJOB_TEST_HYPERVISOR`, `VMJOB_TEST_KERNEL` and
//! `VMJOB_TEST_ROOTFS` name a `cloud-hypervisor` binary, a guest kernel and a
//! root filesystem image with `vmjob-init` at `/sbin/vmjob-init` and a shell at
//! `/bin/sh`. CONTRIBUTING.md says how to make them; CI does.

#![allow(clippy::expect_used, reason = "a test that cannot set up fails")]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use vmjob_api::{GuestJob, RUNNER_CONFIG, RunnerConfig, RunnerResult};

struct Guest {
    hypervisor: PathBuf,
    kernel: PathBuf,
    rootfs: PathBuf,
}

fn guest() -> Option<Guest> {
    let var = |name: &str| std::env::var_os(name).map(PathBuf::from);
    let guest = Guest {
        hypervisor: var("VMJOB_TEST_HYPERVISOR")?,
        kernel: var("VMJOB_TEST_KERNEL")?,
        rootfs: var("VMJOB_TEST_ROOTFS")?,
    };
    Path::new("/dev/kvm").exists().then_some(guest)
}

/// A directory of its own under the system temporary directory, removed when dropped.
struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("vmjob-vm-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("work")).expect("work");
        std::fs::create_dir_all(dir.join("out")).expect("out");
        std::fs::create_dir_all(dir.join("in")).expect("in");
        Self(dir)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Run `script` in a guest with `timeout` seconds and `output_mib` of output
/// disk (0: no output), and read the runner's result.
fn run(guest: &Guest, dir: &Scratch, script: &str, timeout: u32, output_mib: u64) -> RunnerResult {
    let config = RunnerConfig {
        kernel: guest.kernel.display().to_string(),
        rootfs: guest.rootfs.display().to_string(),
        input: Some(dir.0.join("in").display().to_string()),
        output: (output_mib > 0).then(|| (dir.0.join("out").display().to_string(), output_mib << 20)),
        work: dir.0.join("work").display().to_string(),
        kernel_args: None,
        job: GuestJob {
            command: vec!["/bin/sh".into(), "-c".into(), script.into()],
            env: BTreeMap::from([("GREETING".into(), "hi".into())]),
            output: output_mib > 0,
        },
        cpus: 1,
        memory_mib: 256,
        timeout_seconds: timeout,
    };
    let result = dir.0.join("result.json");
    let status = Command::new(env!("CARGO_BIN_EXE_vmjob-runner"))
        .env(RUNNER_CONFIG, serde_json::to_string(&config).expect("config"))
        .env("VMJOB_HYPERVISOR", &guest.hypervisor)
        .env("VMJOB_TERMINATION_LOG", &result)
        .status()
        .expect("runner");
    let result: RunnerResult = serde_json::from_slice(&std::fs::read(&result).expect("result")).expect("result json");
    assert!(status.success() || result.reason.as_deref() == Some("RunnerFailed"), "{result:?}");
    result
}

macro_rules! needs_kvm {
    () => {
        match guest() {
            Some(guest) => guest,
            None => {
                eprintln!("skipped: no KVM guest (VMJOB_TEST_HYPERVISOR, VMJOB_TEST_KERNEL, VMJOB_TEST_ROOTFS)");
                return;
            }
        }
    };
}

#[test]
fn files_go_in_and_come_out_with_the_exit_status() {
    let guest = needs_kvm!();
    let dir = Scratch::new("files");
    std::fs::write(dir.0.join("in/in.txt"), "from the host\n").expect("input");
    let result = run(
        &guest,
        &dir,
        r#"cat in.txt > out/copy.txt && echo "$GREETING" > out/env.txt && mkdir out/sub && echo x > out/sub/y && exit 3"#,
        60,
        4,
    );
    assert_eq!((result.exit_code, result.reason.as_deref()), (Some(3), Some("CommandFailed")), "{}", result.message);
    let read = |name: &str| std::fs::read_to_string(dir.0.join("out").join(name)).expect(name);
    assert_eq!(read("copy.txt"), "from the host\n");
    assert_eq!(read("env.txt"), "hi\n");
    assert_eq!(read("sub/y"), "x\n");
}

#[test]
fn the_guest_has_no_network_device() {
    let guest = needs_kvm!();
    let dir = Scratch::new("network");
    let result = run(&guest, &dir, "ls /sys/class/net > out/net.txt", 60, 1);
    assert_eq!(result.reason, None, "{}", result.message);
    assert_eq!(std::fs::read_to_string(dir.0.join("out/net.txt")).expect("net"), "lo\n");
}

#[test]
fn a_guest_past_its_timeout_is_stopped() {
    let guest = needs_kvm!();
    let dir = Scratch::new("timeout");
    let result = run(&guest, &dir, "sleep 120", 5, 0);
    assert_eq!(result.reason.as_deref(), Some("Timeout"), "{}", result.message);
}

#[test]
fn a_link_in_the_output_refuses_it_all() {
    let guest = needs_kvm!();
    let dir = Scratch::new("hostile");
    let result = run(&guest, &dir, "echo a > out/a && ln -s /etc/passwd out/link", 60, 1);
    assert_eq!(result.reason.as_deref(), Some("OutputRefused"), "{}", result.message);
    assert_eq!(std::fs::read_dir(dir.0.join("out")).expect("out").count(), 0);
}

#[test]
fn an_output_larger_than_its_disk_still_reports_the_status() {
    let guest = needs_kvm!();
    let dir = Scratch::new("overflow");
    let result = run(&guest, &dir, "head -c 3000000 /dev/zero > out/big", 60, 1);
    assert_eq!((result.reason.as_deref(), result.exit_code), (Some("OutputTooLarge"), Some(0)), "{}", result.message);
}

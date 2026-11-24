//! One boot of Cloud Hypervisor, with no network device, to its end.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::Context as _;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::process::Command;

/// What one boot needs.
#[derive(Debug, Clone)]
pub struct Boot {
    /// The `cloud-hypervisor` binary.
    pub hypervisor: PathBuf,
    /// The kernel.
    pub kernel: PathBuf,
    /// The whole kernel command line.
    pub cmdline: String,
    /// Disks in order: they are `/dev/vda`, `/dev/vdb`, ... in the guest.
    pub disks: Vec<Disk>,
    /// Virtual CPUs.
    pub cpus: u8,
    /// Memory in MiB.
    pub memory_mib: u32,
    /// Where Cloud Hypervisor reports its events (JSON).
    pub events: PathBuf,
    /// The longest the virtual machine may run.
    pub timeout: Duration,
}

/// A disk image file.
#[derive(Debug, Clone)]
pub struct Disk {
    /// The image file.
    pub path: PathBuf,
    /// Whether the guest may only read it.
    pub readonly: bool,
}

/// How a boot ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum End {
    /// The guest powered the virtual machine off.
    PoweredOff,
    /// The guest asked to reboot: its kernel panicked (`panic=1`) or its init
    /// failed. The virtual machine was stopped instead.
    Rebooted,
    /// It ran past the timeout and was stopped.
    TimedOut,
    /// Cloud Hypervisor exited with this status without a power-off.
    HypervisorFailed(Option<i32>),
}

/// The end of the guest console kept for a report.
const CONSOLE_TAIL: usize = 64 * 1024;

/// Run the virtual machine. The guest console (`--serial tty`) is copied to
/// this process's standard output as it comes, and its last
/// [`CONSOLE_TAIL`] bytes are returned with the end.
pub async fn run(boot: &Boot) -> anyhow::Result<(End, String)> {
    let mut command = Command::new(&boot.hypervisor);
    command
        .arg("--kernel")
        .arg(&boot.kernel)
        .arg("--cmdline")
        .arg(&boot.cmdline)
        .arg("--cpus")
        .arg(format!("boot={}", boot.cpus))
        .arg("--memory")
        .arg(format!("size={}M", boot.memory_mib))
        .arg("--serial")
        .arg("tty")
        .arg("--console")
        .arg("off")
        .arg("--event-monitor")
        .arg(format!("path={}", boot.events.display()));
    // No `--net`: the guest has no network device at all.
    command.arg("--disk");
    for disk in &boot.disks {
        command.arg(disk_arg(disk)?);
    }
    command.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::inherit()).kill_on_drop(true);
    let mut child = command.spawn().with_context(|| format!("starting {}", boot.hypervisor.display()))?;
    let mut stdout = child.stdout.take().context("the hypervisor's console")?;

    let mut tail = VecDeque::with_capacity(CONSOLE_TAIL);
    let mut buffer = vec![0u8; 16 * 1024];
    let mut out = tokio::io::stdout();
    let deadline = tokio::time::Instant::now() + boot.timeout;
    let mut poll = tokio::time::interval(Duration::from_millis(500));
    let mut console_open = true;
    let end = loop {
        tokio::select! {
            read = stdout.read(&mut buffer), if console_open => {
                let n = read.unwrap_or(0);
                if n == 0 {
                    console_open = false;
                    continue;
                }
                let _ = out.write_all(&buffer[..n]).await;
                for &byte in &buffer[..n] {
                    if tail.len() == CONSOLE_TAIL {
                        tail.pop_front();
                    }
                    tail.push_back(byte);
                }
            }
            status = child.wait() => {
                let status = status?;
                break if rebooted(&boot.events).await == Some(true) {
                    End::Rebooted
                } else if status.success() {
                    End::PoweredOff
                } else {
                    End::HypervisorFailed(status.code())
                };
            }
            _ = poll.tick() => {
                if rebooted(&boot.events).await == Some(true) {
                    let _ = child.kill().await;
                    break End::Rebooted;
                }
                if tokio::time::Instant::now() >= deadline {
                    let _ = child.kill().await;
                    break End::TimedOut;
                }
            }
        }
    };
    // What the console still holds after the end.
    let mut rest = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(1), stdout.read_to_end(&mut rest)).await;
    let _ = out.write_all(&rest).await;
    let _ = out.flush().await;
    for byte in rest {
        if tail.len() == CONSOLE_TAIL {
            tail.pop_front();
        }
        tail.push_back(byte);
    }
    let _ = child.wait().await;
    Ok((end, String::from_utf8_lossy(tail.make_contiguous()).into_owned()))
}

/// `path=...,readonly=on`, refusing a path Cloud Hypervisor's option parser
/// would split.
fn disk_arg(disk: &Disk) -> anyhow::Result<String> {
    let path = disk.path.to_str().context("a disk path that is not UTF-8")?;
    if path.contains([',', '=', '"']) {
        anyhow::bail!("a disk path with `,`, `=` or `\"`: {path}");
    }
    // Cloud Hypervisor asks for the image type: a raw file is never probed as qcow2.
    Ok(if disk.readonly {
        format!("path={path},image_type=raw,readonly=on")
    } else {
        format!("path={path},image_type=raw")
    })
}

/// Whether the event log shows the guest asking for a reboot; `None` while
/// there is no log yet. The log is a stream of JSON objects; only the VM's own
/// `reboot` counts, not the `reset` its devices report on a power-off too.
async fn rebooted(events: &Path) -> Option<bool> {
    let text = tokio::fs::read_to_string(events).await.ok()?;
    let mut stream = serde_json::Deserializer::from_str(&text).into_iter::<serde_json::Value>();
    Some(
        stream.by_ref().map_while(Result::ok).any(|event| {
            event["source"] == "vm" && event["event"].as_str().is_some_and(|name| name.starts_with("reboot"))
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_disk_path_the_parser_would_split_is_refused() {
        let ok = Disk { path: PathBuf::from("/work/input.tar"), readonly: true };
        assert_eq!(disk_arg(&ok).ok().as_deref(), Some("path=/work/input.tar,image_type=raw,readonly=on"));
        for bad in ["/work/a,b", "/work/a=b", "/work/\"a"] {
            assert!(disk_arg(&Disk { path: PathBuf::from(bad), readonly: false }).is_err(), "{bad}");
        }
    }

    #[tokio::test]
    async fn a_reboot_event_is_seen() {
        let dir = std::env::temp_dir().join(format!("vmjob-events-{}", std::process::id()));
        tokio::fs::create_dir_all(&dir).await.ok();
        let events = dir.join("events.json");
        assert_eq!(rebooted(&events).await, None);
        tokio::fs::write(
            &events,
            r#"{"timestamp":{"secs":0,"nanos":1},"source":"vm","event":"booted","properties":null}"#,
        )
        .await
        .ok();
        assert_eq!(rebooted(&events).await, Some(false));
        // A power-off: the devices report a reset, the VM does not reboot.
        tokio::fs::write(
            &events,
            "{\"source\":\"vm\",\"event\":\"booted\"}
{\"source\":\"virtio-device\",\"event\":\"reset\"}
{\"source\":\"vm\",\"event\":\"shutdown\"}",
        )
        .await
        .ok();
        assert_eq!(rebooted(&events).await, Some(false));
        tokio::fs::write(
            &events,
            "{\"source\":\"vm\",\"event\":\"booted\"}
{
  \"source\": \"vm\",
  \"event\": \"rebooting\"
}",
        )
        .await
        .ok();
        assert_eq!(rebooted(&events).await, Some(true));
        let _ = tokio::fs::remove_dir_all(&dir).await;
    }
}

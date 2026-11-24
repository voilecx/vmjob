//! The runner: one `VmJob`'s pod. It reads its [`RunnerConfig`] from the
//! environment, makes the input and output disks, boots the virtual machine
//! with no network device, unpacks the output, and writes a [`RunnerResult`]
//! as its termination message.
//!
//! It exits 0 when the virtual machine ran to its end, whatever the command's
//! status (that is in the result), and 1 when it did not.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Context as _;
use vmjob_api::{RUNNER_CONFIG, RunnerConfig, RunnerResult};

mod disks;
mod vm;

/// Where Kubernetes reads a container's termination message by default.
const TERMINATION_LOG: &str = "/dev/termination-log";

/// The output disk when the job wants no output: room for the exit status.
const MINIMAL_OUTPUT: u64 = 1 << 20;

#[tokio::main(flavor = "current_thread")]
async fn main() -> std::process::ExitCode {
    let result = match run().await {
        Ok(result) => result,
        Err(error) => {
            RunnerResult { exit_code: None, reason: Some("RunnerFailed".into()), message: format!("{error:#}") }
        }
    };
    let message = result.to_termination_message();
    let path = std::env::var("VMJOB_TERMINATION_LOG").unwrap_or_else(|_| TERMINATION_LOG.into());
    if let Err(error) = std::fs::write(&path, &message) {
        eprintln!("vmjob-runner: writing {path}: {error}");
    }
    println!("\nvmjob-runner: {message}");
    if result.reason.as_deref() == Some("RunnerFailed") {
        std::process::ExitCode::FAILURE
    } else {
        std::process::ExitCode::SUCCESS
    }
}

async fn run() -> anyhow::Result<RunnerResult> {
    let config: RunnerConfig =
        serde_json::from_str(&std::env::var(RUNNER_CONFIG).with_context(|| format!("{RUNNER_CONFIG} is not set"))?)
            .with_context(|| format!("{RUNNER_CONFIG} is not a runner configuration"))?;
    let hypervisor =
        PathBuf::from(std::env::var("VMJOB_HYPERVISOR").unwrap_or_else(|_| "/usr/local/bin/cloud-hypervisor".into()));
    let work = PathBuf::from(&config.work);
    let input = work.join("input.tar");
    let output = work.join("output.raw");
    let output_bytes = config.output.as_ref().map_or(MINIMAL_OUTPUT, |(_, bytes)| *bytes);
    {
        let (input_dir, job, input) = (config.input.clone(), config.job.clone(), input.clone());
        tokio::task::spawn_blocking(move || disks::write_input(&input, input_dir.as_deref().map(Path::new), &job))
            .await??;
    }
    disks::create_output(&output, output_bytes)?;

    let boot = vm::Boot {
        hypervisor,
        kernel: PathBuf::from(&config.kernel),
        cmdline: cmdline(config.kernel_args.as_deref()),
        disks: vec![
            vm::Disk { path: PathBuf::from(&config.rootfs), readonly: true },
            vm::Disk { path: input.clone(), readonly: true },
            vm::Disk { path: output.clone(), readonly: false },
        ],
        cpus: config.cpus,
        memory_mib: config.memory_mib,
        events: work.join("events.json"),
        timeout: Duration::from_secs(config.timeout_seconds.into()),
    };
    let (end, console) = vm::run(&boot).await?;
    let failed =
        |reason: &str| Ok(RunnerResult { exit_code: None, reason: Some(reason.into()), message: console.clone() });
    match end {
        vm::End::PoweredOff => {}
        vm::End::Rebooted => return failed("GuestCrashed"),
        vm::End::TimedOut => return failed("Timeout"),
        vm::End::HypervisorFailed(code) => {
            return Ok(RunnerResult {
                exit_code: None,
                reason: Some("RunnerFailed".into()),
                message: format!("cloud-hypervisor exited with {code:?}\n{console}"),
            });
        }
    }
    let destination = config.output.as_ref().map(|(dir, _)| PathBuf::from(dir));
    let limits = disks::Limits { entries: 1_000_000, bytes: output_bytes, depth: 64 };
    let unpacked = tokio::task::spawn_blocking(move || {
        let disk = std::fs::File::open(&output)?;
        disks::read_output(std::io::BufReader::new(disk), destination.as_deref(), limits).map_err(anyhow::Error::from)
    })
    .await?;
    Ok(match unpacked {
        Err(error) => RunnerResult {
            exit_code: None,
            reason: Some("OutputRefused".into()),
            message: format!("{error:#}\n{console}"),
        },
        Ok(unpacked) if unpacked.overflow => RunnerResult {
            exit_code: unpacked.exit_code,
            reason: Some("OutputTooLarge".into()),
            message: format!(
                "the output did not fit {output_bytes} bytes
{console}"
            ),
        },
        Ok(unpacked) => match unpacked.exit_code {
            None => RunnerResult {
                exit_code: None,
                reason: Some("GuestCrashed".into()),
                message: format!("the guest wrote no exit status\n{console}"),
            },
            Some(0) => RunnerResult { exit_code: Some(0), reason: None, message: console },
            Some(code) => {
                RunnerResult { exit_code: Some(code), reason: Some("CommandFailed".into()), message: console }
            }
        },
    })
}

/// The kernel command line: the root disk read-only, the guest's init, a
/// reboot a second after a panic so the runner sees it, then the job's own.
fn cmdline(extra: Option<&str>) -> String {
    let mut line = String::from("console=ttyS0 root=/dev/vda ro rootfstype=ext4 init=/sbin/vmjob-init panic=1 quiet");
    if let Some(extra) = extra.map(str::trim).filter(|e| !e.is_empty()) {
        line.push(' ');
        line.push_str(extra);
    }
    line
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_job_adds_to_the_command_line() {
        assert!(super::cmdline(None).ends_with("quiet"));
        assert!(super::cmdline(Some(" vmjob.tmpfs=75% ")).ends_with("quiet vmjob.tmpfs=75%"));
    }
}

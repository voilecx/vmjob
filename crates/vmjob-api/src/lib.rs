//! The `VmJob` resource and the two small protocols around it.
//!
//! A `VmJob` runs one command in a disposable Cloud Hypervisor virtual machine
//! that has no network device. The operator turns it into a runner pod; the
//! runner boots the VM and moves files in and out of it on disks:
//!
//! | disk       | contents                                              |
//! |------------|-------------------------------------------------------|
//! | `/dev/vda` | the guest root filesystem, read-only                  |
//! | `/dev/vdb` | a tar of the input directory plus [`GUEST_JOB`]       |
//! | `/dev/vdc` | zeroes; the guest writes a tar of its output there    |
//! | `/dev/vdd`… | the job's [`VmJobSpec::disks`], in order              |
//!
//! The guest's init ([`GuestJob`]) unpacks the input, runs the command, packs
//! its output directory and [`GUEST_EXIT`], and powers the VM off. The runner
//! unpacks the output where the job asked and reports a [`RunnerResult`] as its
//! termination message, which the operator copies into the status.

use std::collections::BTreeMap;

use k8s_openapi::api::core::v1::{LocalObjectReference, Volume};
use kube::CustomResource;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// The API group of the resource.
pub const GROUP: &str = "vmjob.voile.cx";

/// The extended resource a runner container requests for `/dev/kvm`.
pub const KVM_RESOURCE: &str = "vmjob.voile.cx/kvm";

/// The entry of the input tar that describes the command, for the guest.
pub const GUEST_JOB: &str = ".vmjob/job.json";

/// The entry of the output tar that holds the command's exit status, decimal.
pub const GUEST_EXIT: &str = ".vmjob/exit";

/// The entry of the output tar that says the output did not fit the output
/// disk, so the guest wrote its exit status alone.
pub const GUEST_OVERFLOW: &str = ".vmjob/overflow";

/// The most a termination message may hold (Kubernetes truncates at 4096).
pub const RESULT_MAX_BYTES: usize = 4000;

/// One command in a disposable virtual machine with no network.
#[derive(CustomResource, Deserialize, Serialize, Clone, Debug, PartialEq, JsonSchema)]
#[kube(
    group = "vmjob.voile.cx",
    version = "v1alpha1",
    kind = "VmJob",
    namespaced,
    status = "VmJobStatus",
    shortname = "vmj",
    printcolumn = r#"{"name":"Phase","type":"string","jsonPath":".status.phase"}"#,
    printcolumn = r#"{"name":"Exit","type":"integer","jsonPath":".status.exitCode"}"#,
    printcolumn = r#"{"name":"Age","type":"date","jsonPath":".metadata.creationTimestamp"}"#
)]
#[serde(rename_all = "camelCase")]
pub struct VmJobSpec {
    /// The guest kernel: an uncompressed `vmlinux` (or a `bzImage`) in an OCI image.
    pub kernel: ImageFile,
    /// The guest root filesystem: a raw ext4 image file in an OCI image. It is
    /// attached read-only; the guest's init keeps its writes in memory.
    pub rootfs: ImageFile,
    /// Arguments added to the kernel command line.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kernel_args: Option<String>,
    /// The command, run by the guest's init in the job directory.
    pub command: Vec<String>,
    /// Its environment.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    /// Virtual CPUs, 1..=64. The runner container requests as many.
    pub cpus: u8,
    /// Guest memory in MiB, 128..=262144.
    pub memory_mib: u32,
    /// What the job reads: a directory of a volume, copied into the guest's job
    /// directory before the command runs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<Transfer>,
    /// Where the job's output goes: the guest's `out/` directory is copied to a
    /// directory of a volume after the command ends.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<Output>,
    /// Raw disk image files attached after the output disk, in order: the
    /// guest sees them as `/dev/vdd`, `/dev/vde`, ... At most [`MAX_DISKS`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub disks: Vec<DiskFile>,
    /// The longest the virtual machine may run, 1..=86400 seconds.
    pub timeout_seconds: u32,
    /// Remove the job this long after it finished. Absent: keep it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttl_seconds_after_finished: Option<u32>,
    /// Constrain the runner pod to nodes with these labels.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub node_selector: BTreeMap<String, String>,
    /// Secrets in the job's namespace to pull the kernel and root filesystem
    /// images with, when they are private.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub image_pull_secrets: Vec<LocalObjectReference>,
}

/// A file inside an OCI image, mounted as an image volume.
#[derive(Deserialize, Serialize, Clone, Debug, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ImageFile {
    /// The image reference; a digest reference pins what boots.
    pub image: String,
    /// The file's path inside the image, relative to its root.
    pub path: String,
    /// `IfNotPresent` (absent), `Always` or `Never`: with `Always` a tag that
    /// moves boots its newest image, at the cost of one registry lookup.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_pull_policy: Option<String>,
}

/// A directory of a volume.
#[derive(Deserialize, Serialize, Clone, Debug, PartialEq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Transfer {
    /// The volume: any source the runner's pod can mount. Its `name` is
    /// replaced with the runner's own.
    pub volume: Volume,
    /// The directory inside the volume, relative. Empty: its root.
    #[serde(default)]
    pub path: String,
}

/// The most [`VmJobSpec::disks`] a job may attach.
pub const MAX_DISKS: usize = 4;

/// A raw disk image file of a volume, attached to the guest as one block
/// device. The runner never reads or writes its contents: what a writable
/// disk's guest writes stays in the file, for whatever job attaches it next.
/// The file must already exist as a regular file the runner's user (65532)
/// can open; a missing one is not created.
#[derive(Deserialize, Serialize, Clone, Debug, PartialEq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DiskFile {
    /// The volume: any source the runner's pod can mount. Its `name` is
    /// replaced with the runner's own.
    pub volume: Volume,
    /// The file inside the volume, relative.
    pub path: String,
    /// Attach it read-only.
    #[serde(default)]
    pub read_only: bool,
}

/// Where the output goes, and how much of it there may be.
#[derive(Deserialize, Serialize, Clone, Debug, PartialEq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Output {
    /// The destination.
    #[serde(flatten)]
    pub to: Transfer,
    /// The size of the output disk in MiB, 1..=65536: the most the output tar
    /// may take.
    pub max_mib: u32,
}

/// What became of a job.
#[derive(Deserialize, Serialize, Clone, Debug, Default, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct VmJobStatus {
    /// Where the job stands.
    #[serde(default)]
    pub phase: Phase,
    /// The runner pod.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pod: Option<String>,
    /// When the runner pod was created, RFC 3339.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_time: Option<String>,
    /// When the job finished, RFC 3339.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completion_time: Option<String>,
    /// The command's exit status in the guest, once it ran to the end.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    /// Why the job failed, in one word: `CommandFailed`, `Timeout`,
    /// `GuestCrashed`, `OutputRefused`, `OutputTooLarge`, `RunnerFailed`,
    /// `PodLost`, `Invalid`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// The end of the guest's console, or the runner's error.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// The generation of the spec this status is about.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_generation: Option<i64>,
}

/// The phase of a job.
#[derive(Deserialize, Serialize, Clone, Copy, Debug, Default, PartialEq, Eq, JsonSchema)]
pub enum Phase {
    /// Accepted; the runner pod is not running yet.
    #[default]
    Pending,
    /// The runner pod runs the virtual machine.
    Running,
    /// The command ran to the end and exited 0.
    Succeeded,
    /// It exited non-zero, or did not run to the end.
    Failed,
}

impl Phase {
    /// Whether the job is over.
    #[must_use]
    pub fn finished(self) -> bool {
        matches!(self, Self::Succeeded | Self::Failed)
    }
}

/// Why a spec cannot be run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invalid(pub String);

impl std::fmt::Display for Invalid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Invalid {}

impl VmJobSpec {
    /// Check the bounds the runner relies on.
    ///
    /// # Errors
    /// The first field out of bounds.
    pub fn validate(&self) -> Result<(), Invalid> {
        let bad = |what: &str| Err(Invalid(what.to_owned()));
        if self.command.is_empty() || self.command[0].is_empty() {
            return bad("command must name a program");
        }
        if !(1..=64).contains(&self.cpus) {
            return bad("cpus must be 1..=64");
        }
        if !(128..=262_144).contains(&self.memory_mib) {
            return bad("memoryMib must be 128..=262144");
        }
        if !(1..=86_400).contains(&self.timeout_seconds) {
            return bad("timeoutSeconds must be 1..=86400");
        }
        for file in [&self.kernel, &self.rootfs] {
            if file.image.trim().is_empty() {
                return bad("an image file needs an image");
            }
            if !relative_path(&file.path) || file.path.is_empty() {
                return bad("an image file's path must be relative, without `..`");
            }
            if !matches!(file.image_pull_policy.as_deref(), None | Some("Always" | "IfNotPresent" | "Never")) {
                return bad("an image file's imagePullPolicy must be Always, IfNotPresent or Never");
            }
        }
        for transfer in [self.input.as_ref(), self.output.as_ref().map(|o| &o.to)].into_iter().flatten() {
            if !transfer.path.is_empty() && !relative_path(&transfer.path) {
                return bad("a transfer's path must be relative, without `..`");
            }
        }
        if self.disks.len() > MAX_DISKS {
            return bad("disks may hold at most 4 files");
        }
        for disk in &self.disks {
            if disk.path.is_empty() || !relative_path(&disk.path) || disk.path.ends_with('/') {
                return bad("a disk's path must name a file, relative, without `..`");
            }
        }
        if let Some(output) = &self.output
            && !(1..=65_536).contains(&output.max_mib)
        {
            return bad("output.maxMib must be 1..=65536");
        }
        if let Some(args) = &self.kernel_args
            && (args.len() > 1024 || args.chars().any(char::is_control))
        {
            return bad("kernelArgs must be at most 1024 printable characters");
        }
        for (key, value) in &self.env {
            if key.is_empty() || key.contains(['=', '\0']) || value.contains('\0') {
                return bad("an env name must be non-empty without `=`, and no NUL anywhere");
            }
        }
        Ok(())
    }
}

/// A path the runner can join below a directory: relative, no `..`, no NUL.
#[must_use]
pub fn relative_path(path: &str) -> bool {
    !path.starts_with('/') && !path.contains('\0') && !path.contains('\\') && path.split('/').all(|part| part != "..")
}

/// What the operator hands the runner (environment variable [`RUNNER_CONFIG`]).
#[derive(Deserialize, Serialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RunnerConfig {
    /// The kernel file.
    pub kernel: String,
    /// The root filesystem image file.
    pub rootfs: String,
    /// The input directory, if any.
    pub input: Option<String>,
    /// The output directory and the output disk's size in bytes, if any.
    pub output: Option<(String, u64)>,
    /// Where the runner keeps the disks it makes.
    pub work: String,
    /// The job's own disk files and whether each is read-only, in order.
    #[serde(default)]
    pub disks: Vec<(String, bool)>,
    /// Kernel command line additions.
    pub kernel_args: Option<String>,
    /// What the guest runs.
    pub job: GuestJob,
    /// Virtual CPUs.
    pub cpus: u8,
    /// Guest memory in MiB.
    pub memory_mib: u32,
    /// Seconds before the virtual machine is stopped.
    pub timeout_seconds: u32,
}

/// The environment variable that carries a [`RunnerConfig`] as JSON.
pub const RUNNER_CONFIG: &str = "VMJOB_RUNNER_CONFIG";

/// What the guest's init runs ([`GUEST_JOB`] in the input tar).
#[derive(Deserialize, Serialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct GuestJob {
    /// The program and its arguments.
    pub command: Vec<String>,
    /// Its environment, on top of `PATH`, `HOME` and `TMPDIR`.
    pub env: BTreeMap<String, String>,
    /// Whether to pack `out/` onto the output disk.
    pub output: bool,
}

/// How a runner ended, as its termination message.
#[derive(Deserialize, Serialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RunnerResult {
    /// The command's exit status, when it ran to the end.
    pub exit_code: Option<i32>,
    /// One of the [`VmJobStatus::reason`] words, or absent on success.
    pub reason: Option<String>,
    /// The end of the guest console, or the error.
    pub message: String,
}

impl RunnerResult {
    /// The JSON form, with `message` cut from the front until it fits a
    /// termination message.
    #[must_use]
    pub fn to_termination_message(&self) -> String {
        let mut result = self.clone();
        loop {
            let text = serde_json::to_string(&result).unwrap_or_default();
            if text.len() <= RESULT_MAX_BYTES || result.message.is_empty() {
                return text;
            }
            let cut = result.message.len().saturating_sub(RESULT_MAX_BYTES / 2).max(1);
            let at = (cut..=result.message.len())
                .find(|&i| result.message.is_char_boundary(i))
                .unwrap_or(result.message.len());
            result.message.replace_range(..at, "");
        }
    }
}

#[cfg(test)]
mod tests;

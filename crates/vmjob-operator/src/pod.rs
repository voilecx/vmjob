//! The runner pod of a job, and what its end means for the job.

use std::collections::BTreeMap;

use k8s_openapi::api::core::v1::{
    Capabilities, Container, EmptyDirVolumeSource, EnvVar, ImageVolumeSource, Pod, PodSecurityContext, PodSpec,
    ResourceRequirements, SeccompProfile, SecurityContext, Volume, VolumeMount,
};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use kube::Resource as _;
use kube::api::ObjectMeta;
use vmjob_api::{GuestJob, KVM_RESOURCE, Phase, RUNNER_CONFIG, RunnerConfig, RunnerResult, VmJob};

/// How the operator was started.
#[derive(Debug, Clone)]
pub struct Settings {
    /// The runner image.
    pub runner_image: String,
    /// Memory the runner and the hypervisor take beside the guest, in MiB.
    pub overhead_mib: u32,
    /// Room in the runner's work directory for the input tar, in MiB; the
    /// output disk's size is added.
    pub work_mib: u32,
}

/// The label every runner pod carries, with its job's uid.
pub const JOB_LABEL: &str = "vmjob.voile.cx/job-uid";

const KERNEL: &str = "/vmjob/kernel";
const ROOTFS: &str = "/vmjob/rootfs";
const INPUT: &str = "/vmjob/input";
const OUTPUT: &str = "/vmjob/output";
const WORK: &str = "/vmjob/work";

/// The pod's name for a job: as much of the job's name as fits, then the
/// first characters of its uid, so two jobs never share a pod name and the
/// name stays a valid DNS label (at most 63 characters, alphanumeric at both
/// ends).
#[must_use]
pub fn name(job: &VmJob) -> String {
    let base = job.metadata.name.as_deref().unwrap_or("vmjob");
    let uid: String =
        job.metadata.uid.as_deref().unwrap_or("").chars().filter(char::is_ascii_alphanumeric).take(8).collect();
    let prefix: String = base.chars().take(42).collect();
    let prefix = prefix.trim_end_matches(|c: char| !c.is_ascii_alphanumeric());
    format!("{prefix}-{uid}-runner")
}

/// The runner pod: it boots the job's virtual machine, then stops. It runs as
/// an unprivileged user with no capabilities, a read-only root filesystem,
/// the runtime's seccomp profile and no service account token; `/dev/kvm`
/// comes from the [`KVM_RESOURCE`] device plugin, and the only volumes are the
/// two images, the job's two transfer volumes and its work directory.
///
/// # Errors
/// If the job has no name or namespace, or its configuration cannot be written.
pub fn build(job: &VmJob, settings: &Settings) -> anyhow::Result<Pod> {
    let spec = &job.spec;
    let owner = job.controller_owner_ref(&()).ok_or_else(|| anyhow::anyhow!("the job has no name or uid"))?;
    let output_bytes = spec.output.as_ref().map(|o| u64::from(o.max_mib) << 20);
    let config = RunnerConfig {
        kernel: format!("{KERNEL}/{}", spec.kernel.path),
        rootfs: format!("{ROOTFS}/{}", spec.rootfs.path),
        input: spec.input.as_ref().map(|_| INPUT.to_owned()),
        output: output_bytes.map(|bytes| (OUTPUT.to_owned(), bytes)),
        work: WORK.to_owned(),
        kernel_args: spec.kernel_args.clone(),
        job: GuestJob { command: spec.command.clone(), env: spec.env.clone(), output: spec.output.is_some() },
        cpus: spec.cpus,
        memory_mib: spec.memory_mib,
        timeout_seconds: spec.timeout_seconds,
    };

    let (volumes, mounts) = volumes(spec, settings, output_bytes);

    let memory = Quantity(format!("{}Mi", u64::from(spec.memory_mib) + u64::from(settings.overhead_mib)));
    let cpus = Quantity(spec.cpus.to_string());
    let resources: BTreeMap<String, Quantity> =
        [("cpu".to_owned(), cpus), ("memory".to_owned(), memory), (KVM_RESOURCE.to_owned(), Quantity("1".into()))]
            .into();

    let job_uid = job.metadata.uid.clone().unwrap_or_default();
    Ok(Pod {
        metadata: ObjectMeta {
            name: Some(name(job)),
            namespace: job.metadata.namespace.clone(),
            labels: Some([(JOB_LABEL.to_owned(), job_uid)].into()),
            owner_references: Some(vec![owner]),
            ..ObjectMeta::default()
        },
        spec: Some(PodSpec {
            restart_policy: Some("Never".into()),
            // The runner stops the virtual machine at its timeout; this only
            // catches a runner that hangs, with room for image pulls and the
            // output's unpacking.
            active_deadline_seconds: Some(i64::from(spec.timeout_seconds) + 3600),
            automount_service_account_token: Some(false),
            enable_service_links: Some(false),
            node_selector: (!spec.node_selector.is_empty()).then(|| spec.node_selector.clone()),
            security_context: Some(PodSecurityContext {
                run_as_non_root: Some(true),
                run_as_user: Some(65_532),
                run_as_group: Some(65_532),
                fs_group: Some(65_532),
                seccomp_profile: Some(SeccompProfile { type_: "RuntimeDefault".into(), localhost_profile: None }),
                ..PodSecurityContext::default()
            }),
            containers: vec![Container {
                name: "runner".into(),
                image: Some(settings.runner_image.clone()),
                env: Some(vec![EnvVar {
                    name: RUNNER_CONFIG.into(),
                    value: Some(serde_json::to_string(&config)?),
                    value_from: None,
                }]),
                resources: Some(ResourceRequirements {
                    limits: Some(resources.clone()),
                    requests: Some(resources),
                    ..ResourceRequirements::default()
                }),
                volume_mounts: Some(mounts),
                security_context: Some(SecurityContext {
                    allow_privilege_escalation: Some(false),
                    privileged: Some(false),
                    read_only_root_filesystem: Some(true),
                    capabilities: Some(Capabilities { drop: Some(vec!["ALL".into()]), add: None }),
                    ..SecurityContext::default()
                }),
                termination_message_policy: Some("File".into()),
                ..Container::default()
            }],
            volumes: Some(volumes),
            ..PodSpec::default()
        }),
        status: None,
    })
}

/// The pod's volumes and the runner's mounts of them: the two images
/// read-only, the input read-only, the output, and the work directory sized
/// for the output disk.
fn volumes(
    spec: &vmjob_api::VmJobSpec,
    settings: &Settings,
    output_bytes: Option<u64>,
) -> (Vec<Volume>, Vec<VolumeMount>) {
    let image = |name: &str, image: &str| Volume {
        name: name.to_owned(),
        image: Some(ImageVolumeSource { reference: Some(image.to_owned()), pull_policy: Some("IfNotPresent".into()) }),
        ..Volume::default()
    };
    let mut volumes = vec![image("kernel", &spec.kernel.image), image("rootfs", &spec.rootfs.image)];
    let mount = |name: &str, path: &str, read_only: bool, sub_path: &str| VolumeMount {
        name: name.to_owned(),
        mount_path: path.to_owned(),
        read_only: Some(read_only),
        sub_path: (!sub_path.is_empty()).then(|| sub_path.to_owned()),
        ..VolumeMount::default()
    };
    let mut mounts = vec![mount("kernel", KERNEL, true, ""), mount("rootfs", ROOTFS, true, "")];
    if let Some(input) = &spec.input {
        volumes.push(Volume { name: "input".into(), ..input.volume.clone() });
        mounts.push(mount("input", INPUT, true, &input.path));
    }
    if let Some(output) = &spec.output {
        volumes.push(Volume { name: "output".into(), ..output.to.volume.clone() });
        mounts.push(mount("output", OUTPUT, false, &output.to.path));
    }
    let work_mib = u64::from(settings.work_mib) + output_bytes.map_or(1, |b| b >> 20);
    volumes.push(Volume {
        name: "work".into(),
        empty_dir: Some(EmptyDirVolumeSource {
            size_limit: Some(Quantity(format!("{work_mib}Mi"))),
            ..EmptyDirVolumeSource::default()
        }),
        ..Volume::default()
    });
    mounts.push(mount("work", WORK, false, ""));
    (volumes, mounts)
}

/// What the end of a runner pod means for its job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    /// `Succeeded` or `Failed`.
    pub phase: Phase,
    /// The command's exit status, when it ran to the end.
    pub exit_code: Option<i32>,
    /// Why it failed.
    pub reason: Option<String>,
    /// The end of the console, or what Kubernetes said.
    pub message: Option<String>,
}

/// The job's outcome once its runner pod ended; `None` while it has not.
#[must_use]
pub fn outcome(pod: &Pod) -> Option<Outcome> {
    let status = pod.status.as_ref()?;
    let phase = status.phase.as_deref()?;
    if phase != "Succeeded" && phase != "Failed" {
        return None;
    }
    let terminated = status
        .container_statuses
        .iter()
        .flatten()
        .find(|c| c.name == "runner")
        .and_then(|c| c.state.as_ref())
        .and_then(|s| s.terminated.as_ref());
    let result =
        terminated.and_then(|t| t.message.as_deref()).and_then(|m| serde_json::from_str::<RunnerResult>(m).ok());
    if let Some(result) = result {
        let ok = result.reason.is_none() && result.exit_code == Some(0);
        return Some(Outcome {
            phase: if ok { Phase::Succeeded } else { Phase::Failed },
            exit_code: result.exit_code,
            reason: result.reason,
            message: Some(result.message),
        });
    }
    let why = status.reason.clone().or_else(|| terminated.and_then(|t| t.reason.clone()));
    let reason = if why.as_deref() == Some("DeadlineExceeded") { "Timeout" } else { "RunnerFailed" };
    Some(Outcome {
        phase: Phase::Failed,
        exit_code: None,
        reason: Some(reason.into()),
        message: why.or_else(|| status.message.clone()),
    })
}

#[cfg(test)]
mod tests;

use k8s_openapi::api::core::v1::{
    ContainerState, ContainerStateTerminated, ContainerStatus, LocalObjectReference, PersistentVolumeClaimVolumeSource,
    PodStatus,
};
use vmjob_api::{ImageFile, Output, Transfer, VmJobSpec};

use super::*;

fn job() -> VmJob {
    let claim = |name: &str| Volume {
        name: "ignored".into(),
        persistent_volume_claim: Some(PersistentVolumeClaimVolumeSource { claim_name: name.into(), read_only: None }),
        ..Volume::default()
    };
    let mut job = VmJob::new(
        "build-1",
        VmJobSpec {
            kernel: ImageFile { image: "registry.example/kernel@sha256:00".into(), path: "vmlinux".into() },
            rootfs: ImageFile { image: "registry.example/rootfs@sha256:11".into(), path: "rootfs.ext4".into() },
            kernel_args: None,
            command: vec!["/usr/bin/make".into()],
            env: BTreeMap::new(),
            cpus: 4,
            memory_mib: 2048,
            input: Some(Transfer { volume: claim("jobs"), path: "1/in".into() }),
            output: Some(Output { to: Transfer { volume: claim("jobs"), path: "1/out".into() }, max_mib: 512 }),
            timeout_seconds: 600,
            ttl_seconds_after_finished: Some(60),
            node_selector: BTreeMap::new(),
            image_pull_secrets: vec![LocalObjectReference { name: "registry".into() }],
        },
    );
    job.metadata.namespace = Some("jobs".into());
    job.metadata.uid = Some("uid-1".into());
    job
}

fn settings() -> Settings {
    Settings { runner_image: "registry.example/runner:1".into(), overhead_mib: 256, work_mib: 4096 }
}

#[test]
fn the_runner_pod_is_unprivileged_and_owned_by_its_job() {
    let pod = build(&job(), &settings()).expect("pod");
    assert_eq!(pod.metadata.name.as_deref(), Some("build-1-uid1-runner"));
    assert_eq!(pod.metadata.labels.as_ref().and_then(|l| l.get(JOB_LABEL)).map(String::as_str), Some("uid-1"));
    let owner = &pod.metadata.owner_references.as_ref().expect("owner")[0];
    assert_eq!((owner.uid.as_str(), owner.controller), ("uid-1", Some(true)));
    let spec = pod.spec.as_ref().expect("spec");
    assert_eq!(spec.restart_policy.as_deref(), Some("Never"));
    assert_eq!(spec.automount_service_account_token, Some(false));
    assert_eq!(spec.image_pull_secrets.as_deref().map(|s| s[0].name.as_str()), Some("registry"));
    let security = spec.security_context.as_ref().expect("pod security");
    assert_eq!(security.run_as_non_root, Some(true));
    let container = &spec.containers[0];
    let sc = container.security_context.as_ref().expect("container security");
    assert_eq!(
        (sc.privileged, sc.allow_privilege_escalation, sc.read_only_root_filesystem),
        (Some(false), Some(false), Some(true))
    );
    assert_eq!(sc.capabilities.as_ref().and_then(|c| c.drop.clone()), Some(vec!["ALL".into()]));
    let limits = container.resources.as_ref().and_then(|r| r.limits.as_ref()).expect("limits");
    assert_eq!(limits["memory"].0, "2304Mi");
    assert_eq!(limits["cpu"].0, "4");
    assert_eq!(limits[KVM_RESOURCE].0, "1");
}

#[test]
fn the_volumes_are_the_images_the_transfers_and_the_work_directory() {
    let pod = build(&job(), &settings()).expect("pod");
    let spec = pod.spec.as_ref().expect("spec");
    let names: Vec<&str> = spec.volumes.iter().flatten().map(|v| v.name.as_str()).collect();
    assert_eq!(names, ["kernel", "rootfs", "input", "output", "work"]);
    let volumes = spec.volumes.as_ref().expect("volumes");
    assert_eq!(
        volumes[0].image.as_ref().and_then(|i| i.reference.as_deref()),
        Some("registry.example/kernel@sha256:00")
    );
    assert_eq!(volumes[4].empty_dir.as_ref().and_then(|e| e.size_limit.clone()).map(|q| q.0), Some("4608Mi".into()));
    let mounts = spec.containers[0].volume_mounts.as_ref().expect("mounts");
    let input = mounts.iter().find(|m| m.name == "input").expect("input");
    assert_eq!((input.read_only, input.sub_path.as_deref()), (Some(true), Some("1/in")));
    let config: RunnerConfig =
        serde_json::from_str(spec.containers[0].env.as_ref().expect("env")[0].value.as_deref().expect("value"))
            .expect("config");
    assert_eq!(config.kernel, "/vmjob/kernel/vmlinux");
    assert_eq!(config.output, Some(("/vmjob/output".into(), 512 << 20)));
    assert!(config.job.output);
}

fn ended(phase: &str, message: Option<&str>, reason: Option<&str>) -> Pod {
    Pod {
        status: Some(PodStatus {
            phase: Some(phase.into()),
            reason: reason.map(Into::into),
            container_statuses: Some(vec![ContainerStatus {
                name: "runner".into(),
                state: Some(ContainerState {
                    terminated: Some(ContainerStateTerminated {
                        message: message.map(Into::into),
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            }]),
            ..Default::default()
        }),
        ..Pod::default()
    }
}

#[test]
fn the_pod_end_becomes_the_job_outcome() {
    let ok = r#"{"exitCode":0,"reason":null,"message":"done"}"#;
    assert_eq!(outcome(&ended("Succeeded", Some(ok), None)).map(|o| o.phase), Some(Phase::Succeeded));
    let failed = r#"{"exitCode":2,"reason":"CommandFailed","message":"x"}"#;
    assert_eq!(
        outcome(&ended("Succeeded", Some(failed), None)),
        Some(Outcome {
            phase: Phase::Failed,
            exit_code: Some(2),
            reason: Some("CommandFailed".into()),
            message: Some("x".into())
        })
    );
    let deadline = outcome(&ended("Failed", None, Some("DeadlineExceeded"))).expect("ended");
    assert_eq!((deadline.phase, deadline.reason.as_deref()), (Phase::Failed, Some("Timeout")));
    let garbage = outcome(&ended("Failed", Some("not json"), None)).expect("ended");
    assert_eq!(garbage.reason.as_deref(), Some("RunnerFailed"));
    assert_eq!(outcome(&ended("Running", None, None)), None);
}

#[test]
fn every_job_name_makes_a_distinct_valid_pod_name() {
    let mut long = job();
    long.metadata.name = Some(format!("{}.-x", "a".repeat(41)));
    let first = name(&long);
    assert!(first.len() <= 63 && first.ends_with("-runner"), "{first}");
    assert!(!first.contains(".-"), "{first}");
    long.metadata.uid = Some("uid-2".into());
    assert_ne!(name(&long), first);
    long.metadata.name = Some("b".repeat(250));
    assert!(name(&long).len() <= 63);
}

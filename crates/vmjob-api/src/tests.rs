use super::*;

fn spec() -> VmJobSpec {
    VmJobSpec {
        kernel: ImageFile { image: "registry.example/kernel:1".into(), path: "vmlinux".into() },
        rootfs: ImageFile { image: "registry.example/rootfs:1".into(), path: "rootfs.ext4".into() },
        kernel_args: None,
        command: vec!["/bin/true".into()],
        env: BTreeMap::new(),
        cpus: 2,
        memory_mib: 512,
        input: None,
        output: None,
        timeout_seconds: 60,
        ttl_seconds_after_finished: None,
        node_selector: BTreeMap::new(),
    }
}

#[test]
fn a_plain_spec_is_valid() {
    assert_eq!(spec().validate(), Ok(()));
}

/// A change to a valid spec, and the field its error names.
type Case = (&'static str, Box<dyn Fn(&mut VmJobSpec)>);

#[test]
fn bounds_and_paths_are_checked() {
    let cases: Vec<Case> = vec![
        ("command", Box::new(|s| s.command.clear())),
        ("cpus", Box::new(|s| s.cpus = 0)),
        ("cpus", Box::new(|s| s.cpus = 65)),
        ("memoryMib", Box::new(|s| s.memory_mib = 64)),
        ("timeoutSeconds", Box::new(|s| s.timeout_seconds = 0)),
        ("path", Box::new(|s| s.kernel.path = "/vmlinux".into())),
        ("path", Box::new(|s| s.rootfs.path = "a/../../etc".into())),
        ("path", Box::new(|s| s.rootfs.path = String::new())),
        (
            "path",
            Box::new(|s| {
                s.input = Some(Transfer { volume: Volume::default(), path: "../other".into() });
            }),
        ),
        (
            "maxMib",
            Box::new(|s| {
                s.output = Some(Output { to: Transfer { volume: Volume::default(), path: String::new() }, max_mib: 0 });
            }),
        ),
        ("kernelArgs", Box::new(|s| s.kernel_args = Some("init=/bin/sh\nquiet".into()))),
        ("env", Box::new(|s| _ = s.env.insert("A=B".into(), "c".into()))),
    ];
    for (field, change) in cases {
        let mut bad = spec();
        change(&mut bad);
        let error = bad.validate().expect_err(field);
        assert!(error.0.contains(field), "{field}: {error}");
    }
}

#[test]
fn a_long_message_is_cut_from_the_front_to_fit() {
    let result = RunnerResult {
        exit_code: Some(1),
        reason: Some("CommandFailed".into()),
        message: format!("{}the end", "é".repeat(10_000)),
    };
    let text = result.to_termination_message();
    assert!(text.len() <= RESULT_MAX_BYTES);
    let back: RunnerResult = serde_json::from_str(&text).expect("json");
    assert!(back.message.ends_with("the end"));
    assert_eq!(back.exit_code, Some(1));
}

#[test]
fn the_crd_names_the_group_and_status() {
    use kube::CustomResourceExt as _;
    let crd = VmJob::crd();
    assert_eq!(crd.spec.group, GROUP);
    assert_eq!(crd.spec.names.kind, "VmJob");
    let version = &crd.spec.versions[0];
    assert!(version.subresources.as_ref().and_then(|s| s.status.as_ref()).is_some());
}

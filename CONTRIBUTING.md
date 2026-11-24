# Contributing

Issues and pull requests are welcome. For anything larger than a fix, open an
issue first so we can agree on the shape.

## Building and testing

```sh
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

`deploy/base/crd.json` is generated; after changing `crates/vmjob-api`:

```sh
cargo run -q -p vmjob-operator -- crd > deploy/base/crd.json
```

### Real virtual machines

`crates/vmjob-runner/tests/vm.rs` boots VMs through the runner. It is skipped
unless these name a Cloud Hypervisor binary, a guest kernel and a root
filesystem with `vmjob-init` and busybox (see the `vm` job in
`.github/workflows/ci.yml`, which builds them):

```sh
export VMJOB_TEST_HYPERVISOR=/path/to/cloud-hypervisor
export VMJOB_TEST_KERNEL=/path/to/vmlinux
export VMJOB_TEST_ROOTFS=/path/to/rootfs.ext4
cargo test -p vmjob-runner --test vm
```

## Code

- Rust, edition 2024; `unsafe` is denied, `unwrap`/`expect` outside tests too.
- Anything the guest writes is hostile input: keep `crates/vmjob-runner/src/disks.rs`
  refusing by default, and add a test for every case it refuses.
- Commits say what changed and why, in the imperative.

By contributing you agree that your contributions are licensed under Apache-2.0.

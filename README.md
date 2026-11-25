# vmjob

[![ci](https://github.com/voilecx/vmjob/actions/workflows/ci.yml/badge.svg)](https://github.com/voilecx/vmjob/actions/workflows/ci.yml)

**vmjob** is a Kubernetes operator that runs one command in a disposable
[Cloud Hypervisor](https://www.cloudhypervisor.org/) virtual machine with **no
network device**. Files go in and come out on disks; the job's status says how
it ended.

It is meant for one-shot work you would not run in a plain container: building
untrusted code, transforming or signing artifacts, anything where a shared
kernel is not isolation enough and a network is one way out too many.

```yaml
apiVersion: vmjob.voile.cx/v1alpha1
kind: VmJob
metadata:
  name: hello
spec:
  kernel: { image: ghcr.io/example/guest@sha256:..., path: vmlinux }
  rootfs: { image: ghcr.io/example/guest@sha256:..., path: rootfs.ext4 }
  command: [/bin/sh, -c, "uname -a > out/uname.txt"]
  cpus: 1
  memoryMib: 512
  timeoutSeconds: 120
  output:
    volume: { name: results, persistentVolumeClaim: { claimName: results } }
    path: hello
    maxMib: 16
  ttlSecondsAfterFinished: 3600
```

```console
$ kubectl get vmj
NAME    PHASE       EXIT   AGE
hello   Succeeded   0      12s
```

## Features

- **No network.** The VM gets a root disk, an input disk and an output disk,
  plus up to four disk files the job names (`disks`: a cache a later job of
  the same owner reattaches, say), and nothing else; its console goes to the
  runner pod's log.
- **Unprivileged runners.** Runner pods meet the `restricted` Pod Security
  Standard. `/dev/kvm` comes from a small device plugin.
- **Untrusted output, handled as such.** Only regular files and directories
  below the destination are accepted, resolved beneath it. A refused output
  leaves nothing behind.
- **Guest images are plain OCI images**, mounted as Kubernetes image volumes:
  a kernel file and an ext4 file with [`vmjob-init`](docs/guest-images.md) as
  its init.
- **Bounded everything:** CPUs, memory, run time, output size, entry count.

## Install

Requirements: Kubernetes 1.35+ (image volumes), nodes with `/dev/kvm` (in a
VM: nested virtualization).

```sh
kubectl apply --server-side -k https://github.com/voilecx/vmjob/deploy/base?ref=main
```

This installs the CRD, the operator, the device plugin (namespace
`vmjob-system`) and a ValidatingAdmissionPolicy that holds the operator to
creating runner-shaped pods. Try [`examples/hello.yaml`](examples/hello.yaml).

## Documentation

- [Architecture](docs/architecture.md): the resource, the runner, the guest
  protocol, statuses and reasons.
- [Security model](docs/security.md): what is trusted, what is not, and why.
- [Guest images](docs/guest-images.md): building a kernel and root filesystem.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md). Security reports: [SECURITY.md](SECURITY.md).

## License

[Apache-2.0](LICENSE).

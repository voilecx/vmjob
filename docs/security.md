# Security model

## Trust

| Party | Trusted with |
|---|---|
| The guest (the command and anything it runs) | nothing |
| Whoever creates a `VmJob` | the same as creating a pod in that namespace |
| The operator | creating runner-shaped pods, writing job status |
| The device plugin | the kubelet's device-plugins directory, `/dev/kvm` |

## The guest

- It has **no network device**: Cloud Hypervisor is started without `--net`.
  Its root disk and its input disk are read-only. Its writes stay in memory.
- Its **only way out is the output disk.** The runner reads it as a tar and
  accepts regular files and directories at relative paths of at most 64
  components below the destination. Anything else refuses the whole output:
  links, devices, FIFOs, `..`, absolute paths, an entry written twice, a name
  the destination already has, more than 1 000 000 entries, more bytes than
  the disk, a GNU long-name or PAX header over 64 KiB. The headers are checked
  before the tar reader buffers any of them.
- Unpacking goes into a staging directory below the destination and is moved
  into place only when the whole tar was accepted, so a refused output leaves
  nothing. Every name is resolved beneath the destination with
  [`cap-std`](https://github.com/bytecodealliance/cap-std): a link that is
  there, however it got there, does not lead outside. Files are created new,
  never replaced, with mode 0644 or 0755.
- What the guest says about itself, its exit status and console, is reported
  as it says it; nothing acts on it.
- A job's own **disk files** (`disks`) are attached as raw block devices and
  never read by the runner: what a guest writes to a writable one reaches only
  the next guest that attaches the same file. Whoever creates the `VmJob`
  chooses that file, and with it which jobs share what one guest wrote. Each
  is mounted into the runner pod alone, by `subPath`, so the runner sees no
  other file of its volume; one that is not a non-empty regular file fails
  the job before boot.

## The runner

The runner pod meets the `restricted` Pod Security Standard: user 65532, no
privilege escalation, every capability dropped, a read-only root filesystem,
the `RuntimeDefault` seccomp profile, no service account token, no service
links. Cloud Hypervisor adds its own seccomp filters per thread. The pod gets
`/dev/kvm` through the device plugin and nothing else from the host.

## The operator

It may create pods in every namespace. A ValidatingAdmissionPolicy
(`deploy/base/admission.yaml`) holds that to the runner's shape: one container,
no host namespaces, no host paths, unprivileged with every capability dropped,
non-root, with the `vmjob.voile.cx/job-uid` label. A stolen operator token
cannot make a privileged pod. To keep it to some namespaces, set
`VMJOB_NAMESPACE` and bind its role per namespace.

A `VmJob` names volumes, which the runner mounts. That is why creating one is
as much as creating a pod: Pod Security Admission of the job's namespace
applies to its runner pod.

## The device plugin

It runs as root with no capability but `CHOWN` and no privilege escalation.
Its namespace is labeled `privileged` only because it mounts host paths: the
kubelet's device-plugins directory and `/dev/kvm`. At start it gives
`/dev/kvm` to the runner's group with mode 0660.

## Supply chain

Cloud Hypervisor is its upstream static release build, pinned by SHA-256 in
the `Dockerfile`; so is the test kernel in CI. Pin the guest images and the
runner image by digest. `cargo audit` runs in CI.

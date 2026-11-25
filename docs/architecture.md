# Architecture

```
VmJob ──operator──▶ runner pod (restricted Pod Security)
                     ├─ kernel, rootfs   image volumes, read-only
                     ├─ input, output    the job's volumes
                     ├─ disk-0..3        the job's disk files, one file each
                     ├─ work             emptyDir: the input tar, the output disk
                     └─ vmjob-runner ──▶ cloud-hypervisor (no --net)
                          /dev/vda  root filesystem, read-only
                          /dev/vdb  input tar, read-only
                          /dev/vdc  output disk
                          /dev/vdd… the job's disks, as they are
                          guest PID 1: vmjob-init
```

## Components

| Component | Runs as | Does |
|---|---|---|
| `vmjob-operator` | Deployment, `vmjob-system` | one runner pod per `VmJob`; the pod's end becomes the job's status |
| `vmjob-runner` | the runner pod, the job's namespace | makes the disks, boots the VM, enforces the timeout, unpacks the output |
| `vmjob-init` | PID 1 in the guest | unpacks the input, runs the command, packs the output, powers off |
| `vmjob-kvm-plugin` | DaemonSet, `vmjob-system` | the `vmjob.voile.cx/kvm` device: `/dev/kvm` for the runner |

## A job's life

1. The operator validates the spec (bounds, relative paths). An invalid job
   fails with `Invalid`.
2. It creates the runner pod `<name>-<uid>-runner`, owned by the job, and sets
   `phase: Pending`, then `Running` when the pod runs. A pod is created once; a
   pod that disappears before it ended fails the job with `PodLost`.
3. The runner writes `/dev/vdb`: a tar of the input directory with
   `.vmjob/job.json` (the command and environment) first. It creates
   `/dev/vdc`, `output.maxMib` of zeroes (1 MiB without an output). Each of
   `disks` is attached after it as it is (`/dev/vdd`, ...), read-only where
   `readOnly` says; the guest's command mounts or reads it itself.
4. Cloud Hypervisor boots the kernel with `root=/dev/vda ro
   init=/sbin/vmjob-init panic=1` plus `kernelArgs`. The guest console streams
   to the pod log.
5. `vmjob-init` mounts a tmpfs at `/j` (`vmjob.tmpfs=<size>`, default 75% of
   memory), unpacks `/dev/vdb` there, runs the command in `/j` with a clean
   environment (`PATH`, `HOME=/j`, `TMPDIR=/tmp`, then `env`), and writes a tar
   of `/j/out` and `.vmjob/exit` to `/dev/vdc`. If the output does not fit, it
   writes the exit status and `.vmjob/overflow` alone. Then it powers off.
6. The runner stops the VM at `timeoutSeconds` (`Timeout`) or when the guest
   asks to reboot, which is what a kernel panic does (`GuestCrashed`). After a
   power-off it unpacks `/dev/vdc` into the output directory and writes its
   result as the container's termination message.
7. The operator copies the result into the status. With
   `ttlSecondsAfterFinished`, the job and its pod are deleted that long after.

## Status

| Field | Meaning |
|---|---|
| `phase` | `Pending`, `Running`, `Succeeded` (exit 0), `Failed` |
| `exitCode` | the command's exit status, when it ran to the end; a signal is 128+n |
| `reason` | `CommandFailed`, `Timeout`, `GuestCrashed`, `OutputRefused`, `OutputTooLarge`, `RunnerFailed`, `PodLost`, `Invalid` |
| `message` | the end of the guest console, or the error |
| `pod`, `startTime`, `completionTime`, `observedGeneration` | |

## Operator settings

| Variable | Default | |
|---|---|---|
| `VMJOB_RUNNER_IMAGE` | (required) | the runner image; pin it by digest |
| `VMJOB_NAMESPACE` | all | watch one namespace |
| `VMJOB_OVERHEAD_MIB` | 256 | memory for the runner and the hypervisor beside the guest |
| `VMJOB_WORK_MIB` | 4096 | room for the input tar in the work directory |

## Device plugin settings

| Variable | Default | |
|---|---|---|
| `VMJOB_KVM_SLOTS` | 32 | units of `vmjob.voile.cx/kvm` per node |
| `VMJOB_KVM_GID` | 65532 | the group `/dev/kvm` is given to: the runner's |
| `VMJOB_KVM_MODE` | 0660 | its mode |

# Changelog

## Unreleased

- `disks`: up to four raw disk image files of a volume, attached after the
  output disk, writable or read-only. A writable one keeps what its guest
  wrote for the next job that attaches it.

## 0.1.0

First release.

- `VmJob` (`vmjob.voile.cx/v1alpha1`): one command in a disposable Cloud
  Hypervisor VM with no network device, files in and out on disks.
- Operator, runner (Cloud Hypervisor v55.0), guest init, `/dev/kvm` device
  plugin, ValidatingAdmissionPolicy for the operator's pods.
- `imagePullSecrets` for private guest images, `imagePullPolicy` for a guest
  image that follows a tag.

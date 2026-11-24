# Guest images

A job boots two files, each taken from an OCI image mounted as an image
volume: a kernel and an ext4 root filesystem. They may be in one image.

## The kernel

An uncompressed `vmlinux` (or a `bzImage`) with virtio-blk, ext4, tmpfs and
devtmpfs built in. Cloud Hypervisor's own builds work as they are:
<https://github.com/cloud-hypervisor/linux/releases>.

## The root filesystem

Any root filesystem with:

- `/sbin/vmjob-init`, statically linked; the `ghcr.io/voilecx/vmjob-init`
  image holds it at `/vmjob-init`;
- the empty directories `/dev`, `/proc`, `/sys`, `/tmp` and `/j`;
- whatever the command needs.

It is attached read-only; everything the guest writes goes to tmpfs.

```dockerfile
FROM ghcr.io/voilecx/vmjob-init:main AS init

FROM docker.io/library/debian:bookworm-slim AS root
RUN apt-get update && apt-get install -y --no-install-recommends build-essential \
    && rm -rf /var/lib/apt/lists/* && mkdir -p /j
COPY --from=init /vmjob-init /sbin/vmjob-init

FROM docker.io/library/debian:bookworm-slim AS image
RUN apt-get update && apt-get install -y --no-install-recommends e2fsprogs
COPY --from=root / /rootfs
RUN mkfs.ext4 -q -d /rootfs /rootfs.ext4 2G && rm -rf /rootfs

FROM scratch
COPY --from=image /rootfs.ext4 /rootfs.ext4
ADD --checksum=sha256:0d1d7ec2443f9a65fa1191ffb5ca2370934964405cece218b20796f0a4520b23 \
    https://github.com/cloud-hypervisor/linux/releases/download/ch-release-v6.16.9-20261001/vmlinux-x86_64 /vmlinux
```

```yaml
spec:
  kernel: { image: registry.example/guest@sha256:..., path: vmlinux }
  rootfs: { image: registry.example/guest@sha256:..., path: rootfs.ext4 }
```

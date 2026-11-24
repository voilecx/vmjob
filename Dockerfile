# syntax=docker/dockerfile:1.10
# Every image of vmjob, one target each:
#   docker build --target operator   -t vmjob-operator .
#   docker build --target runner     -t vmjob-runner .
#   docker build --target kvm-plugin -t vmjob-kvm-plugin .
#   docker build --target init       -t vmjob-init .   (FROM scratch: /vmjob-init, for guest images)

FROM docker.io/library/rust:1.100-bookworm AS build
RUN rustup target add x86_64-unknown-linux-musl \
    && apt-get update -q && apt-get install -yq --no-install-recommends musl-tools && rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked --bin vmjob-operator --bin vmjob-runner --bin vmjob-kvm-plugin \
    && cargo build --release --locked --target x86_64-unknown-linux-musl -p vmjob-init \
    && mkdir /out \
    && cp target/release/vmjob-operator target/release/vmjob-runner target/release/vmjob-kvm-plugin \
          target/x86_64-unknown-linux-musl/release/vmjob-init /out/

FROM gcr.io/distroless/cc-debian12:nonroot AS operator
COPY --from=build /out/vmjob-operator /usr/local/bin/vmjob-operator
ENTRYPOINT ["/usr/local/bin/vmjob-operator"]

FROM gcr.io/distroless/cc-debian12:nonroot AS runner
# Cloud Hypervisor's own static release build, pinned by its digest.
ADD --chmod=755 --checksum=sha256:ce7133bf535ed5a4e2a63ea1f26025445a5b1241f73ddfb81ff2fbcb2e803a59 \
    https://github.com/cloud-hypervisor/cloud-hypervisor/releases/download/v55.0/cloud-hypervisor-static \
    /usr/local/bin/cloud-hypervisor
COPY --from=build /out/vmjob-runner /usr/local/bin/vmjob-runner
ENTRYPOINT ["/usr/local/bin/vmjob-runner"]

FROM gcr.io/distroless/cc-debian12:nonroot AS kvm-plugin
COPY --from=build /out/vmjob-kvm-plugin /usr/local/bin/vmjob-kvm-plugin
USER 0
ENTRYPOINT ["/usr/local/bin/vmjob-kvm-plugin"]

FROM scratch AS init
COPY --from=build /out/vmjob-init /vmjob-init

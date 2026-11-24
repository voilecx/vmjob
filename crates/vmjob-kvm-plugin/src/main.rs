//! A kubelet device plugin for [`vmjob_api::KVM_RESOURCE`]: a container that
//! requests it gets the node's `/dev/kvm` and nothing else, so a vmjob runner
//! needs no privilege of its own.
//!
//! It runs as root in a `DaemonSet` with the kubelet's device-plugins
//! directory and `/dev/kvm`, with no capability but `CHOWN`: at start it gives
//! `/dev/kvm` to the runner's group (`VMJOB_KVM_GID`, default 65532) with mode
//! `VMJOB_KVM_MODE` (default `0660`), so the unprivileged runner can open it
//! and other users of the node cannot. The resource has `VMJOB_KVM_SLOTS` (default 32)
//! units, each the same device; the kubelet's own accounting bounds how many
//! runners share a node.
//!
//! When the kubelet restarts it removes the plugin's socket; the plugin sees
//! that and serves and registers again.

#[cfg(unix)]
mod plugin;

/// The generated API.
#[allow(missing_docs, clippy::pedantic)]
mod api {
    tonic::include_proto!("v1beta1");
}

#[cfg(unix)]
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .json()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();
    plugin::run(plugin::Settings::from_env()?).await
}

#[cfg(not(unix))]
fn main() -> anyhow::Result<()> {
    anyhow::bail!("the kvm device plugin runs on a Linux node")
}

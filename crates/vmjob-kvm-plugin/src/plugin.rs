//! Serving and registering the plugin on a Linux node.

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::time::Duration;

use anyhow::Context as _;
use tokio::net::{UnixListener, UnixStream};
use tokio_stream::Stream;
use tokio_stream::wrappers::{ReceiverStream, UnixListenerStream};
use tonic::{Request, Response, Status};

use crate::api::device_plugin_server::{DevicePlugin, DevicePluginServer};
use crate::api::registration_client::RegistrationClient;
use crate::api::{
    AllocateRequest, AllocateResponse, ContainerAllocateResponse, ContainerPreferredAllocationResponse, Device,
    DevicePluginOptions, DeviceSpec, Empty, ListAndWatchResponse, PreStartContainerRequest, PreStartContainerResponse,
    PreferredAllocationRequest, PreferredAllocationResponse, RegisterRequest,
};

/// The plugin's socket, in the kubelet's device-plugins directory.
const SOCKET: &str = "vmjob-kvm.sock";

/// Where and what the plugin serves.
#[derive(Debug, Clone)]
pub struct Settings {
    /// The kubelet's device-plugins directory.
    pub dir: PathBuf,
    /// The device handed out.
    pub device: PathBuf,
    /// Units of the resource.
    pub slots: usize,
    /// The device's mode, set at start.
    pub mode: u32,
    /// The device's group, set at start: the runner's.
    pub gid: u32,
}

impl Settings {
    /// From `VMJOB_DEVICE_PLUGIN_DIR`, `VMJOB_KVM_DEVICE`, `VMJOB_KVM_SLOTS`,
    /// `VMJOB_KVM_MODE` and `VMJOB_KVM_GID`.
    pub fn from_env() -> anyhow::Result<Self> {
        let var = |name: &str, default: &str| std::env::var(name).unwrap_or_else(|_| default.to_owned());
        Ok(Self {
            dir: var("VMJOB_DEVICE_PLUGIN_DIR", "/var/lib/kubelet/device-plugins").into(),
            device: var("VMJOB_KVM_DEVICE", "/dev/kvm").into(),
            slots: var("VMJOB_KVM_SLOTS", "32").parse().context("VMJOB_KVM_SLOTS")?,
            mode: u32::from_str_radix(var("VMJOB_KVM_MODE", "0660").trim_start_matches("0o"), 8)
                .context("VMJOB_KVM_MODE")?,
            gid: var("VMJOB_KVM_GID", "65532").parse().context("VMJOB_KVM_GID")?,
        })
    }
}

/// Serve and register until stopped; again each time the kubelet restarts.
pub async fn run(settings: Settings) -> anyhow::Result<()> {
    if settings.device.exists() {
        std::os::unix::fs::chown(&settings.device, None, Some(settings.gid))
            .with_context(|| format!("setting the group of {}", settings.device.display()))?;
        std::fs::set_permissions(&settings.device, std::fs::Permissions::from_mode(settings.mode))
            .with_context(|| format!("setting the mode of {}", settings.device.display()))?;
    } else {
        tracing::warn!(device = %settings.device.display(), "the device is missing; its units are reported unhealthy");
    }
    loop {
        let socket = settings.dir.join(SOCKET);
        let _ = std::fs::remove_file(&socket);
        let listener = UnixListener::bind(&socket).with_context(|| format!("binding {}", socket.display()))?;
        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        let plugin = Plugin { device: settings.device.clone(), slots: settings.slots };
        let server = tokio::spawn(
            tonic::transport::Server::builder()
                .add_service(DevicePluginServer::new(plugin))
                .serve_with_incoming_shutdown(UnixListenerStream::new(listener), async {
                    let _ = stopped.await;
                }),
        );
        register(&settings.dir.join("kubelet.sock")).await?;
        tracing::info!(resource = vmjob_api::KVM_RESOURCE, slots = settings.slots, "registered");
        while socket.exists() && !server.is_finished() {
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
        let _ = stop.send(());
        if let Ok(Err(error)) = server.await {
            tracing::warn!(%error, "the plugin server stopped");
        }
        tracing::warn!("the socket went away (a kubelet restart); serving and registering again");
    }
}

/// Register with the kubelet, retrying while it starts.
async fn register(kubelet: &Path) -> anyhow::Result<()> {
    let mut last = None;
    for attempt in 0..30u32 {
        if attempt > 0 {
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
        let path = kubelet.to_path_buf();
        let connected = tonic::transport::Endpoint::from_static("http://kubelet.invalid")
            .connect_with_connector(tower::service_fn(move |_| {
                let path = path.clone();
                async move { Ok::<_, std::io::Error>(hyper_util::rt::TokioIo::new(UnixStream::connect(path).await?)) }
            }))
            .await;
        let channel = match connected {
            Ok(channel) => channel,
            Err(error) => {
                last = Some(anyhow::Error::from(error));
                continue;
            }
        };
        let request = RegisterRequest {
            version: "v1beta1".into(),
            endpoint: SOCKET.into(),
            resource_name: vmjob_api::KVM_RESOURCE.into(),
            options: Some(DevicePluginOptions::default()),
        };
        match RegistrationClient::new(channel).register(request).await {
            Ok(_) => return Ok(()),
            Err(error) => last = Some(error.into()),
        }
    }
    Err(last
        .unwrap_or_else(|| anyhow::anyhow!("no attempt"))
        .context(format!("registering with {}", kubelet.display())))
}

struct Plugin {
    device: PathBuf,
    slots: usize,
}

impl Plugin {
    fn devices(&self) -> Vec<Device> {
        let health = if self.device.exists() { "Healthy" } else { "Unhealthy" };
        (0..self.slots).map(|i| Device { id: format!("kvm-{i}"), health: health.into(), topology: None }).collect()
    }
}

type DeviceStream = Pin<Box<dyn Stream<Item = Result<ListAndWatchResponse, Status>> + Send>>;

#[tonic::async_trait]
impl DevicePlugin for Plugin {
    type ListAndWatchStream = DeviceStream;

    async fn get_device_plugin_options(&self, _: Request<Empty>) -> Result<Response<DevicePluginOptions>, Status> {
        Ok(Response::new(DevicePluginOptions::default()))
    }

    async fn list_and_watch(&self, _: Request<Empty>) -> Result<Response<Self::ListAndWatchStream>, Status> {
        let (send, receive) = tokio::sync::mpsc::channel(4);
        let plugin = Self { device: self.device.clone(), slots: self.slots };
        tokio::spawn(async move {
            let mut last = None;
            loop {
                let devices = plugin.devices();
                let health = devices.first().map(|d| d.health.clone());
                if last != health {
                    if send.send(Ok(ListAndWatchResponse { devices })).await.is_err() {
                        return;
                    }
                    last = health;
                }
                tokio::time::sleep(Duration::from_secs(10)).await;
            }
        });
        Ok(Response::new(Box::pin(ReceiverStream::new(receive))))
    }

    async fn get_preferred_allocation(
        &self,
        request: Request<PreferredAllocationRequest>,
    ) -> Result<Response<PreferredAllocationResponse>, Status> {
        let container_responses = request
            .into_inner()
            .container_requests
            .into_iter()
            .map(|r| {
                let size = usize::try_from(r.allocation_size).unwrap_or(0);
                ContainerPreferredAllocationResponse {
                    device_i_ds: r.available_device_i_ds.into_iter().take(size).collect(),
                }
            })
            .collect();
        Ok(Response::new(PreferredAllocationResponse { container_responses }))
    }

    async fn allocate(&self, request: Request<AllocateRequest>) -> Result<Response<AllocateResponse>, Status> {
        let path = self.device.to_string_lossy().into_owned();
        let container_responses = request
            .into_inner()
            .container_requests
            .iter()
            .map(|_| ContainerAllocateResponse {
                devices: vec![DeviceSpec {
                    container_path: "/dev/kvm".into(),
                    host_path: path.clone(),
                    permissions: "rw".into(),
                }],
                ..ContainerAllocateResponse::default()
            })
            .collect();
        Ok(Response::new(AllocateResponse { container_responses }))
    }

    async fn pre_start_container(
        &self,
        _: Request<PreStartContainerRequest>,
    ) -> Result<Response<PreStartContainerResponse>, Status> {
        Ok(Response::new(PreStartContainerResponse {}))
    }
}

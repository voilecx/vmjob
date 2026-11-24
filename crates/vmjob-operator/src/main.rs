//! The vmjob operator: each `VmJob` becomes one runner pod, and the pod's end
//! becomes the job's status.
//!
//! Settings come from the environment: `VMJOB_RUNNER_IMAGE` (required),
//! `VMJOB_OVERHEAD_MIB` (default 256), `VMJOB_WORK_MIB` (room for the input
//! tar, default 4096) and `VMJOB_NAMESPACE` (watch one namespace; default all).
//!
//! A job's pod is created once and never recreated: a pod that disappears
//! before it ended fails the job with `PodLost`. A finished job with
//! `ttlSecondsAfterFinished` is deleted that long after it finished, its pod
//! with it (the pod is owned by the job).

use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt as _;
use k8s_openapi::api::core::v1::Pod;
use k8s_openapi::jiff::Timestamp;
use kube::api::{Api, DeleteParams, Patch, PatchParams, PostParams};
use kube::runtime::controller::{Action, Controller};
use kube::runtime::watcher;
use kube::{Client, ResourceExt as _};
use vmjob_api::{Phase, VmJob, VmJobStatus};

mod pod;

/// The field manager of the status this operator writes.
const MANAGER: &str = "vmjob-operator";

struct Context {
    client: Client,
    settings: pod::Settings,
}

#[derive(Debug)]
struct Error(anyhow::Error);

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:#}", self.0)
    }
}

impl std::error::Error for Error {}

impl From<anyhow::Error> for Error {
    fn from(error: anyhow::Error) -> Self {
        Self(error)
    }
}

impl From<kube::Error> for Error {
    fn from(error: kube::Error) -> Self {
        Self(error.into())
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // `vmjob-operator crd` prints the CustomResourceDefinition (JSON, which
    // kubectl takes as it takes YAML) and exits.
    if std::env::args().nth(1).as_deref() == Some("crd") {
        use kube::CustomResourceExt as _;
        println!("{}", serde_json::to_string_pretty(&VmJob::crd())?);
        return Ok(());
    }
    tracing_subscriber::fmt()
        .json()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();
    let number = |name: &str, default: u32| -> anyhow::Result<u32> {
        std::env::var(name).map_or(Ok(default), |v| v.parse().map_err(|e| anyhow::anyhow!("{name}: {e}")))
    };
    let settings = pod::Settings {
        runner_image: std::env::var("VMJOB_RUNNER_IMAGE")
            .map_err(|_| anyhow::anyhow!("VMJOB_RUNNER_IMAGE is required"))?,
        overhead_mib: number("VMJOB_OVERHEAD_MIB", 256)?,
        work_mib: number("VMJOB_WORK_MIB", 4096)?,
    };
    let client = Client::try_default().await?;
    let namespace = std::env::var("VMJOB_NAMESPACE").ok().filter(|n| !n.is_empty());
    let (jobs, pods): (Api<VmJob>, Api<Pod>) = match &namespace {
        Some(ns) => (Api::namespaced(client.clone(), ns), Api::namespaced(client.clone(), ns)),
        None => (Api::all(client.clone()), Api::all(client.clone())),
    };
    tracing::info!(namespace = namespace.as_deref().unwrap_or("*"), image = %settings.runner_image, "starting");
    let context = Arc::new(Context { client, settings });
    Controller::new(jobs, watcher::Config::default())
        .owns(pods, watcher::Config::default().labels(pod::JOB_LABEL))
        .shutdown_on_signal()
        .run(reconcile, on_error, context)
        .for_each(|result| async move {
            if let Err(error) = result {
                tracing::warn!(%error, "reconcile");
            }
        })
        .await;
    Ok(())
}

async fn reconcile(job: Arc<VmJob>, context: Arc<Context>) -> Result<Action, Error> {
    let namespace = job.namespace().ok_or_else(|| anyhow::anyhow!("a VmJob without a namespace"))?;
    let jobs: Api<VmJob> = Api::namespaced(context.client.clone(), &namespace);
    let pods: Api<Pod> = Api::namespaced(context.client.clone(), &namespace);
    let status = job.status.clone().unwrap_or_default();

    if status.phase.finished() {
        return finished(&job, &status, &jobs).await;
    }
    if let Err(invalid) = job.spec.validate() {
        return fail(&jobs, &job, status, "Invalid", invalid.0).await;
    }

    let name = pod::name(&job);
    let Some(pod) = pods.get_opt(&name).await? else {
        if status.pod.is_some() {
            return fail(&jobs, &job, status, "PodLost", format!("the runner pod {name} went away before it ended"))
                .await;
        }
        match pods.create(&PostParams::default(), &pod::build(&job, &context.settings)?).await {
            Ok(_) => {}
            // Another reconcile made it first: the next one reads it and checks its owner.
            Err(kube::Error::Api(e)) if e.code == 409 => return Ok(Action::requeue(Duration::from_secs(1))),
            // The API refused the pod as it is, and always will.
            Err(kube::Error::Api(e)) if e.code == 422 || e.code == 403 => {
                return fail(&jobs, &job, status, "Invalid", format!("the runner pod was refused: {}", e.message))
                    .await;
            }
            Err(e) => return Err(e.into()),
        }
        tracing::info!(job = %job.name_any(), %namespace, pod = %name, "runner pod created");
        let start_time = Some(Timestamp::now().to_string());
        write_status(&jobs, &job, VmJobStatus { phase: Phase::Pending, pod: Some(name), start_time, ..status }).await?;
        return Ok(Action::requeue(Duration::from_secs(300)));
    };

    if !pod.owner_references().iter().any(|o| Some(&o.uid) == job.metadata.uid.as_ref()) {
        return fail(&jobs, &job, status, "Invalid", format!("a pod {name} exists and is not this job's")).await;
    }
    if let Some(pod::Outcome { phase, exit_code, reason, message }) = pod::outcome(&pod) {
        tracing::info!(job = %job.name_any(), %namespace, ?phase, ?exit_code, ?reason, "finished");
        let completion_time = Some(Timestamp::now().to_string());
        let pod = Some(name);
        write_status(&jobs, &job, VmJobStatus { phase, exit_code, reason, message, pod, completion_time, ..status })
            .await?;
        return Ok(Action::await_change());
    }
    let running = pod.status.as_ref().and_then(|s| s.phase.as_deref()) == Some("Running");
    let phase = if running { Phase::Running } else { Phase::Pending };
    if status.phase != phase || status.pod.as_deref() != Some(name.as_str()) {
        write_status(&jobs, &job, VmJobStatus { phase, pod: Some(name), ..status }).await?;
    }
    Ok(Action::requeue(Duration::from_secs(300)))
}

/// End the job as failed, for `reason`.
async fn fail(
    jobs: &Api<VmJob>,
    job: &VmJob,
    status: VmJobStatus,
    reason: &str,
    message: String,
) -> Result<Action, Error> {
    let completion_time = Some(Timestamp::now().to_string());
    let reason = Some(reason.to_owned());
    write_status(
        jobs,
        job,
        VmJobStatus { phase: Phase::Failed, reason, message: Some(message), completion_time, ..status },
    )
    .await?;
    Ok(Action::await_change())
}

/// A finished job waits for its time to live, then goes.
async fn finished(job: &VmJob, status: &VmJobStatus, jobs: &Api<VmJob>) -> Result<Action, Error> {
    let Some(ttl) = job.spec.ttl_seconds_after_finished else { return Ok(Action::await_change()) };
    let Some(done) = status.completion_time.as_deref().and_then(|t| t.parse::<Timestamp>().ok()) else {
        return Ok(Action::await_change());
    };
    let age = Timestamp::now().duration_since(done).as_secs();
    let left = i64::from(ttl) - age;
    if left > 0 {
        return Ok(Action::requeue(Duration::from_secs(left.unsigned_abs())));
    }
    match jobs.delete(&job.name_any(), &DeleteParams::background()).await {
        Ok(_) => {}
        Err(kube::Error::Api(e)) if e.code == 404 => {}
        Err(e) => return Err(e.into()),
    }
    Ok(Action::await_change())
}

async fn write_status(jobs: &Api<VmJob>, job: &VmJob, mut status: VmJobStatus) -> Result<(), Error> {
    status.observed_generation = job.metadata.generation;
    let patch = serde_json::json!({ "status": status });
    jobs.patch_status(
        &job.name_any(),
        &PatchParams { field_manager: Some(MANAGER.into()), ..PatchParams::default() },
        &Patch::Merge(&patch),
    )
    .await?;
    Ok(())
}

fn on_error(_job: Arc<VmJob>, error: &Error, _context: Arc<Context>) -> Action {
    tracing::warn!(%error, "retrying");
    Action::requeue(Duration::from_secs(15))
}

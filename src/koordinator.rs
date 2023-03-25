use std::env;
use std::process::{exit, ExitStatus};
use std::time::Duration;
use duration_str::deserialize_duration;
use k8s_openapi::api::core::v1::Pod;
use kube::runtime::watcher::watch_object;
use serde::Deserialize;
use tokio::fs;
use tokio::process::Command;
use tokio::signal::unix::{signal, SignalKind};
use tokio::sync::mpsc;
use tokio_stream::StreamExt;
use tracing::{error, info, warn};

// Describes the ready and terminated containers of a pod.
#[derive(Debug, Default)]
struct ContainersStatus {
    ready_containers: Vec<String>,
    terminated_containers: Vec<String>,
}

// Defines all events that can be sent to the main loop from other asynchronous tasks.
#[derive(Debug)]
enum Event {
    // The child process exited on its own.
    ChildProcessExited(ExitStatus),

    // The child process was gracefully terminated by the koordinator.
    ChildProcessTerminatedBecauseStopDependency(ExitStatus),

    // The child process was forcefully killed by the koordinator.
    ChildProcessKilledBecauseStopDependency,

    // The pod's containers status changed.
    ContainersStatusChanged(ContainersStatus),
}

// Defines the "commands" that can be sent to the asynchronous task
// which spawns the child process.
#[derive(Debug)]
enum CommandToProcess {
    Start,
    Stop(i32, Duration),
}

// Describes the possible HTTP methods that can be used to send a request to the child process
// when one of the stop dependencies has exited.
#[derive(Deserialize, Debug, Clone)]
enum StopByStopDepHttp {
    #[serde(rename = "get")]
    Get(),
    #[serde(rename = "post")]
    Post(),
    #[serde(rename = "put")]
    Put(),
}

// The action that the koordinator will take once one of the stop dependencies has exited.
#[derive(Deserialize, Debug, Clone, Default)]
enum StopDepExitedAction {
    // The default is to send the `term_signal` to the child process.
    #[serde(skip_deserializing)]
    #[default]
    Default,

    // The koordinator will send the given signal to the child process.
    #[serde(rename = "signal")]
    Signal(i32),

    // The koordinator will send an HTTP request to the child process on 127.0.0.1
    // and a given port and path.
    #[serde(rename = "http")]
    Http(StopByStopDepHttp),
}

// This will be given in JSON form via the KOORDINATOR_SPEC environment variable
// to configure the koordinator.
#[derive(Deserialize, Debug, Clone)]
struct KoordinatorSpec {
    // A list of container names that the koordinator will wait for to be ready
    // before starting the child process.
    // This can be used to make sure that the main container in a pod only starts
    // once the sidecar container is ready.
    #[serde(default, rename = "startDeps")]
    start_deps: Vec<String>,

    // A list of container names that the koordinator will watch for termination
    // in order to stop the child process.
    // This can be used to make sure that a sidecar container in a job is terminated
    // after the main container has finished.
    #[serde(default, rename = "stopDeps")]
    stop_deps: Vec<String>,

    // The delay between the koordinator receiving the term_signal (e.g. by Kubernetes)
    // and sending the term_signal to the child process.
    // This should be used to give Kubernetes some time to stop sending new requests
    // to this process after initiating a pod termination.
    // After this duration passed, the term_signal will be sent to the child process.
    // See: https://learnk8s.io/graceful-shutdown
    #[serde(default, deserialize_with = "deserialize_duration", rename = "stopDelay")]
    stop_delay: Duration,

    // The action that the koordinator will take once one of the stop dependencies
    // has exited.
    // This can be either a signal that will be sent to the child process or an HTTP
    // request that will be sent to the child process on 127.0.0.1 and a given port and path.
    // The default is to send the `term_signal` to the child process.
    #[serde(default, rename = "whenStopDepExited")]
    when_stop_dep_exited: StopDepExitedAction,

    // The signal that the koordinator will recognize to initiate gracefully termination
    // of the the child process after the stop_delay has passed.
    // This is also the signal used to terminate the child process gracefully after one
    // of the stop dependencies exited.
    #[serde(default = "default_term_signal", rename = "termSignal")]
    term_signal: String,

    // The timeout after which the koordinator will forcefully kill the child process
    // if it has not terminated after receiving the term_signal due to a stop dependency
    // having exited.
    #[serde(default = "default_term_timeout", deserialize_with = "deserialize_duration", rename = "termTimeout")]
    term_timeout: Duration,

    // The exit code that the koordinator will exit with once its child
    // has been forcefully killed after failing to terminate using the term_signal
    // within the term_timeout due to a stop dependency exiting.
    #[serde(default = "killed_by_stop_dep_exit_code", rename = "killedByStopDepExitCode")]
    killed_by_stop_dep_exit_code: u32,

    // The exit code that the koordinator will exit with once its child
    // has been successfully terminated/stopped using the configured term_signal
    // within the term_timeout after one of the stop dependencies has exited.
    // Use a negative value to indicate that the original child exit code should be used.
    #[serde(default = "terminated_by_stop_dep_exit_code", rename = "terminatedByStopDepExitCode")]
    terminated_by_stop_dep_exit_code: i32,
}

// All configuration settings that are loaded from environment variables.
#[derive(Debug)]
struct EnvConfig {
    pod_name: Option<String>,
    pod_namespace: Option<String>,
    spec: String,
}

// The default exit code that the koordinator will exit with once its child
// has been forcefully killed after failing to terminate using the term_signal
// within the term_timeout due to a stop dependency exiting.
fn killed_by_stop_dep_exit_code() -> u32 {
    128u32 + libc::SIGKILL as u32
}

// The default exit code that the koordinator will exit with once its child
// has been successfully terminated/stopped using the configured term_signal
// within the term_timeout after one of the stop dependencies has exited.
// This is usually 0 in order to report a successful termination of a sidecar
// container in the context of a Kubernetes job.
fn terminated_by_stop_dep_exit_code() -> i32 {
    0
}

// The default signal that the koordinator will recognize to gracefully the
// child process after the stop_delay has passed.
// This is also the signal used to terminate the child process gracefully
// after one of the stop dependencies exited.
fn default_term_signal() -> String {
    "SIGTERM".to_string()
}

// The default timeout after which the koordinator will forcefully kill the
// child process if it has not terminated after receiving the term_signal due
// to a stop dependency having exited.
fn default_term_timeout() -> Duration {
    Duration::from_secs(30)
}

fn load_env_config() -> EnvConfig {
    EnvConfig {
        pod_name: env::var("KOORDINATOR_POD_NAME").ok(),
        pod_namespace: env::var("KOORDINATOR_POD_NAMESPACE").ok(),
        spec: env::var("KOORDINATOR_SPEC").unwrap_or_else(|_| String::from("{}")),
    }
}

fn listen_for_pod_status_changes(pod_namespace: String, pod_name: String, tx: mpsc::Sender<Event>) {
    tokio::spawn(async move {
        let client = kube::Client::try_default().await.unwrap();
        let pods: kube::Api<Pod> = kube::Api::namespaced(client, &pod_namespace);
        let objs = watch_object(pods, &pod_name);
        tokio::pin!(objs);
        while let Some(event) = objs.next().await {
            match event {
                Ok(Some(pod)) => {
                    tx.send(Event::ContainersStatusChanged(containers_status(&pod))).await.unwrap();
                }
                Err(e) => {
                    warn!("Error watching own pod \"{}\" in namespace \"{}\": {}", pod_name, pod_namespace, e);
                    break;
                }
                Ok(None) => {
                    warn!("Own pod \"{}\" in namespace \"{}\" was deleted", pod_name, pod_namespace);
                    break;
                }
            }
        }
    });
}

fn containers_status(p: &Pod) -> ContainersStatus {
    let st = p.status.as_ref().
        and_then(|s| s.container_statuses.as_ref());
    ContainersStatus {
        ready_containers: st.map(|cs| cs.iter().
            filter(|c| c.ready).
            map(|c| c.name.clone()).
            collect()
        ).
            unwrap_or(vec![]),
        terminated_containers: st.map(|cs| cs.iter().
            filter(|c| c.state.as_ref().
                map(|s| s.terminated.is_some()).
                unwrap_or(false)
            ).
            map(|c| c.name.clone()).
            collect()
        ).
            unwrap_or(vec![]),
    }
}

fn start_child_process(spec: KoordinatorSpec, cmd_and_args: Vec<String>, child_terminated_tx: mpsc::Sender<Event>, mut cmd_rx: mpsc::Receiver<CommandToProcess>) {
    tokio::spawn(async move {
        // wait for start command
        while let Some(cmd) = cmd_rx.recv().await {
            if let CommandToProcess::Start = cmd {
                break;
            }
        }

        // install signal handlers and start child process
        let (signal_tx, mut signal_rx) = mpsc::channel(1);
        spawn_signal_handlers(signal_tx);
        info!("Starting child process using: {:?}", cmd_and_args);
        let mut child = Command::new(cmd_and_args[0].clone())
            .args(cmd_and_args[1..].to_vec()).spawn().unwrap();
        if let Some(pid) = child.id() {
            info!("Started child process with pid: {}", pid);
        }

        let term_signal = signal_name_to_int(&spec.term_signal).unwrap_or(libc::SIGTERM);

        loop {
            tokio::select! {
                Some(cmd) = cmd_rx.recv() => {
                    if let CommandToProcess::Stop(signal, timeout) = cmd {
                        let signal_name = signal_int_to_name(signal).unwrap_or("<unknown>");
                        info!("Stopping child process with signal {} and timeout {:?}", signal_name, timeout);
                        if let Some(pid) = child.id() {
                            unsafe { libc::kill(pid as i32, signal); }
                        }
                        let wait_res = tokio::time::timeout(timeout, child.wait()).await;
                        if let Err(_) = wait_res {
                            warn!("Child process did not exit within timeout. Killing it with SIGKILL.");
                            if let Some(pid) = child.id() {
                                unsafe { libc::kill(pid as i32, libc::SIGKILL); }
                            }
                            let _ = child_terminated_tx.send(Event::ChildProcessKilledBecauseStopDependency).await;
                        } else {
                            match wait_res.unwrap() {
                                Ok(s) => {
                                    info!("Child process exited with status {}", s);
                                    let _ = child_terminated_tx.send(Event::ChildProcessTerminatedBecauseStopDependency(s)).await;
                                }
                                Err(e) => {
                                    panic!("Error while waiting for child process to exit: {}", e)
                                }
                            }
                        }
                        break;
                    }
                }
                Some(s) = signal_rx.recv() => {
                    let signal_name = signal_int_to_name(s).unwrap_or("<unknown>");
                    if s == term_signal {
                        info!("Received configured termination signal {} ({}). Waiting for {}s before forwarding to child.", term_signal, signal_name, spec.stop_delay.as_secs());
                        tokio::time::sleep(spec.stop_delay).await;
                    }
                    info!("Forwarding signal {} ({}) to child process", s, signal_name);
                    if let Some(pid) = child.id() {
                        unsafe { libc::kill(pid as i32, s); }
                    }
                }
                Ok(s) = child.wait() => {
                    info!("Child process exited with status {}", s);
                    let _ = child_terminated_tx.send(Event::ChildProcessExited(s)).await;
                    break;
                }
            }
        }
    });
}

fn spawn_signal_handlers(signal_tx: mpsc::Sender<i32>) {
    tokio::spawn(async move {
        let mut sigint = signal(SignalKind::interrupt()).unwrap();
        let mut sigterm = signal(SignalKind::terminate()).unwrap();
        let mut sigquit = signal(SignalKind::quit()).unwrap();
        let mut sighup = signal(SignalKind::hangup()).unwrap();
        let mut sigusr1 = signal(SignalKind::user_defined1()).unwrap();
        loop {
            tokio::select! {
                Some(_) = sigint.recv() => signal_tx.send(libc::SIGINT).await.unwrap_or(()),
                Some(_) = sigterm.recv() => signal_tx.send(libc::SIGTERM).await.unwrap_or(()),
                Some(_) = sigquit.recv() => signal_tx.send(libc::SIGQUIT).await.unwrap_or(()),
                Some(_) = sighup.recv() => signal_tx.send(libc::SIGHUP).await.unwrap_or(()),
                Some(_) = sigusr1.recv() => signal_tx.send(libc::SIGUSR1).await.unwrap_or(()),
            }
        }
    });
}

fn init_logging_for_gcp() {
    tracing_subscriber::fmt()
        .json()
        .flatten_event(true)
        .init();
}

fn signal_name_to_int(name: &str) -> Option<i32> {
    match name {
        "SIGINT" => Some(libc::SIGINT),
        "SIGTERM" => Some(libc::SIGTERM),
        "SIGQUIT" => Some(libc::SIGQUIT),
        "SIGHUP" => Some(libc::SIGHUP),
        "SIGUSR1" => Some(libc::SIGUSR1),
        _ => None,
    }
}

fn signal_int_to_name(sig: i32) -> Option<&'static str> {
    match sig {
        libc::SIGINT => Some("SIGINT"),
        libc::SIGTERM => Some("SIGTERM"),
        libc::SIGQUIT => Some("SIGQUIT"),
        libc::SIGHUP => Some("SIGHUP"),
        libc::SIGUSR1 => Some("SIGUSR1"),
        _ => None,
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    init_logging_for_gcp();
    let config = load_env_config();
    let spec = serde_json::from_str::<KoordinatorSpec>(&config.spec);
    if spec.is_err() {
        error!("Failed to parse KOORDINATOR_SPEC: {}", spec.err().unwrap());
        return;
    }
    let spec = spec.unwrap();
    info!("Loaded koordinator spec: {:?}", spec);
    let cmd_and_args: Vec<String> = env::args().skip(1).collect();
    if cmd_and_args.is_empty() {
        error!("No command specified that will be executed by the koordinator. You must specify the command and all of its arguments as command line arguments.");
        return;
    }
    let pod_name = determine_pod_name(&config).await;
    if pod_name.is_none() {
        error!("No pod name specified. You must specify the pod name either via the KOORDINATOR_POD_NAME environment variable or by mounting a downwardAPI volume into the container and setting the pod name in /etc/podinfo/name.");
        return;
    }
    let pod_name = pod_name.unwrap();
    let pod_namespace = determine_pod_namespace(&config).await;
    if pod_namespace.is_none() {
        error!("No pod namespace specified. You must specify the pod namespace either via the KOORDINATOR_POD_NAMESPACE environment variable or by mounting a downwardAPI volume into the container and setting the pod namespace in /etc/podinfo/namespace.");
        return;
    }
    let pod_namespace = pod_namespace.unwrap();
    info!("Will use own pod \"{}\" in namespace \"{}\".", pod_name, pod_namespace);

    if spec.start_deps.is_empty() {
        info!("No start dependencies configured. Will not wait for any other containers in this pod to become ready.");
    } else {
        info!("Will wait for containers {:?} in this pod to become ready before starting this container's process.", spec.start_deps);
    }
    if spec.stop_deps.is_empty() {
        info!("No stop dependencies configured. Will not terminate this container's process when any other container in this pod terminate.");
    } else {
        info!("Will listen for termination of any of the containers {:?} in this pod to terminate this container's process.", spec.stop_deps);
    }
    let term_signal = signal_name_to_int(&spec.term_signal).unwrap_or(libc::SIGTERM);
    let (event_tx, mut event_rx) = mpsc::channel::<Event>(1);
    let (cmd_tx, cmd_rx) = mpsc::channel::<CommandToProcess>(1);
    listen_for_pod_status_changes(pod_namespace.clone(), pod_name.clone(), event_tx.clone());
    start_child_process(spec.clone(), cmd_and_args.clone(), event_tx.clone(), cmd_rx);
    let mut started = false;
    let mut exit_status = 0;
    while let Some(message) = event_rx.recv().await {
        match message {
            Event::ChildProcessExited(status) => {
                exit_status = status.code().unwrap_or(0);
                break;
            }
            Event::ChildProcessTerminatedBecauseStopDependency(status) => {
                if spec.terminated_by_stop_dep_exit_code < 0 {
                    exit_status = status.code().unwrap_or(0);
                } else {
                    exit_status = spec.terminated_by_stop_dep_exit_code;
                }
                break;
            }
            Event::ChildProcessKilledBecauseStopDependency => {
                exit_status = spec.killed_by_stop_dep_exit_code as i32;
                break;
            }
            Event::ContainersStatusChanged(status) => {
                if !started {
                    if spec.start_deps.iter().all(|dep| status.ready_containers.contains(dep)) {
                        info!("All start dependencies are ready");
                        started = true;
                        cmd_tx.send(CommandToProcess::Start).await.unwrap_or(());
                    } else {
                        info!("Not all start dependencies are ready yet. Waiting...");
                    }
                } else {
                    let stopped_deps = spec.stop_deps.iter().filter(|dep| status.terminated_containers.contains(dep)).collect::<Vec<&String>>();
                    if !stopped_deps.is_empty() {
                        info!("Some stop dependencies are terminated: {:?}", stopped_deps);
                        match &spec.when_stop_dep_exited {
                            StopDepExitedAction::Default => {
                                cmd_tx.send(CommandToProcess::Stop(term_signal, spec.term_timeout)).await.unwrap_or(());
                            }
                            StopDepExitedAction::Signal(sig) => {
                                cmd_tx.send(CommandToProcess::Stop(sig.clone(), spec.term_timeout)).await.unwrap_or(());
                            }
                            StopDepExitedAction::Http(http) => {
                                unreachable!("HTTP stop dependency not implemented yet: {:?}", http);
                            }
                        }
                    }
                }
            }
        }
    }
    exit(exit_status);
}

// Try determining the pod name from either the environment variable KOORDINATOR_POD_NAME
// or a downwardAPI volume file /etc/podinfo/name.
async fn determine_pod_name(config: &EnvConfig) -> Option<String> {
    if let Some(pod_name) = &config.pod_name {
        Some(pod_name.clone())
    } else {
        fs::read_to_string("/etc/podinfo/name").await.ok()
    }
}

// Try determining the pod name from either the environment variable KOORDINATOR_POD_NAMESPACE
// or a downwardAPI volume file /etc/podinfo/namespace.
async fn determine_pod_namespace(config: &EnvConfig) -> Option<String> {
    if let Some(pod_namespace) = &config.pod_namespace {
        Some(pod_namespace.clone())
    } else {
        fs::read_to_string("/etc/podinfo/namespace").await.ok()
    }
}

use super::{KernelSession, KernelSpecification, RunningKernel, WslKernelSpecification};
use anyhow::{Context as _, Result};
use futures::{
    AsyncBufReadExt as _, SinkExt as _,
    channel::mpsc::{self},
    io::BufReader,
    stream::{FuturesUnordered, SelectAll, StreamExt},
};
use gpui::{App, AppContext as _, BackgroundExecutor, Entity, EntityId, Task, Window};
use jupyter_protocol::{
    ExecutionState, JupyterMessage, JupyterMessageContent, KernelInfoReply,
    connection_info::{ConnectionInfo, Transport},
};
use jupyter_websocket_client::KernelSpecsResponse;
use project::Fs;
use runtimelib::dirs;
use smol::net::TcpListener;
use std::{
    fmt::Debug,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::PathBuf,
    sync::Arc,
};
use uuid::Uuid;

// Find a set of open ports. This creates a listener with port set to 0. The listener will be closed at the end when it goes out of scope.
// There's a race condition between closing the ports and usage by a kernel, but it's inherent to the Jupyter protocol.
async fn peek_ports(ip: IpAddr) -> Result<[u16; 5]> {
    let mut addr_zeroport: SocketAddr = SocketAddr::new(ip, 0);
    addr_zeroport.set_port(0);
    let mut ports: [u16; 5] = [0; 5];
    for i in 0..5 {
        let listener = TcpListener::bind(addr_zeroport).await?;
        let addr = listener.local_addr()?;
        ports[i] = addr.port();
    }
    Ok(ports)
}

pub struct WslRunningKernel {
    pub process: smol::process::Child,
    connection_path: PathBuf,
    _process_status_task: Option<Task<()>>,
    pub working_directory: PathBuf,
    pub request_tx: mpsc::Sender<JupyterMessage>,
    pub execution_state: ExecutionState,
    pub kernel_info: Option<KernelInfoReply>,
}

impl Debug for WslRunningKernel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WslRunningKernel")
            .field("process", &self.process)
            .finish()
    }
}

impl WslRunningKernel {
    pub fn new<S: KernelSession + 'static>(
        kernel_specification: WslKernelSpecification,
        entity_id: EntityId,
        working_directory: PathBuf,
        fs: Arc<dyn Fs>,
        session: Entity<S>,
        window: &mut Window,
        cx: &mut App,
    ) -> Task<Result<Box<dyn RunningKernel>>> {
        window.spawn(cx, async move |cx| {
            let ip = IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1));
            let ports = peek_ports(ip).await?;
            log::info!("WSL kernel: picked ports: {:?}", ports);

            let connection_info = ConnectionInfo {
                transport: Transport::TCP,
                ip: ip.to_string(),
                stdin_port: ports[0],
                control_port: ports[1],
                hb_port: ports[2],
                shell_port: ports[3],
                iopub_port: ports[4],
                signature_scheme: "hmac-sha256".to_string(),
                key: uuid::Uuid::new_v4().to_string(),
                kernel_name: Some(format!("zed-wsl-{}", kernel_specification.name)),
            };

            let runtime_dir = dirs::runtime_dir();
            fs.create_dir(&runtime_dir)
                .await
                .with_context(|| format!("Failed to create jupyter runtime dir {runtime_dir:?}"))?;
            let connection_path = runtime_dir.join(format!("kernel-zed-wsl-{entity_id}.json"));
            let content = serde_json::to_string(&connection_info)?;
            fs.atomic_write(connection_path.clone(), content).await?;
            log::info!("WSL kernel: wrote connection file to {:?}", connection_path);

            // Convert connection_path to WSL path
            // We assume wslpath is available inside WSL, or we can run it from Windows against the distro.
            // running `wsl -d <distro> wslpath -u <windows_path>`
            let mut wslpath_cmd = util::command::new_smol_command("wsl");
            wslpath_cmd
                .arg("-d")
                .arg(&kernel_specification.distro)
                .arg("wslpath")
                .arg("-u")
                .arg(connection_path.to_string_lossy().to_string());

            let output = wslpath_cmd.output().await?;
            if !output.status.success() {
                anyhow::bail!("Failed to convert path to WSL path: {:?}", output);
            }
            let wsl_connection_path = String::from_utf8_lossy(&output.stdout).trim().to_string();
            log::info!(
                "WSL kernel: converted connection path to WSL path: {}",
                wsl_connection_path
            );

            // Construct the kernel command
            // The kernel spec argv might have absolute paths valid INSIDE WSL.
            // We need to run inside WSL.
            // `wsl -d <distro> --exec <argv0> <argv1> ...`
            // But we need to replace {connection_file} with wsl_connection_path.

            let argv = kernel_specification.kernelspec.argv;
            anyhow::ensure!(
                !argv.is_empty(),
                "Empty argv in kernelspec {}",
                kernel_specification.name
            );

            // Convert working dir
            let mut wslpath_wd_cmd = util::command::new_smol_command("wsl");
            wslpath_wd_cmd
                .arg("-d")
                .arg(&kernel_specification.distro)
                .arg("wslpath")
                .arg("-u")
                .arg(working_directory.to_string_lossy().to_string());

            let wd_output = wslpath_wd_cmd.output().await;
            let wsl_working_directory = if let Ok(output) = wd_output {
                if output.status.success() {
                    Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
                } else {
                    None
                }
            } else {
                None
            };
            log::info!(
                "WSL kernel: converted working directory to WSL path: {:?}",
                wsl_working_directory
            );

            let mut cmd = util::command::new_smol_command("wsl");
            cmd.arg("-d").arg(&kernel_specification.distro);

            if let Some(wd) = wsl_working_directory.as_ref() {
                cmd.arg("--cd").arg(wd);
            }

            cmd.arg("--exec");

            if let Some(env) = &kernel_specification.kernelspec.env {
                cmd.arg("env");
                for (k, v) in env {
                    cmd.arg(format!("{}={}", k, v));
                }
            }

            for arg in argv {
                if arg == "{connection_file}" {
                    cmd.arg(&wsl_connection_path);
                } else {
                    cmd.arg(arg);
                }
            }

            log::info!("WSL kernel: spawning command: {:?}", cmd);

            let mut process = cmd
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .stdin(std::process::Stdio::piped())
                .kill_on_drop(true)
                .spawn()
                .context("failed to start the kernel process")?;

            let session_id = Uuid::new_v4().to_string();

            log::info!("WSL kernel: creating client iopub connection");
            let mut iopub_socket =
                runtimelib::create_client_iopub_connection(&connection_info, "", &session_id)
                    .await?;
            log::info!("WSL kernel: creating client shell connection");
            let mut shell_socket =
                runtimelib::create_client_shell_connection(&connection_info, &session_id).await?;
            log::info!("WSL kernel: creating client control connection");
            let mut control_socket =
                runtimelib::create_client_control_connection(&connection_info, &session_id).await?;
            log::info!("WSL kernel: client connections created");

            let (request_tx, mut request_rx) =
                futures::channel::mpsc::channel::<JupyterMessage>(100);

            let (mut control_reply_tx, control_reply_rx) = futures::channel::mpsc::channel(100);
            let (mut shell_reply_tx, shell_reply_rx) = futures::channel::mpsc::channel(100);

            let mut messages_rx = SelectAll::new();
            messages_rx.push(control_reply_rx);
            messages_rx.push(shell_reply_rx);

            cx.spawn({
                let session = session.clone();

                async move |cx| {
                    while let Some(message) = messages_rx.next().await {
                        session
                            .update_in(cx, |session, window, cx| {
                                session.route(&message, window, cx);
                            })
                            .ok();
                    }
                }
            })
            .detach();

            // iopub task
            let iopub_task = cx.spawn({
                let session = session.clone();

                async move |cx| -> anyhow::Result<()> {
                    loop {
                        let message = iopub_socket.read().await?;
                        session
                            .update_in(cx, |session, window, cx| {
                                session.route(&message, window, cx);
                            })
                            .ok();
                    }
                }
            });

            let (mut control_request_tx, mut control_request_rx) =
                futures::channel::mpsc::channel(100);
            let (mut shell_request_tx, mut shell_request_rx) = futures::channel::mpsc::channel(100);

            let routing_task = cx.background_spawn({
                async move {
                    while let Some(message) = request_rx.next().await {
                        match message.content {
                            JupyterMessageContent::DebugRequest(_)
                            | JupyterMessageContent::InterruptRequest(_)
                            | JupyterMessageContent::ShutdownRequest(_) => {
                                control_request_tx.send(message).await?;
                            }
                            _ => {
                                shell_request_tx.send(message).await?;
                            }
                        }
                    }
                    anyhow::Ok(())
                }
            });

            let shell_task = cx.background_spawn({
                async move {
                    while let Some(message) = shell_request_rx.next().await {
                        shell_socket.send(message).await.ok();
                        let reply = shell_socket.read().await?;
                        shell_reply_tx.send(reply).await?;
                    }
                    anyhow::Ok(())
                }
            });

            let control_task = cx.background_spawn({
                async move {
                    while let Some(message) = control_request_rx.next().await {
                        control_socket.send(message).await.ok();
                        let reply = control_socket.read().await?;
                        control_reply_tx.send(reply).await?;
                    }
                    anyhow::Ok(())
                }
            });

            let stderr = process.stderr.take();

            cx.spawn(async move |_cx| {
                if stderr.is_none() {
                    return;
                }
                let reader = BufReader::new(stderr.unwrap());
                let mut lines = reader.lines();
                while let Some(Ok(line)) = lines.next().await {
                    log::error!("kernel: {}", line);
                }
            })
            .detach();

            let stdout = process.stdout.take();

            cx.spawn(async move |_cx| {
                if stdout.is_none() {
                    return;
                }
                let reader = BufReader::new(stdout.unwrap());
                let mut lines = reader.lines();
                while let Some(Ok(line)) = lines.next().await {
                    log::info!("kernel: {}", line);
                }
            })
            .detach();

            cx.spawn({
                let session = session.clone();
                async move |cx| {
                    async fn with_name(
                        name: &'static str,
                        task: Task<Result<()>>,
                    ) -> (&'static str, Result<()>) {
                        (name, task.await)
                    }

                    let mut tasks = FuturesUnordered::new();
                    tasks.push(with_name("iopub task", iopub_task));
                    tasks.push(with_name("shell task", shell_task));
                    tasks.push(with_name("control task", control_task));
                    tasks.push(with_name("routing task", routing_task));

                    while let Some((name, result)) = tasks.next().await {
                        if let Err(err) = result {
                            log::error!("kernel: handling failed for {name}: {err:?}");

                            session.update(cx, |session, cx| {
                                session.kernel_errored(
                                    format!("handling failed for {name}: {err}"),
                                    cx,
                                );
                                cx.notify();
                            });
                        }
                    }
                }
            })
            .detach();

            let status = process.status();

            let process_status_task = cx.spawn(async move |cx| {
                let error_message = match status.await {
                    Ok(status) => {
                        if status.success() {
                            log::info!("kernel process exited successfully");
                            return;
                        }

                        format!("kernel process exited with status: {:?}", status)
                    }
                    Err(err) => {
                        format!("kernel process exited with error: {:?}", err)
                    }
                };

                log::error!("{}", error_message);

                session.update(cx, |session, cx| {
                    session.kernel_errored(error_message, cx);

                    cx.notify();
                });
            });

            anyhow::Ok(Box::new(Self {
                process,
                request_tx,
                working_directory,
                _process_status_task: Some(process_status_task),
                connection_path,
                execution_state: ExecutionState::Idle,
                kernel_info: None,
            }) as Box<dyn RunningKernel>)
        })
    }
}

impl RunningKernel for WslRunningKernel {
    fn request_tx(&self) -> mpsc::Sender<JupyterMessage> {
        self.request_tx.clone()
    }

    fn working_directory(&self) -> &PathBuf {
        &self.working_directory
    }

    fn execution_state(&self) -> &ExecutionState {
        &self.execution_state
    }

    fn set_execution_state(&mut self, state: ExecutionState) {
        self.execution_state = state;
    }

    fn kernel_info(&self) -> Option<&KernelInfoReply> {
        self.kernel_info.as_ref()
    }

    fn set_kernel_info(&mut self, info: KernelInfoReply) {
        self.kernel_info = Some(info);
    }

    fn force_shutdown(&mut self, _window: &mut Window, _cx: &mut App) -> Task<anyhow::Result<()>> {
        self._process_status_task.take();
        self.request_tx.close_channel();
        Task::ready(self.process.kill().context("killing the kernel process"))
    }
}

impl Drop for WslRunningKernel {
    fn drop(&mut self) {
        std::fs::remove_file(&self.connection_path).ok();
        self.request_tx.close_channel();
        self.process.kill().ok();
    }
}

pub async fn wsl_kernel_specifications(
    background_executor: BackgroundExecutor,
) -> Result<Vec<KernelSpecification>> {
    let output = util::command::new_smol_command("wsl")
        .arg("-l")
        .arg("-q")
        .output()
        .await;

    if output.is_err() {
        return Ok(Vec::new());
    }

    let output = output.unwrap();
    if !output.status.success() {
        return Ok(Vec::new());
    }

    // wsl output is often UTF-16LE, but -l -q might be simpler or just ASCII compatible if not using weird charsets.
    // However, on Windows, wsl often outputs UTF-16LE.
    // We can try to detect or use from_utf16 if valid, or just use String::from_utf8_lossy and see.
    // Actually, `smol::process` on Windows might receive bytes that are UTF-16LE if wsl writes that.
    // But typically terminal output for wsl is UTF-16.
    // Let's try to parse as UTF-16LE if it looks like it (BOM or just 00 bytes).

    let stdout = output.stdout;
    let distros_str = if stdout.len() >= 2 && stdout[1] == 0 {
        // likely UTF-16LE
        let u16s: Vec<u16> = stdout
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        String::from_utf16_lossy(&u16s)
    } else {
        String::from_utf8_lossy(&stdout).to_string()
    };

    let distros: Vec<String> = distros_str
        .lines()
        .map(|line| line.trim().to_string())
        .filter(|line| !line.is_empty())
        .collect();

    let tasks = distros.into_iter().map(|distro| {
        background_executor.spawn(async move {
            let output = util::command::new_smol_command("wsl")
                .arg("-d")
                .arg(&distro)
                .arg("jupyter")
                .arg("kernelspec")
                .arg("list")
                .arg("--json")
                .output()
                .await;

            if let Ok(output) = output {
                if output.status.success() {
                    let json_str = String::from_utf8_lossy(&output.stdout);
                    if let Ok(specs_response) =
                        serde_json::from_str::<KernelSpecsResponse>(&json_str)
                    {
                        return specs_response
                            .kernelspecs
                            .into_iter()
                            .map(|(name, spec)| {
                                KernelSpecification::WslRemote(WslKernelSpecification {
                                    name,
                                    kernelspec: spec.spec,
                                    distro: distro.clone(),
                                })
                            })
                            .collect::<Vec<_>>();
                    }
                }
            }

            Vec::new()
        })
    });

    let specs = futures::future::join_all(tasks)
        .await
        .into_iter()
        .flatten()
        .collect();

    Ok(specs)
}

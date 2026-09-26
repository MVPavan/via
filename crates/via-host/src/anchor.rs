//! Internal same-binary anchor. It alone may signal its current process group.

use std::{
    ffi::OsString, fs, io, os::unix::fs::PermissionsExt, path::Path, process::Stdio, time::Duration,
};

use rustix::process::{self, Signal};
use tokio::{
    net::{UnixListener, UnixStream},
    process::Child,
    signal::unix::{SignalKind, signal},
    time::{Instant, interval},
};

use crate::{
    linux,
    protocol::{self, Bootstrap, Reply, Request, VendorConfig, WireIdentity},
};

/// Runs the private same-binary anchor entrypoint from one bootstrap path.
pub fn run_anchor_from_args(args: &[OsString]) -> i32 {
    let [config_path] = args else {
        return 2;
    };
    let path = Path::new(config_path);
    let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    else {
        return 1;
    };
    match runtime.block_on(run(path)) {
        Ok(()) => 0,
        Err(_) => 1,
    }
}

async fn run(path: &Path) -> io::Result<()> {
    linux::secure_directory(
        path.parent()
            .ok_or_else(|| io::Error::other("missing anchor directory"))?,
    )?;
    let bytes = fs::read(path)
        .map_err(|error| io::Error::new(error.kind(), format!("read bootstrap: {error}")))?;
    fs::remove_file(path)
        .map_err(|error| io::Error::new(error.kind(), format!("remove bootstrap: {error}")))?;
    let bootstrap: Bootstrap = serde_json::from_slice(&bytes).map_err(io::Error::other)?;
    if bootstrap.marker.len() != 32 || bootstrap.generation.len() != 32 {
        return Err(io::Error::other("invalid bootstrap"));
    }
    let mut terminate = signal(SignalKind::terminate())
        .map_err(|error| io::Error::new(error.kind(), format!("signal: {error}")))?;
    let listener = UnixListener::bind(&bootstrap.socket_path)
        .map_err(|error| io::Error::new(error.kind(), format!("bind: {error}")))?;
    fs::set_permissions(&bootstrap.socket_path, fs::Permissions::from_mode(0o600))
        .map_err(|error| io::Error::new(error.kind(), format!("chmod socket: {error}")))?;
    let result = serve(&listener, &bootstrap, &mut terminate)
        .await
        .map_err(|error| io::Error::new(error.kind(), format!("serve: {error}")));
    let _ = fs::remove_file(&bootstrap.socket_path);
    result
}

async fn serve(
    listener: &UnixListener,
    bootstrap: &Bootstrap,
    terminate: &mut tokio::signal::unix::Signal,
) -> io::Result<()> {
    let bootstrap_deadline = Instant::now() + Duration::from_secs(5);
    let process_id = std::process::id();
    let (group_id, start_ticks) = linux::process_stat(process_id)?;
    if group_id != process_id {
        return Err(io::Error::other("anchor is not group leader"));
    }
    let identity = WireIdentity {
        pid: process_id,
        pgid: group_id,
        uid: rustix::process::getuid().as_raw(),
        boot_id: linux::boot_id()?,
        pid_namespace: linux::pid_namespace()?,
        start_ticks,
        marker: bootstrap.marker.clone(),
    };
    let mut stream = loop {
        let (candidate, _) =
            tokio::time::timeout_at(bootstrap_deadline, listener.accept()).await??;
        if verify_peer(&candidate, identity.uid).is_ok()
            && candidate.peer_cred()?.pid() == i32::try_from(bootstrap.controller_pid).ok()
        {
            break candidate;
        }
    };
    protocol::write_frame(
        &mut stream,
        &Reply::Ready {
            identity: identity.clone(),
        },
        1024,
    )
    .await?;
    let mut configured: Option<VendorConfig> = None;
    loop {
        let request = tokio::select! {
            result = protocol::read_frame::<Request>(&mut stream, 65_536) => result?,
            () = tokio::time::sleep_until(bootstrap_deadline) => return Ok(()),
            _ = terminate.recv() => return Ok(()),
        };
        match request {
            Some(Request::Configure { vendor }) if configured.is_none() => {
                if !vendor.program().is_absolute() || !vendor.cwd().is_absolute() {
                    return Err(io::Error::other("vendor paths must be absolute"));
                }
                configured = Some(vendor);
                protocol::write_frame(&mut stream, &Reply::Configured, 1024).await?;
            }
            Some(Request::Arm { generation })
                if generation == bootstrap.generation && configured.is_some() =>
            {
                let Some(vendor) = configured.take() else {
                    return Err(io::Error::other("missing vendor configuration"));
                };
                return armed(listener, stream, bootstrap, vendor, terminate).await;
            }
            Some(Request::Challenge { nonce, proof })
                if nonce.len() <= 64
                    && proof == protocol::challenge_proof(&bootstrap.marker, &nonce) =>
            {
                protocol::write_frame(
                    &mut stream,
                    &Reply::Challenge {
                        nonce,
                        identity: identity.clone(),
                    },
                    1024,
                )
                .await?;
            }
            Some(Request::Status { generation }) if generation == bootstrap.generation => {
                protocol::write_frame(
                    &mut stream,
                    &Reply::Status {
                        pid: None,
                        exit_code: None,
                        exit_signal: None,
                    },
                    1024,
                )
                .await?;
            }
            None => return Ok(()),
            _ => return Err(io::Error::other("invalid pre-arm control")),
        }
    }
}

async fn armed(
    listener: &UnixListener,
    mut stream: UnixStream,
    bootstrap: &Bootstrap,
    vendor: VendorConfig,
    terminate: &mut tokio::signal::unix::Signal,
) -> io::Result<()> {
    let (mut child, vendor_pid) = spawn_vendor(&mut stream, vendor, terminate).await?;
    let mut poll = interval(Duration::from_millis(20));
    let mut exit = None;
    let mut controller = Some(stream);
    let mut reader = protocol::FrameReader::new(1024);
    let mut verified_connection = true;
    let mut kill_at: Option<Instant> = None;
    loop {
        tokio::select! {
            incoming = async {
                match controller.as_mut() {
                    Some(active) => reader.read::<Request>(active).await,
                    None => std::future::pending().await,
                }
            } => {
                let lost = match incoming {
                    Ok(Some(Request::Challenge { nonce, proof })) if nonce.len() <= 64 && proof == protocol::challenge_proof(&bootstrap.marker, &nonce) => {
                        let identity = current_identity(&bootstrap.marker)?;
                        let lost = match controller.as_mut() {
                            Some(active) => protocol::write_frame(active, &Reply::Challenge { nonce, identity }, 1024).await.is_err(),
                            None => true,
                        };
                        if !lost { verified_connection = true; }
                        lost
                    }
                    Ok(Some(Request::Status { generation })) if verified_connection && generation == bootstrap.generation => {
                        let (exit_code, exit_signal) = exit.map_or((None, None), |report: crate::ExitReport| (report.code, report.signal));
                        match controller.as_mut() {
                            Some(active) => protocol::write_frame(active, &Reply::Status { pid: Some(vendor_pid), exit_code, exit_signal }, 1024).await.is_err(),
                            None => true,
                        }
                    }
                    Ok(Some(Request::Stop { generation, deadline_monotonic_ns })) if verified_connection && generation == bootstrap.generation => {
                        let lost = match controller.as_mut() {
                            Some(active) => protocol::write_frame(active, &Reply::Stopping, 1024).await.is_err(),
                            None => true,
                        };
                        let grace = crate::monotonic_remaining(deadline_monotonic_ns).unwrap_or(Duration::ZERO);
                        begin_cleanup(&mut kill_at, grace.min(Duration::from_millis(200)));
                        lost
                    }
                    _ => true,
                };
                if lost {
                    controller = None;
                    reader.reset();
                    verified_connection = false;
                    begin_cleanup(&mut kill_at, Duration::from_millis(200));
                }
            }
            accepted = listener.accept(), if controller.is_none() => {
                // The bootstrap controller cannot be displaced. Reconnect is available
                // only after its EOF has already started own-group cleanup.
                if let Ok((candidate, _)) = accepted
                    && verify_peer(&candidate, rustix::process::getuid().as_raw()).is_ok() {
                    controller = Some(candidate);
                    reader.reset();
                    verified_connection = false;
                }
            }
            () = async {
                match kill_at {
                    Some(deadline) => tokio::time::sleep_until(deadline).await,
                    None => std::future::pending().await,
                }
            } => {
                let _ = process::kill_process_group(process::getpgrp(), Signal::KILL);
                return Ok(());
            }
            _ = poll.tick(), if exit.is_none() => {
                if let Some(status) = child.try_wait()? {
                    use std::os::unix::process::ExitStatusExt;
                    exit = Some(crate::ExitReport { code: status.code(), signal: status.signal() });
                }
            }
            _ = terminate.recv() => {
                begin_cleanup(&mut kill_at, Duration::from_millis(200));
            }
        }
    }
}

async fn spawn_vendor(
    stream: &mut UnixStream,
    vendor: VendorConfig,
    terminate: &mut tokio::signal::unix::Signal,
) -> io::Result<(Child, u32)> {
    let mut command = tokio::process::Command::new(vendor.program());
    command
        .args(vendor.args())
        .current_dir(vendor.cwd())
        .env_clear();
    for (key, value) in vendor.env() {
        command.env(key, value);
    }
    command
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    let spawn = command.spawn();
    let detach = detach_standard_streams();
    if detach.is_err() {
        let _ = protocol::write_frame(
            stream,
            &Reply::Error {
                code: "PipeDetachFailed".into(),
            },
            1024,
        )
        .await;
        stop_own_group(terminate, Duration::from_millis(200)).await;
        return Err(io::Error::other("PipeDetachFailed"));
    }
    let child = match spawn {
        Ok(child) => child,
        Err(error) => {
            let _ = protocol::write_frame(
                stream,
                &Reply::Error {
                    code: "VendorSpawnFailed".into(),
                },
                1024,
            )
            .await;
            return Err(error);
        }
    };
    let vendor_pid = child
        .id()
        .ok_or_else(|| io::Error::other("missing vendor pid"))?;
    if protocol::write_frame(stream, &Reply::Spawned { pid: vendor_pid }, 1024)
        .await
        .is_err()
    {
        stop_own_group(terminate, Duration::from_millis(200)).await;
        return Err(io::Error::new(
            io::ErrorKind::BrokenPipe,
            "spawn reply lost",
        ));
    }
    Ok((child, vendor_pid))
}

fn begin_cleanup(kill_at: &mut Option<Instant>, grace: Duration) {
    let proposed = Instant::now() + grace;
    if let Some(existing) = kill_at {
        *existing = (*existing).min(proposed);
    } else {
        // Current membership pins this group while the anchor issues TERM.
        let _ = process::kill_process_group(process::getpgrp(), Signal::TERM);
        *kill_at = Some(proposed);
    }
}

fn current_identity(marker: &str) -> io::Result<WireIdentity> {
    let process_id = std::process::id();
    let (group_id, start_ticks) = linux::process_stat(process_id)?;
    Ok(WireIdentity {
        pid: process_id,
        pgid: group_id,
        uid: rustix::process::getuid().as_raw(),
        boot_id: linux::boot_id()?,
        pid_namespace: linux::pid_namespace()?,
        start_ticks,
        marker: marker.to_owned(),
    })
}

fn verify_peer(stream: &UnixStream, uid: u32) -> io::Result<()> {
    if stream.peer_cred()?.uid() != uid {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "unexpected control peer",
        ));
    }
    Ok(())
}

fn detach_standard_streams() -> io::Result<()> {
    let null = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/null")?;
    rustix::stdio::dup2_stdin(&null)?;
    rustix::stdio::dup2_stdout(&null)?;
    rustix::stdio::dup2_stderr(&null)?;
    Ok(())
}

async fn stop_own_group(_terminate: &mut tokio::signal::unix::Signal, grace: Duration) {
    let group = process::getpgrp();
    // The anchor is still in this group. No daemon-supplied numeric group is signalled.
    let _ = process::kill_process_group(group, Signal::TERM);
    tokio::time::sleep(grace).await;
    let _ = process::kill_process_group(group, Signal::KILL);
}

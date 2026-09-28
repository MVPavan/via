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
    #[cfg(feature = "test-failpoints")]
    if let Some((dir, token)) = &bootstrap.failpoints {
        via_store::failpoint::activate(dir, token).map_err(io::Error::other)?;
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
                arm_received_seam().await;
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
            None => {
                eof_cleanup_seam().await;
                return Ok(());
            }
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
    // Set once cleanup begins: whether the vendor was then still live.
    let mut stopped_live: Option<bool> = None;
    // A Host `Stop` began the cleanup, not an EOF or `SIGTERM`: only then is
    // `stopped_live` Host force evidence, repeated on every later `Stop`.
    let mut stopped_by_host = false;
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
                        stop_received_seam().await;
                        // Cleanup begins before the reply, which reports its
                        // evidence; a test-deferred cleanup reports none.
                        let reply = if cleanup_deferred().await {
                            Reply::Stopping { stopped_live: false }
                        } else {
                            stopped_by_host |= kill_at.is_none();
                            let grace = crate::monotonic_remaining(deadline_monotonic_ns).unwrap_or(Duration::ZERO);
                            begin_cleanup(&mut kill_at, &mut stopped_live, &mut child, grace.min(Duration::from_millis(200)));
                            Reply::Stopping { stopped_live: stopped_by_host && stopped_live == Some(true) }
                        };
                        match controller.as_mut() {
                            // Runtime §11: the reply is lost; the stop still runs.
                            Some(_) if final_reply_lost().await => true,
                            Some(active) => protocol::write_frame(active, &reply, 1024).await.is_err(),
                            None => true,
                        }
                    }
                    _ => true,
                };
                if lost {
                    controller = None;
                    reader.reset();
                    verified_connection = false;
                    if kill_at.is_none() {
                        eof_cleanup_seam().await;
                    }
                    if !cleanup_deferred().await {
                        begin_cleanup(&mut kill_at, &mut stopped_live, &mut child, Duration::from_millis(200));
                    }
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
                begin_cleanup(&mut kill_at, &mut stopped_live, &mut child, Duration::from_millis(200));
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

/// Starts own-group cleanup once; a later call only shortens its grace.
/// `stopped_live` records whether the vendor was still live when the anchor
/// signalled its group: checked first, so a vendor that already exited, reaped
/// or not, was not stopped by this cleanup.
fn begin_cleanup(
    kill_at: &mut Option<Instant>,
    stopped_live: &mut Option<bool>,
    child: &mut Child,
    grace: Duration,
) {
    let proposed = Instant::now() + grace;
    if let Some(existing) = kill_at {
        *existing = (*existing).min(proposed);
    } else {
        *stopped_live = Some(matches!(child.try_wait(), Ok(None)));
        // Current membership pins this group while the anchor issues TERM.
        let _ = process::kill_process_group(process::getpgrp(), Signal::TERM);
        *kill_at = Some(proposed);
    }
}

/// Test-only `host.anchor.before_eof_cleanup`: a pause holds the anchor
/// before its EOF cleanup, the slow anchor exit of design §10 [r1.8].
#[cfg(feature = "test-failpoints")]
async fn eof_cleanup_seam() {
    let _ = via_store::failpoint::hit_async("host.anchor.before_eof_cleanup").await;
}

/// Release builds have no seam before the EOF cleanup.
#[cfg(not(feature = "test-failpoints"))]
#[expect(clippy::unused_async, reason = "test builds pause here")]
async fn eof_cleanup_seam() {}

/// Test-only `host.anchor.arm_received`: a pause holds a received ARM
/// before the vendor spawns, so ARM is in flight (design §11 [r6.1]).
#[cfg(feature = "test-failpoints")]
async fn arm_received_seam() {
    let _ = via_store::failpoint::hit_async("host.anchor.arm_received").await;
}

/// Release builds never hold an ARM.
#[cfg(not(feature = "test-failpoints"))]
#[expect(clippy::unused_async, reason = "test builds pause here")]
async fn arm_received_seam() {}

/// Test-only `host.anchor.stop_received`: a pause holds this anchor's
/// `Stop` before it is handled, keeping its control busy (design §11 [r6.5]).
#[cfg(feature = "test-failpoints")]
async fn stop_received_seam() {
    let _ = via_store::failpoint::hit_async("host.anchor.stop_received").await;
}

/// Release builds never hold a `Stop`.
#[cfg(not(feature = "test-failpoints"))]
#[expect(clippy::unused_async, reason = "test builds pause here")]
async fn stop_received_seam() {}

/// Test-only `host.anchor.defer_cleanup` (design §11 [r6.3]): while a
/// `fail_io` is armed (persistently, each trigger one occurrence), a `Stop`
/// or EOF starts no cleanup and a `Stop` reply withholds `stopped_live`;
/// the first trigger after the harness disarms it runs the cleanup.
#[cfg(feature = "test-failpoints")]
async fn cleanup_deferred() -> bool {
    via_store::failpoint::hit_async("host.anchor.defer_cleanup")
        .await
        .is_err()
}

/// Release builds never defer cleanup.
#[cfg(not(feature = "test-failpoints"))]
#[expect(clippy::unused_async, reason = "test builds can defer here")]
async fn cleanup_deferred() -> bool {
    false
}

/// Test-only `host.anchor.final_reply_lost` (runtime §11): true drops the
/// `Stop` reply; the cleanup it started continues.
#[cfg(feature = "test-failpoints")]
async fn final_reply_lost() -> bool {
    via_store::failpoint::hit_async("host.anchor.final_reply_lost")
        .await
        .is_err()
}

/// Release builds never lose a reply.
#[cfg(not(feature = "test-failpoints"))]
#[expect(clippy::unused_async, reason = "test builds can lose the reply here")]
async fn final_reply_lost() -> bool {
    false
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

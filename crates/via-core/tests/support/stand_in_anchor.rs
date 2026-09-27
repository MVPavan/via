//! A scripted stand-in for Host's private anchor. It speaks the real control
//! protocol (identity, Configure, ARM, status, stop) so a test can hold Host at
//! one step or make the anchor report chosen stop evidence. Host starts it with
//! an empty environment; it writes barrier flags next to the anchors directory.

use std::{fs, os::unix::fs::PermissionsExt, path::Path};

/// What the stand-in does once Host has sent ARM.
pub(crate) enum AfterArm {
    /// Launch a vendor that writes `line` to the vendor stdout pipe, set the
    /// `wrote` flag, and never confirm the launch; stop the vendor and exit
    /// when Host closes the control connection.
    Stall { line: &'static str },
    /// Launch a vendor that holds the vendor pipes, confirm the launch and
    /// serve status and stop. A stop reports `stopped_live` and ends the
    /// vendor; with `linger` the stand-in ignores TERM and stays alive long
    /// after, so no group absence can be proved; otherwise it exits.
    Serve { stopped_live: bool, linger: bool },
}

const COMMON: &str = r"#!/usr/bin/env python3
import json, os, signal, socket, subprocess, sys, time
bootstrap = json.load(open(sys.argv[2]))
flags = os.path.dirname(os.path.dirname(bootstrap['socket_path']))
def flag(name):
    open(os.path.join(flags, name), 'w').close()
server = socket.socket(socket.AF_UNIX)
server.bind(bootstrap['socket_path'])
server.listen(1)
connection, _ = server.accept()
control = connection.makefile('rwb')
fields = open('/proc/self/stat').read().rsplit(') ', 1)[1].split()
identity = {
    'pid': os.getpid(), 'pgid': int(fields[2]), 'uid': os.getuid(),
    'boot_id': open('/proc/sys/kernel/random/boot_id').read().strip(),
    'pid_namespace': os.readlink('/proc/self/ns/pid'),
    'start_ticks': int(fields[19]), 'marker': bootstrap['marker'],
}
def send(frame):
    control.write(json.dumps(frame).encode() + b'\n')
    control.flush()
def detach():
    null = os.open('/dev/null', os.O_RDWR)
    for descriptor in (0, 1, 2):
        os.dup2(null, descriptor)
send({'kind': 'ready', 'identity': identity})
control.readline()
send({'kind': 'configured'})
control.readline()
";

impl AfterArm {
    fn script(&self) -> String {
        let tail = match self {
            Self::Stall { line } => format!(
                r#"vendor = subprocess.Popen(['/bin/sh', '-c', 'echo "$0"; : > "$1"; exec sleep 30', {line:?}, os.path.join(flags, 'wrote')])
detach()
control.readline()
vendor.kill()
vendor.wait()
"#
            ),
            Self::Serve {
                stopped_live,
                linger,
            } => format!(
                r"vendor = subprocess.Popen(['/bin/sleep', '30'])
detach()
if {linger}:
    signal.signal(signal.SIGTERM, signal.SIG_IGN)
send({{'kind': 'spawned', 'pid': vendor.pid}})
flag('spawned')
while True:
    line = control.readline()
    if not line:
        break
    request = json.loads(line)
    if request['kind'] == 'status':
        send({{'kind': 'status', 'pid': os.getpid(), 'exit_code': None, 'exit_signal': None}})
    elif request['kind'] == 'stop':
        send({{'kind': 'stopping', 'stopped_live': {live}}})
        break
vendor.kill()
vendor.wait()
if {linger}:
    time.sleep(15)
",
                linger = if *linger { "True" } else { "False" },
                live = if *stopped_live { "True" } else { "False" },
            ),
        };
        format!("{COMMON}{tail}")
    }

    /// Writes the stand-in as an executable at `path`.
    pub(crate) fn install(&self, path: &Path) {
        fs::write(path, self.script()).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
    }
}

/// Waits up to 10 s for the stand-in's barrier `flag` in `flags`, the parent
/// of Host's anchor directory.
pub(crate) async fn wait_flag(flags: &Path, flag: &str) {
    let path = flags.join(flag);
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    while !path.exists() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "stand-in anchor never set {flag}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}

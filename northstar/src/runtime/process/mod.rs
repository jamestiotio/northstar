// Copyright (c) 2021 ESRLabs
//
//   Licensed under the Apache License, Version 2.0 (the "License");
//   you may not use this file except in compliance with the License.
//   You may obtain a copy of the License at
//
//      http://www.apache.org/licenses/LICENSE-2.0
//
//   Unless required by applicable law or agreed to in writing, software
//   distributed under the License is distributed on an "AS IS" BASIS,
//   WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
//   See the License for the specific language governing permissions and
//   limitations under the License.

use self::fs::Dev;
use super::{
    config::Config,
    error::Error,
    pipe::{Condition, ConditionNotify, ConditionWait},
    state::MountedContainer,
    Event, EventTx, ExitStatus, Pid, ENV_NAME, ENV_VERSION,
};
use crate::{
    common::{container::Container, non_null_string::NonNullString},
    npk::manifest::Manifest,
    seccomp,
};
use async_trait::async_trait;
use caps::CapsHashSet;
use futures::{Future, FutureExt};
use log::{debug, error, info, warn};
use nix::{
    errno::Errno,
    libc::c_int,
    sched,
    sys::{
        self,
        signal::{sigprocmask, SigSet, SigmaskHow, Signal},
        wait::WaitPidFlag,
    },
    unistd,
};
use sched::CloneFlags;
use std::{
    collections::HashMap,
    convert::TryFrom,
    ffi::{c_void, CString},
    fmt,
    ptr::null,
};
use sys::wait;
use tokio::{signal, task, time};
use Signal::SIGCHLD;

mod clone;
mod fs;
mod init;
mod io;

/// Offset for signal as exit code encoding
const SIGNAL_OFFSET: i32 = 128;

#[derive(Debug)]
pub(super) struct Launcher {
    tx: EventTx,
    config: Config,
}

pub(super) struct Process {
    pid: Pid,
    checkpoint: Option<Checkpoint>,
    io: (Option<io::Log>, Option<io::Log>),
    exit_status: Option<Box<dyn Future<Output = ExitStatus> + Send + Sync + Unpin>>,
    _dev: Dev,
}

impl fmt::Debug for Process {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Process")
            .field("pid", &self.pid)
            .field("checkpoint", &self.checkpoint)
            .finish()
    }
}

impl Launcher {
    pub async fn start(tx: EventTx, config: Config) -> Result<Self, Error> {
        Ok(Launcher { tx, config })
    }

    pub async fn shutdown(self) -> Result<(), Error> {
        Ok(())
    }

    pub async fn create(
        &self,
        container: &MountedContainer,
        args: Option<&Vec<NonNullString>>,
        env: Option<&HashMap<NonNullString, NonNullString>>,
    ) -> Result<impl super::state::Process, Error> {
        let root = container
            .root
            .canonicalize()
            .expect("Failed to canonicalize root");
        let manifest = container.manifest.clone();
        let (mounts, dev) = fs::prepare_mounts(&self.config, container).await?;
        let container = container.container.clone();
        let (init, argv) = init_argv(&manifest, args);
        let env = self::env(&manifest, env);
        let (stdout, stderr, mut fds) = io::from_manifest(&manifest).await?;
        let fds = fds.drain().collect::<Vec<_>>();
        let (checkpoint_runtime, checkpoint_init) = checkpoints();
        let groups = groups(&manifest);
        let capabilities = capabilities(&manifest);
        let seccomp = seccomp_filter(&manifest);

        debug!("{} init is {:?}", manifest.name, init);
        debug!("{} argv is {:?}", manifest.name, argv);
        debug!("{} env is {:?}", manifest.name, env);

        // Block signals to make sure that a SIGTERM is not sent to init before the child is spawned.
        // init doesn't have any signals handler and the signal would be lost. The child has default handlers
        // and unblocks all signal and terminates in pending ones if needed. This termination is then caught
        // by init...
        signals_block();

        // Clone init
        let flags = CloneFlags::CLONE_NEWPID | CloneFlags::CLONE_NEWNS;
        match clone::clone(flags, Some(SIGCHLD as c_int)) {
            Ok(result) => match result {
                unistd::ForkResult::Parent { child } => {
                    debug!("Created {} with pid {}", container, child);

                    // Unblock signals that were block to start init with masked signals
                    signals_unblock();

                    drop(checkpoint_init);

                    // Close writing part of log forwards if any
                    let stdout = stdout.map(|(log, fd)| {
                        unistd::close(fd).ok();
                        log
                    });
                    let stderr = stderr.map(|(log, fd)| {
                        unistd::close(fd).ok();
                        log
                    });
                    let pid = child.as_raw() as Pid;

                    let exit_status = waitpid(container, pid, self.tx.clone());

                    Ok(Process {
                        pid,
                        io: (stdout, stderr),
                        checkpoint: Some(checkpoint_runtime),
                        exit_status: Some(Box::new(exit_status)),
                        _dev: dev,
                    })
                }
                unistd::ForkResult::Child => {
                    drop(checkpoint_runtime);
                    let init = init::Init {
                        manifest,
                        root,
                        init,
                        argv,
                        env,
                        mounts,
                        fds,
                        groups,
                        capabilities,
                        seccomp,
                    };

                    // Wait for the runtime to signal that init may start.
                    let condition_notify = checkpoint_init.wait();
                    init.run(condition_notify);
                }
            },
            Err(e) => panic!("Fork error: {}", e),
        }
    }
}

#[async_trait]
impl super::state::Process for Process {
    fn pid(&self) -> Pid {
        self.pid
    }

    async fn spawn(&mut self) -> Result<(), Error> {
        let checkpoint = self
            .checkpoint
            .take()
            .expect("Attempt to start container twice. This is a bug.");
        info!("Starting {}", self.pid());
        let wait = checkpoint.notify();

        // If the child process refuses to start - kill it after 5 seconds
        match time::timeout(time::Duration::from_secs(5), wait.async_wait()).await {
            Ok(_) => (),
            Err(_) => {
                error!(
                    "Timeout while waiting for {} to start. Sending SIGKILL to {}",
                    self.pid, self.pid
                );
                let process_group = unistd::Pid::from_raw(-(self.pid as i32));
                let sigkill = Some(sys::signal::SIGKILL);
                sys::signal::kill(process_group, sigkill).ok();
            }
        }

        Ok(())
    }

    async fn kill(&mut self, signal: Signal) -> Result<(), super::error::Error> {
        debug!("Sending {} to {}", signal.as_str(), self.pid);
        let process_group = unistd::Pid::from_raw(-(self.pid as i32));
        let sigterm = Some(signal);
        match sys::signal::kill(process_group, sigterm) {
            Ok(_) => {}
            // The process is terminated already. Wait for the waittask to do it's job and resolve exit_status
            Err(nix::Error::Sys(errno)) if errno == Errno::ESRCH => {
                debug!("Process {} already exited", self.pid);
            }
            Err(e) => {
                return Err(Error::Os(
                    format!("Failed to send signal {} {}", signal, process_group),
                    e,
                ))
            }
        }
        Ok(())
    }

    async fn wait(&mut self) -> Result<ExitStatus, Error> {
        let exit_status = self.exit_status.take().expect("Wait called twice");
        Ok(exit_status.await)
    }

    async fn destroy(&mut self) -> Result<(), Error> {
        if let Some(io) = self.io.0.take() {
            io.stop().await?;
        }
        if let Some(io) = self.io.1.take() {
            io.stop().await?;
        }
        Ok(())
    }
}

/// Spawn a task that waits for the process to exit. Resolves to the exit status of `pid`.
fn waitpid(container: Container, pid: Pid, tx: EventTx) -> impl Future<Output = ExitStatus> {
    task::spawn(async move {
        let mut sigchld = signal::unix::signal(signal::unix::SignalKind::child())
            .expect("Failed to set up signal handle for SIGCHLD");

        // Check the status of the process after every SIGCHLD is received
        let exit_status = loop {
            sigchld.recv().await;
            if let Some(exit) = exit_status(pid) {
                break exit;
            }
        };

        drop(
            tx.send(Event::Exit(container.clone(), exit_status.clone()))
                .await,
        );
        exit_status
    })
    .map(|r| r.expect("Task join error"))
}

/// Get exit status of process with `pid` or None
fn exit_status(pid: Pid) -> Option<ExitStatus> {
    let pid = unistd::Pid::from_raw(pid as i32);
    match wait::waitpid(Some(pid), Some(WaitPidFlag::WNOHANG)) {
        // The process exited normally (as with exit() or returning from main) with the given exit code.
        // This case matches the C macro WIFEXITED(status); the second field is WEXITSTATUS(status).
        Ok(wait::WaitStatus::Exited(pid, code)) => {
            // There is no way to make the "init" exit with a signal status. Use a defined
            // offset to get the original signal. This is the sad way everyone does it...
            if SIGNAL_OFFSET <= code {
                let signal = Signal::try_from(code - SIGNAL_OFFSET).expect("Invalid signal offset");
                debug!("Process {} exit status is signal {}", pid, signal);
                Some(ExitStatus::Signaled(signal))
            } else {
                debug!("Process {} exit code is {}", pid, code);
                Some(ExitStatus::Exit(code))
            }
        }

        // The process was killed by the given signal.
        // The third field indicates whether the signal generated a core dump. This case matches the C macro WIFSIGNALED(status); the last two fields correspond to WTERMSIG(status) and WCOREDUMP(status).
        Ok(wait::WaitStatus::Signaled(pid, signal, _dump)) => {
            debug!("Process {} exit status is signal {}", pid, signal);
            Some(ExitStatus::Signaled(signal))
        }

        // The process is alive, but was stopped by the given signal.
        // This is only reported if WaitPidFlag::WUNTRACED was passed. This case matches the C macro WIFSTOPPED(status); the second field is WSTOPSIG(status).
        Ok(wait::WaitStatus::Stopped(_pid, _signal)) => None,

        // The traced process was stopped by a PTRACE_EVENT_* event.
        // See nix::sys::ptrace and ptrace(2) for more information. All currently-defined events use SIGTRAP as the signal; the third field is the PTRACE_EVENT_* value of the event.
        #[cfg(any(target_os = "linux", target_os = "android"))]
        Ok(wait::WaitStatus::PtraceEvent(_pid, _signal, _)) => None,

        // The traced process was stopped by execution of a system call, and PTRACE_O_TRACESYSGOOD is in effect.
        // See ptrace(2) for more information.
        #[cfg(any(target_os = "linux", target_os = "android"))]
        Ok(wait::WaitStatus::PtraceSyscall(_pid)) => None,

        // The process was previously stopped but has resumed execution after receiving a SIGCONT signal.
        // This is only reported if WaitPidFlag::WCONTINUED was passed. This case matches the C macro WIFCONTINUED(status).
        Ok(wait::WaitStatus::Continued(_pid)) => None,

        // There are currently no state changes to report in any awaited child process.
        // This is only returned if WaitPidFlag::WNOHANG was used (otherwise wait() or waitpid() would block until there was something to report).
        Ok(wait::WaitStatus::StillAlive) => None,
        // Retry the waitpid call if waitpid fails with EINTR
        Err(e) if e == nix::Error::Sys(Errno::EINTR) => None,
        Err(e) if e == nix::Error::Sys(Errno::ECHILD) => {
            panic!("Waitpid returned ECHILD. This is bug.");
        }
        Err(e) => panic!("Failed to waitpid on {}: {}", pid, e),
    }
}

/// Construct the init and argv argument for the containers execve
fn init_argv(manifest: &Manifest, args: Option<&Vec<NonNullString>>) -> (CString, Vec<CString>) {
    // A container without an init shall not be started
    // Validation of init is done in `Manifest`
    let init = CString::new(
        manifest
            .init
            .as_ref()
            .expect("Attempt to use init from resource container")
            .to_str()
            .expect("Invalid init. This a bug in the manifest validation"),
    )
    .expect("Invalid init");

    // If optional arguments are defined, discard the values from the manifest.
    // if there are no optional args - take the values from the manifest if present
    // or nothing.
    let args = match (manifest.args.as_ref(), args) {
        (None, None) => &[],
        (None, Some(a)) => a.as_slice(),
        (Some(m), None) => m.as_slice(),
        (Some(_), Some(a)) => a.as_slice(),
    };

    let mut argv = Vec::with_capacity(1 + args.len());
    argv.push(init.clone());
    argv.extend({
        args.iter().map(|arg| {
            CString::new(arg.as_bytes())
                .expect("Invalid arg. This is a bug in the manifest or parameter validation")
        })
    });

    // argv
    (init, argv)
}

/// Construct the env argument for the containers execve. Optional args and env overwrite values from the
/// manifest.
fn env(manifest: &Manifest, env: Option<&HashMap<NonNullString, NonNullString>>) -> Vec<CString> {
    let mut result = Vec::with_capacity(2);
    result.push(
        CString::new(format!("{}={}", ENV_NAME, manifest.name.to_string()))
            .expect("Invalid container name. This is a bug in the manifest validation"),
    );
    result.push(CString::new(format!("{}={}", ENV_VERSION, manifest.version)).unwrap());

    if let Some(ref e) = manifest.env {
        result.extend({
            e.iter()
                .filter(|(k, _)| {
                    // Skip the values declared in fn arguments
                    env.map(|env| !env.contains_key(k)).unwrap_or(true)
                })
                .map(|(k, v)| {
                    CString::new(format!("{}={}", k, v))
                        .expect("Invalid env. This is a bug in the manifest validation")
                })
        })
    }

    // Add additional env variables passed
    if let Some(env) = env {
        result.extend(
            env.iter().map(|(k, v)| {
                CString::new(format!("{}={}", k, v)).expect("Invalid additional env")
            }),
        );
    }

    result
}

/// Generate a list of supplementary gids if the groups info can be retrieved. This
/// must happen before the init `clone` because the group information cannot be gathered
/// without `/etc` etc...
fn groups(manifest: &Manifest) -> Vec<u32> {
    if let Some(groups) = manifest.suppl_groups.as_ref() {
        let mut result = Vec::with_capacity(groups.len());
        for group in groups {
            let cgroup = CString::new(group.as_str()).unwrap(); // Check during manifest parsing
            let group_info =
                unsafe { nix::libc::getgrnam(cgroup.as_ptr() as *const nix::libc::c_char) };
            if group_info == (null::<c_void>() as *mut nix::libc::group) {
                warn!("Skipping invalid supplementary group {}", group);
            } else {
                let gid = unsafe { (*group_info).gr_gid };
                // TODO: Are there gids cannot use?
                result.push(gid)
            }
        }
        result
    } else {
        Vec::with_capacity(0)
    }
}

/// Generate seccomp filter applied in init
fn seccomp_filter(manifest: &Manifest) -> Option<seccomp::AllowList> {
    if let Some(seccomp) = manifest.seccomp.as_ref() {
        return Some(seccomp::seccomp_filter(
            seccomp.profile.as_ref(),
            seccomp.allow.as_ref(),
            manifest.capabilities.as_ref(),
        ));
    }
    None
}

/// Block all signals of this process and current thread
fn signals_block() {
    SigSet::all()
        .thread_block()
        .expect("Failed to set thread signal mask");
    sigprocmask(SigmaskHow::SIG_BLOCK, Some(&SigSet::all()), None).unwrap();
}

/// Unblock all signals of this process and current thread
fn signals_unblock() {
    SigSet::all()
        .thread_unblock()
        .expect("Failed to set thread signal mask");
    sigprocmask(SigmaskHow::SIG_UNBLOCK, Some(&SigSet::all()), None).unwrap();
}

/// Capability settings applied in init
struct Capabilities {
    all: CapsHashSet,
    bounded: CapsHashSet,
    set: CapsHashSet,
}

/// Calculate capability sets
fn capabilities(manifest: &Manifest) -> Capabilities {
    let all = caps::all();
    let mut bounded =
        caps::read(None, caps::CapSet::Bounding).expect("Failed to read bounding caps");
    let set = manifest.capabilities.clone().unwrap_or_default();
    bounded.retain(|c| !set.contains(c));
    Capabilities { all, bounded, set }
}

#[derive(Debug, Clone)]
pub(super) struct Checkpoint(ConditionWait, ConditionNotify);

fn checkpoints() -> (Checkpoint, Checkpoint) {
    let a = Condition::new().expect("Failed to create condition");
    a.set_cloexec();
    let b = Condition::new().expect("Failed to create condition");
    b.set_cloexec();

    let (aw, an) = a.split();
    let (bw, bn) = b.split();

    (Checkpoint(aw, bn), Checkpoint(bw, an))
}

impl Checkpoint {
    fn notify(self) -> ConditionWait {
        self.1.notify();
        self.0
    }

    fn wait(self) -> ConditionNotify {
        self.0.wait();
        self.1
    }
}
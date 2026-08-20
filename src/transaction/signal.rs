use crate::error::{GroveError, Result};
use rustix::process::{kill_process, Pid, Signal};
use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM};
use signal_hook::low_level::pipe;
use signal_hook::SigId;
use std::io::Read;
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Mutex, OnceLock};

static COORDINATOR: OnceLock<Coordinator> = OnceLock::new();
static INITIALIZE: Mutex<()> = Mutex::new(());

struct Coordinator {
    active_child: &'static AtomicI32,
    first_signal: &'static AtomicI32,
    _registrations: Vec<SigId>,
}

pub fn activate() -> Result<()> {
    let _initializing = INITIALIZE
        .lock()
        .map_err(|_| GroveError::failure("signal coordinator initialization lock was poisoned"))?;
    if COORDINATOR.get().is_some() {
        return check_interrupted();
    }
    let active_child = Box::leak(Box::new(AtomicI32::new(0)));
    let first_signal = Box::leak(Box::new(AtomicI32::new(0)));
    let mut registrations = Vec::new();
    for signal in [SIGINT, SIGTERM, SIGHUP] {
        let (mut read, write) = UnixStream::pair().map_err(|error| {
            GroveError::failure(format!("cannot create signal self-pipe: {error}"))
        })?;
        registrations.push(pipe::register(signal, write).map_err(|error| {
            GroveError::failure(format!("cannot register signal self-pipe: {error}"))
        })?);
        let child = &*active_child;
        let caught = &*first_signal;
        std::thread::Builder::new()
            .name(format!("git-grove-signal-{signal}"))
            .spawn(move || {
                let mut byte = [0_u8; 1];
                loop {
                    match read.read(&mut byte) {
                        Ok(0) => break,
                        Ok(_) => record_and_forward(signal, child, caught),
                        Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                        Err(_) => break,
                    }
                }
            })
            .map_err(|error| {
                GroveError::failure(format!("cannot start signal coordinator: {error}"))
            })?;
    }
    COORDINATOR
        .set(Coordinator {
            active_child,
            first_signal,
            _registrations: registrations,
        })
        .map_err(|_| GroveError::failure("signal coordinator was initialized concurrently"))?;
    check_interrupted()
}

pub fn begin_child(pid: u32) -> Result<()> {
    let Some(coordinator) = COORDINATOR.get() else {
        return Ok(());
    };
    let pid = i32::try_from(pid).map_err(|_| GroveError::failure("child PID exceeds i32"))?;
    coordinator.active_child.store(pid, Ordering::SeqCst);
    let signal = coordinator.first_signal.load(Ordering::SeqCst);
    if signal != 0 {
        forward(pid, signal);
    }
    Ok(())
}

pub fn finish_child() -> Result<()> {
    let Some(coordinator) = COORDINATOR.get() else {
        return Ok(());
    };
    coordinator.active_child.store(0, Ordering::SeqCst);
    interrupted(coordinator.first_signal.swap(0, Ordering::SeqCst))
}

pub fn check_interrupted() -> Result<()> {
    let Some(coordinator) = COORDINATOR.get() else {
        return Ok(());
    };
    interrupted(coordinator.first_signal.swap(0, Ordering::SeqCst))
}

fn record_and_forward(signal: i32, active_child: &AtomicI32, first_signal: &AtomicI32) {
    if first_signal
        .compare_exchange(0, signal, Ordering::SeqCst, Ordering::SeqCst)
        .is_ok()
    {
        let pid = active_child.load(Ordering::SeqCst);
        if pid > 0 {
            forward(pid, signal);
        }
    }
}

fn forward(pid: i32, signal: i32) {
    let Some(pid) = Pid::from_raw(pid) else {
        return;
    };
    let signal = match signal {
        SIGINT => Signal::INT,
        SIGTERM => Signal::TERM,
        SIGHUP => Signal::HUP,
        _ => return,
    };
    let _ = kill_process(pid, signal);
}

fn interrupted(signal: i32) -> Result<()> {
    if signal == 0 {
        Ok(())
    } else {
        Err(
            GroveError::failure(format!("interrupted by signal {signal}"))
                .with_exit_code((128 + signal) as u8),
        )
    }
}

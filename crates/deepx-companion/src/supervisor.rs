use std::io;
use std::process::Child;
use std::sync::{Mutex, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy)]
pub struct RestartPolicy {
    max_consecutive_failures: u32,
    base_delay: Duration,
    stable_after: Duration,
    poll_interval: Duration,
}

impl Default for RestartPolicy {
    fn default() -> Self {
        Self {
            max_consecutive_failures: 3,
            base_delay: Duration::from_secs(1),
            stable_after: Duration::from_secs(30),
            poll_interval: Duration::from_millis(250),
        }
    }
}

impl RestartPolicy {
    #[cfg(test)]
    fn for_tests(base_delay: Duration) -> Self {
        Self {
            base_delay,
            stable_after: Duration::from_secs(60),
            poll_interval: Duration::from_millis(5),
            ..Self::default()
        }
    }

    fn restart_delay(self, failures: u32) -> Duration {
        self.base_delay
            .saturating_mul(1_u32 << failures.saturating_sub(1).min(8))
    }
}

pub struct PetSupervisor {
    stop: Mutex<Option<mpsc::Sender<()>>>,
    worker: Mutex<Option<JoinHandle<()>>>,
}

impl PetSupervisor {
    pub fn start<F>(mut launch: F, policy: RestartPolicy) -> Self
    where
        F: FnMut() -> io::Result<Child> + Send + 'static,
    {
        let (stop_tx, stop_rx) = mpsc::channel();
        let worker = thread::Builder::new()
            .name("deepx-companion-supervisor".into())
            .spawn(move || supervise(&mut launch, policy, &stop_rx))
            .expect("spawn companion supervisor thread");
        Self {
            stop: Mutex::new(Some(stop_tx)),
            worker: Mutex::new(Some(worker)),
        }
    }

    pub fn shutdown(&self) {
        if let Some(stop) = self
            .stop
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take()
        {
            let _ = stop.send(());
        }
        if let Some(worker) = self
            .worker
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take()
        {
            let _ = worker.join();
        }
    }
}

fn supervise<F>(launch: &mut F, policy: RestartPolicy, stop: &mpsc::Receiver<()>)
where
    F: FnMut() -> io::Result<Child>,
{
    let mut failures = 0_u32;
    loop {
        if stop.try_recv().is_ok() {
            return;
        }
        let mut child = match launch() {
            Ok(child) => child,
            Err(_) => {
                failures += 1;
                if failures >= policy.max_consecutive_failures
                    || stop.recv_timeout(policy.restart_delay(failures)).is_ok()
                {
                    return;
                }
                continue;
            }
        };
        let launched_at = Instant::now();
        loop {
            if stop.recv_timeout(policy.poll_interval).is_ok() {
                terminate(&mut child);
                return;
            }
            match child.try_wait() {
                Ok(Some(_)) | Err(_) => break,
                Ok(None) if launched_at.elapsed() >= policy.stable_after => failures = 0,
                Ok(None) => {}
            }
        }
        failures += 1;
        if failures >= policy.max_consecutive_failures
            || stop.recv_timeout(policy.restart_delay(failures)).is_ok()
        {
            return;
        }
    }
}

fn terminate(child: &mut Child) {
    if matches!(child.try_wait(), Ok(None)) {
        let _ = child.kill();
    }
    let _ = child.wait();
}

#[cfg(test)]
mod tests {
    use std::process::{Child, Command};
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    use std::time::{Duration, Instant};

    use super::{PetSupervisor, RestartPolicy};

    fn short_lived_child() -> std::io::Result<Child> {
        #[cfg(windows)]
        {
            Command::new("cmd").args(["/C", "exit", "0"]).spawn()
        }
        #[cfg(not(windows))]
        {
            Command::new("sh").args(["-c", "exit 0"]).spawn()
        }
    }

    #[test]
    fn stops_after_three_consecutive_failures() {
        let launches = Arc::new(AtomicUsize::new(0));
        let observed = Arc::clone(&launches);
        let supervisor = PetSupervisor::start(
            move || {
                observed.fetch_add(1, Ordering::SeqCst);
                short_lived_child()
            },
            RestartPolicy::for_tests(Duration::from_millis(5)),
        );

        let deadline = Instant::now() + Duration::from_secs(3);
        while launches.load(Ordering::SeqCst) < 3 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        std::thread::sleep(Duration::from_millis(30));
        assert_eq!(launches.load(Ordering::SeqCst), 3);
        supervisor.shutdown();
    }

    #[test]
    fn shutdown_interrupts_restart_backoff() {
        let supervisor = PetSupervisor::start(
            short_lived_child,
            RestartPolicy::for_tests(Duration::from_secs(10)),
        );
        std::thread::sleep(Duration::from_millis(50));
        let started = Instant::now();
        supervisor.shutdown();
        assert!(started.elapsed() < Duration::from_secs(1));
    }
}

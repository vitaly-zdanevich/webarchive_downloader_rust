use std::net::{SocketAddr, TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use reqwest::{Client, Proxy, RequestBuilder};
use url::Url;

use crate::retry::format_retry_delay;

const SSH_TUNNEL_READY_TIMEOUT: Duration = Duration::from_secs(60);
const INITIAL_SSH_RETRY_DELAY: Duration = Duration::from_secs(60);
const MAX_SSH_RETRY_DELAY: Duration = Duration::from_secs(60 * 60);

/// A Wayback HTTP client that can rotate through SSH SOCKS fallback routes.
#[derive(Clone)]
pub struct WaybackClient {
    inner: Arc<WaybackClientInner>,
}

struct WaybackClientInner {
    direct: Client,
    active: Mutex<ActiveWaybackClient>,
    ssh_fallbacks: Vec<SshFallback>,
}

enum ActiveWaybackClient {
    Direct,
    Ssh { index: usize, client: Client },
}

impl WaybackClient {
    /// Builds a direct client and lazily configured SSH fallback routes.
    pub fn new(user_agent: &str, timeout: Duration, ssh_destinations: Vec<String>) -> Result<Self> {
        let direct = build_reqwest_client(user_agent, timeout, None)?;
        let ssh_fallbacks = ssh_destinations
            .into_iter()
            .map(|destination| SshFallback::new(destination, user_agent.to_owned(), timeout))
            .collect::<Result<Vec<_>>>()?;

        Ok(Self {
            inner: Arc::new(WaybackClientInner {
                direct,
                active: Mutex::new(ActiveWaybackClient::Direct),
                ssh_fallbacks,
            }),
        })
    }

    /// Creates a GET request through the currently active route.
    pub fn get(&self, url: Url) -> RequestBuilder {
        self.active_client().get(url)
    }

    /// Activates the next SSH fallback that is not in a temporary cooldown.
    pub fn activate_ssh(&self, reason: &str) -> Result<bool> {
        if self.inner.ssh_fallbacks.is_empty() {
            return Ok(false);
        };

        let mut active = lock_unpoisoned(&self.inner.active);
        let start_index = match &*active {
            ActiveWaybackClient::Direct => 0,
            ActiveWaybackClient::Ssh { index, .. } => index.saturating_add(1),
        };

        self.activate_ssh_from(&mut active, start_index, reason)
    }

    /// Temporarily cools the currently active SSH route and switches route.
    ///
    /// This is used when the SOCKS proxy is alive but the remote SSH side cannot
    /// connect to Wayback, which appears as SOCKS handshake or channel-open
    /// failures. If no later SSH fallback is available, the client returns to
    /// direct Wayback access. Cooled routes become eligible again automatically.
    pub fn recover_from_active_ssh_failure(&self, reason: &str) -> Result<bool> {
        let mut active = lock_unpoisoned(&self.inner.active);
        let ActiveWaybackClient::Ssh { index, .. } = &*active else {
            return Ok(false);
        };
        let failed_index = *index;
        let failed = &self.inner.ssh_fallbacks[failed_index];
        let retry_delay = failed.mark_failed();
        eprintln!(
            "SSH fallback {} became temporarily unavailable ({reason}); it will be eligible again in {}; trying next configured SSH destination",
            failed.destination(),
            format_retry_delay(retry_delay)
        );
        *active = ActiveWaybackClient::Direct;
        if self.activate_ssh_from(&mut active, failed_index.saturating_add(1), reason)? {
            Ok(true)
        } else {
            eprintln!("No usable SSH fallback remains; continuing direct Wayback retries");
            Ok(true)
        }
    }

    /// Returns true when requests are currently routed through an SSH fallback.
    pub fn is_using_ssh(&self) -> bool {
        matches!(
            &*lock_unpoisoned(&self.inner.active),
            ActiveWaybackClient::Ssh { .. }
        )
    }

    /// Describes the route currently used for new Wayback requests.
    pub fn active_route_label(&self) -> String {
        match &*lock_unpoisoned(&self.inner.active) {
            ActiveWaybackClient::Direct => "direct route".to_owned(),
            ActiveWaybackClient::Ssh { index, .. } => {
                format!(
                    "SSH tunnel {}",
                    self.inner.ssh_fallbacks[*index].destination()
                )
            }
        }
    }

    fn activate_ssh_from(
        &self,
        active: &mut ActiveWaybackClient,
        start_index: usize,
        reason: &str,
    ) -> Result<bool> {
        for index in start_index..self.inner.ssh_fallbacks.len() {
            let ssh_fallback = &self.inner.ssh_fallbacks[index];
            let client = match ssh_fallback.client() {
                Ok(Some(client)) => client,
                // The failure already announced this cooldown; do not repeat it per request.
                Ok(None) => continue,
                Err(error) => {
                    eprintln!(
                        "SSH fallback {} failed: {error:#}; trying next configured SSH destination",
                        ssh_fallback.destination()
                    );
                    continue;
                }
            };
            eprintln!(
                "Wayback unavailable via current route ({reason}); switching to SSH tunnel {}",
                ssh_fallback.destination()
            );
            *active = ActiveWaybackClient::Ssh { index, client };
            return Ok(true);
        }

        Ok(false)
    }

    #[cfg(test)]
    fn is_ssh_configured(&self) -> bool {
        !self.inner.ssh_fallbacks.is_empty()
    }

    #[cfg(test)]
    fn ssh_fallback_count(&self) -> usize {
        self.inner.ssh_fallbacks.len()
    }

    #[cfg(test)]
    fn ssh_retry_remaining_for_test(&self, index: usize) -> Option<Duration> {
        self.inner.ssh_fallbacks[index].retry_remaining_at(Instant::now())
    }

    fn active_client(&self) -> Client {
        match &*lock_unpoisoned(&self.inner.active) {
            ActiveWaybackClient::Direct => self.inner.direct.clone(),
            ActiveWaybackClient::Ssh { client, .. } => client.clone(),
        }
    }
}

struct SshFallback {
    destination: String,
    user_agent: String,
    timeout: Duration,
    state: Mutex<SshFallbackState>,
}

struct SshFallbackState {
    client: Option<Client>,
    tunnel: Option<SshTunnel>,
    retry_at: Option<Instant>,
    consecutive_failures: usize,
}

impl SshFallback {
    fn new(destination: String, user_agent: String, timeout: Duration) -> Result<Self> {
        let destination = destination.trim().to_owned();
        if destination.is_empty() {
            bail!("--ssh requires a destination like ubuntu@151.145.94.114");
        }
        if destination.chars().any(char::is_whitespace) {
            bail!("--ssh value must be a single SSH destination without whitespace");
        }

        Ok(Self {
            destination,
            user_agent,
            timeout,
            state: Mutex::new(SshFallbackState {
                client: None,
                tunnel: None,
                retry_at: None,
                consecutive_failures: 0,
            }),
        })
    }

    fn destination(&self) -> &str {
        &self.destination
    }

    fn client(&self) -> Result<Option<Client>> {
        let mut state = lock_unpoisoned(&self.state);
        if state.retry_remaining_at(Instant::now()).is_some() {
            return Ok(None);
        }
        state.retry_at = None;
        if let Some(client) = &state.client {
            return Ok(Some(client.clone()));
        }

        match self.start_client() {
            Ok((tunnel, client)) => {
                state.tunnel = Some(tunnel);
                state.client = Some(client.clone());
                state.consecutive_failures = 0;
                Ok(Some(client))
            }
            Err(error) => {
                let retry_delay = state.remember_failure(Instant::now());
                Err(error.context(format!(
                    "SSH fallback will be eligible again in {}",
                    format_retry_delay(retry_delay)
                )))
            }
        }
    }

    fn mark_failed(&self) -> Duration {
        let mut state = lock_unpoisoned(&self.state);
        state.remember_failure(Instant::now())
    }

    #[cfg(test)]
    fn retry_remaining_at(&self, now: Instant) -> Option<Duration> {
        lock_unpoisoned(&self.state).retry_remaining_at(now)
    }

    #[cfg(test)]
    fn expire_retry_for_test(&self) {
        lock_unpoisoned(&self.state).retry_at = Some(Instant::now());
    }

    fn start_client(&self) -> Result<(SshTunnel, Client)> {
        let tunnel = SshTunnel::start(&self.destination)?;
        let proxy = Proxy::all(format!("socks5h://{}", tunnel.local_addr()))
            .context("failed to configure SSH SOCKS proxy")?;
        let client = build_reqwest_client(&self.user_agent, self.timeout, Some(proxy))?;
        Ok((tunnel, client))
    }
}

impl SshFallbackState {
    fn remember_failure(&mut self, now: Instant) -> Duration {
        self.client = None;
        self.tunnel = None;
        let delay = ssh_retry_delay(self.consecutive_failures);
        self.consecutive_failures = self.consecutive_failures.saturating_add(1);
        self.retry_at = Some(now + delay);
        delay
    }

    fn retry_remaining_at(&self, now: Instant) -> Option<Duration> {
        self.retry_at
            .and_then(|retry_at| retry_at.checked_duration_since(now))
            .filter(|remaining| !remaining.is_zero())
    }
}

fn ssh_retry_delay(failure_count: usize) -> Duration {
    let multiplier = 1u64 << failure_count.min(6);
    let seconds = INITIAL_SSH_RETRY_DELAY
        .as_secs()
        .saturating_mul(multiplier)
        .min(MAX_SSH_RETRY_DELAY.as_secs());
    Duration::from_secs(seconds)
}

struct SshTunnel {
    child: Child,
    local_addr: SocketAddr,
}

impl SshTunnel {
    fn start(destination: &str) -> Result<Self> {
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .context("failed to reserve local SSH SOCKS port")?;
        let local_addr = listener
            .local_addr()
            .context("failed to read local SSH SOCKS port")?;
        drop(listener);

        let child = Command::new("ssh")
            .arg("-N")
            .arg("-D")
            .arg(local_addr.to_string())
            .arg("-o")
            .arg("ExitOnForwardFailure=yes")
            .arg("-o")
            .arg("ServerAliveInterval=30")
            .arg("-o")
            .arg("ServerAliveCountMax=3")
            .arg("-o")
            .arg("BatchMode=yes")
            .arg("-o")
            .arg(format!(
                "ConnectTimeout={}",
                SSH_TUNNEL_READY_TIMEOUT.as_secs()
            ))
            .arg(destination)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .with_context(|| format!("failed to start SSH tunnel to {destination}"))?;

        let mut tunnel = Self { child, local_addr };
        tunnel
            .wait_until_ready(SSH_TUNNEL_READY_TIMEOUT)
            .with_context(|| format!("SSH tunnel to {destination} did not become ready"))?;

        Ok(tunnel)
    }

    fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    fn wait_until_ready(&mut self, timeout: Duration) -> Result<()> {
        wait_for_ssh_tunnel(&mut self.child, self.local_addr, timeout)
    }
}

impl Drop for SshTunnel {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn wait_for_ssh_tunnel(child: &mut Child, local_addr: SocketAddr, timeout: Duration) -> Result<()> {
    let started_at = Instant::now();
    loop {
        if let Some(status) = child.try_wait().context("failed to poll SSH tunnel")? {
            bail!("ssh exited early with status {status}");
        }
        if TcpStream::connect_timeout(&local_addr, Duration::from_millis(100)).is_ok() {
            return Ok(());
        }
        let elapsed = started_at.elapsed();
        if elapsed >= timeout {
            bail!(
                "timed out after {} waiting for local SOCKS listener at {local_addr}",
                format_retry_delay(timeout)
            );
        }
        std::thread::sleep(Duration::from_millis(50).min(timeout.saturating_sub(elapsed)));
    }
}

fn build_reqwest_client(
    user_agent: &str,
    timeout: Duration,
    proxy: Option<Proxy>,
) -> Result<Client> {
    let mut builder = Client::builder()
        .user_agent(user_agent)
        .timeout(timeout)
        .redirect(reqwest::redirect::Policy::limited(10));
    if let Some(proxy) = proxy {
        builder = builder.proxy(proxy);
    }
    builder.build().context("failed to build HTTP client")
}

fn lock_unpoisoned<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|error| error.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_without_ssh_fallback() {
        let client = WaybackClient::new(
            crate::DEFAULT_USER_AGENT,
            Duration::from_secs(1),
            Vec::new(),
        )
        .unwrap();

        assert!(!client.is_ssh_configured());
        assert_eq!(client.active_route_label(), "direct route");
    }

    #[test]
    fn builds_with_multiple_ssh_fallbacks() {
        let client = WaybackClient::new(
            crate::DEFAULT_USER_AGENT,
            Duration::from_secs(1),
            vec![
                "ubuntu@151.145.94.114".to_owned(),
                "ubuntu@203.0.113.10".to_owned(),
            ],
        )
        .unwrap();

        assert!(client.is_ssh_configured());
        assert_eq!(client.ssh_fallback_count(), 2);
    }

    #[test]
    fn rejects_empty_ssh_destination() {
        assert!(
            WaybackClient::new(
                crate::DEFAULT_USER_AGENT,
                Duration::from_secs(1),
                vec![" ".to_owned()]
            )
            .is_err()
        );
    }

    #[test]
    fn active_ssh_failure_temporarily_cools_fallback() {
        let direct = build_reqwest_client(
            crate::DEFAULT_USER_AGENT,
            Duration::from_secs(1),
            None,
        )
        .unwrap();
        let fallback = SshFallback::new(
            "ubuntu@151.145.94.114".to_owned(),
            crate::DEFAULT_USER_AGENT.to_owned(),
            Duration::from_secs(1),
        )
        .unwrap();
        let client = WaybackClient {
            inner: Arc::new(WaybackClientInner {
                direct: direct.clone(),
                active: Mutex::new(ActiveWaybackClient::Ssh {
                    index: 0,
                    client: direct,
                }),
                ssh_fallbacks: vec![fallback],
            }),
        };

        assert!(client.is_using_ssh());
        assert_eq!(
            client.active_route_label(),
            "SSH tunnel ubuntu@151.145.94.114"
        );
        assert!(
            client
                .recover_from_active_ssh_failure("SOCKS handshake failed")
                .unwrap()
        );
        assert!(!client.is_using_ssh());
        assert!(!client.activate_ssh("retry").unwrap());
        assert!(client.ssh_retry_remaining_for_test(0).is_some());
    }

    #[test]
    fn failed_ssh_fallback_becomes_eligible_after_cooldown() {
        let fallback = SshFallback::new(
            "ubuntu@151.145.94.114".to_owned(),
            crate::DEFAULT_USER_AGENT.to_owned(),
            Duration::from_secs(1),
        )
        .unwrap();

        fallback.mark_failed();
        assert!(fallback.retry_remaining_at(Instant::now()).is_some());

        fallback.expire_retry_for_test();
        assert_eq!(fallback.retry_remaining_at(Instant::now()), None);
    }

    #[test]
    fn ssh_retry_delay_is_exponential_and_capped() {
        assert_eq!(ssh_retry_delay(0), Duration::from_secs(60));
        assert_eq!(ssh_retry_delay(1), Duration::from_secs(120));
        assert_eq!(ssh_retry_delay(100), Duration::from_secs(3600));
    }

    #[test]
    fn active_ssh_recovery_is_noop_for_direct_route() {
        let client = WaybackClient::new(
            crate::DEFAULT_USER_AGENT,
            Duration::from_secs(1),
            vec!["ubuntu@151.145.94.114".to_owned()],
        )
        .unwrap();

        assert!(!client.is_using_ssh());
        assert!(!client.recover_from_active_ssh_failure("direct").unwrap());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn dropping_ssh_tunnel_kills_and_reaps_child_process() {
        use std::path::Path;

        let child = Command::new("sleep").arg("60").spawn().unwrap();
        let child_pid = child.id();
        let tunnel = SshTunnel {
            child,
            local_addr: SocketAddr::from(([127, 0, 0, 1], 0)),
        };

        drop(tunnel);

        assert!(!Path::new(&format!("/proc/{child_pid}")).exists());
    }
}

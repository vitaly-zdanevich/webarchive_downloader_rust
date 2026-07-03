use std::net::{SocketAddr, TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use reqwest::{Client, Proxy, RequestBuilder};
use url::Url;

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

    pub fn get(&self, url: Url) -> RequestBuilder {
        self.active_client().get(url)
    }

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

    /// Marks the currently active SSH route as unusable and switches route.
    ///
    /// This is used when the SOCKS proxy is alive but the remote SSH side cannot
    /// connect to Wayback, which appears as SOCKS handshake or channel-open
    /// failures. If no later SSH fallback is available, the client returns to
    /// direct Wayback access so retries do not stay pinned to a broken tunnel.
    pub fn recover_from_active_ssh_failure(&self, reason: &str) -> Result<bool> {
        let mut active = lock_unpoisoned(&self.inner.active);
        let ActiveWaybackClient::Ssh { index, .. } = &*active else {
            return Ok(false);
        };
        let failed_index = *index;
        let failed = &self.inner.ssh_fallbacks[failed_index];
        failed.mark_failed();
        eprintln!(
            "SSH fallback {} became unusable ({reason}); trying next configured SSH destination",
            failed.destination()
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
                "Wayback unavailable via current route ({reason}); retrying through SSH tunnel {}",
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
    failed: bool,
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
                failed: false,
            }),
        })
    }

    fn destination(&self) -> &str {
        &self.destination
    }

    fn client(&self) -> Result<Option<Client>> {
        let mut state = lock_unpoisoned(&self.state);
        if state.failed {
            return Ok(None);
        }
        if let Some(client) = &state.client {
            return Ok(Some(client.clone()));
        }

        match self.start_client() {
            Ok((tunnel, client)) => {
                state.tunnel = Some(tunnel);
                state.client = Some(client.clone());
                Ok(Some(client))
            }
            Err(error) => {
                state.failed = true;
                Err(error)
            }
        }
    }

    fn mark_failed(&self) {
        let mut state = lock_unpoisoned(&self.state);
        state.failed = true;
        state.client = None;
        state.tunnel = None;
    }

    fn start_client(&self) -> Result<(SshTunnel, Client)> {
        let tunnel = SshTunnel::start(&self.destination)?;
        let proxy = Proxy::all(format!("socks5h://{}", tunnel.local_addr()))
            .context("failed to configure SSH SOCKS proxy")?;
        let client = build_reqwest_client(&self.user_agent, self.timeout, Some(proxy))?;
        Ok((tunnel, client))
    }
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

        let mut child = Command::new("ssh")
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
            .arg(destination)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .with_context(|| format!("failed to start SSH tunnel to {destination}"))?;

        wait_for_ssh_tunnel(&mut child, local_addr)
            .with_context(|| format!("SSH tunnel to {destination} did not become ready"))?;

        Ok(Self { child, local_addr })
    }

    fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }
}

impl Drop for SshTunnel {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn wait_for_ssh_tunnel(child: &mut Child, local_addr: SocketAddr) -> Result<()> {
    for _ in 0..40 {
        if let Some(status) = child.try_wait().context("failed to poll SSH tunnel")? {
            bail!("ssh exited early with status {status}");
        }
        if TcpStream::connect_timeout(&local_addr, Duration::from_millis(100)).is_ok() {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    bail!("timed out waiting for local SOCKS listener at {local_addr}");
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
            "webarchive-downloader-rust/0.1",
            Duration::from_secs(1),
            Vec::new(),
        )
        .unwrap();

        assert!(!client.is_ssh_configured());
    }

    #[test]
    fn builds_with_multiple_ssh_fallbacks() {
        let client = WaybackClient::new(
            "webarchive-downloader-rust/0.1",
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
                "webarchive-downloader-rust/0.1",
                Duration::from_secs(1),
                vec![" ".to_owned()]
            )
            .is_err()
        );
    }

    #[test]
    fn recovers_from_active_ssh_failure_without_reusing_failed_fallback() {
        let direct = build_reqwest_client(
            "webarchive-downloader-rust/0.1",
            Duration::from_secs(1),
            None,
        )
        .unwrap();
        let fallback = SshFallback::new(
            "ubuntu@151.145.94.114".to_owned(),
            "webarchive-downloader-rust/0.1".to_owned(),
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
        assert!(
            client
                .recover_from_active_ssh_failure("SOCKS handshake failed")
                .unwrap()
        );
        assert!(!client.is_using_ssh());
        assert!(!client.activate_ssh("retry").unwrap());
    }

    #[test]
    fn active_ssh_recovery_is_noop_for_direct_route() {
        let client = WaybackClient::new(
            "webarchive-downloader-rust/0.1",
            Duration::from_secs(1),
            vec!["ubuntu@151.145.94.114".to_owned()],
        )
        .unwrap();

        assert!(!client.is_using_ssh());
        assert!(!client.recover_from_active_ssh_failure("direct").unwrap());
    }
}

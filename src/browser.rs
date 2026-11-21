use std::os::unix::fs::DirBuilderExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::config::{Config, claudette_dir};

const PROXY_START: Duration = Duration::from_secs(15);
const PROXY_CHECK: Duration = Duration::from_secs(1);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Family {
    Chromium,
    Firefox,
}

impl Family {
    fn name(self) -> &'static str {
        match self {
            Family::Chromium => "chromium",
            Family::Firefox => "firefox",
        }
    }
}

const MACOS: [(&str, Family); 5] = [
    ("Google Chrome.app/Contents/MacOS/Google Chrome", Family::Chromium),
    ("Chromium.app/Contents/MacOS/Chromium", Family::Chromium),
    ("Brave Browser.app/Contents/MacOS/Brave Browser", Family::Chromium),
    ("Microsoft Edge.app/Contents/MacOS/Microsoft Edge", Family::Chromium),
    ("Firefox.app/Contents/MacOS/firefox", Family::Firefox),
];

const LINUX: [(&str, Family); 7] = [
    ("google-chrome", Family::Chromium),
    ("google-chrome-stable", Family::Chromium),
    ("chromium", Family::Chromium),
    ("chromium-browser", Family::Chromium),
    ("brave-browser", Family::Chromium),
    ("microsoft-edge", Family::Chromium),
    ("firefox", Family::Firefox),
];

fn find() -> Option<(PathBuf, Family)> {
    if cfg!(target_os = "macos") {
        let roots = [PathBuf::from("/Applications"), crate::config::home().join("Applications")];
        return MACOS.iter().flat_map(|(app, family)| roots.iter().map(move |root| (root.join(app), *family))).find(|(path, _)| path.is_file());
    }
    let path = std::env::var_os("PATH")?;
    LINUX.iter().find_map(|(name, family)| std::env::split_paths(&path).map(|dir| dir.join(name)).find(|p| p.is_file()).map(|p| (p, *family)))
}

pub fn check_url(url: &str) -> Result<reqwest::Url, String> {
    let parsed = reqwest::Url::parse(url.trim()).map_err(|e| format!("not a URL ({e}): {url}"))?;
    match parsed.scheme() {
        "http" | "https" => Ok(parsed),
        other => Err(format!("only http and https links open in claudette's browser, not {other}:")),
    }
}

fn chromium_args(profile: &Path, port: u16, url: &str) -> Vec<String> {
    let mut args: Vec<String> = [
        format!("--user-data-dir={}", profile.display()),
        format!("--proxy-server=socks5://127.0.0.1:{port}"),
        "--proxy-bypass-list=<-loopback>".into(),
        "--webrtc-ip-handling-policy=disable_non_proxied_udp".into(),
        "--disable-quic".into(),
        "--disable-background-networking".into(),
        "--disable-component-update".into(),
        "--disable-sync".into(),
        "--disable-breakpad".into(),
        "--disable-domain-reliability".into(),
        "--disable-features=MediaRouter,OptimizationHints,NetworkPrediction".into(),
        "--no-pings".into(),
        "--no-first-run".into(),
        "--no-default-browser-check".into(),
        "--new-window".into(),
    ]
    .into();
    args.push(url.to_owned());
    args
}

fn firefox_prefs(port: u16) -> String {
    let prefs: [(&str, &str); 31] = [
        ("network.proxy.type", "1"),
        ("network.proxy.socks", "\"127.0.0.1\""),
        ("network.proxy.socks_port", &port.to_string()),
        ("network.proxy.socks_version", "5"),
        ("network.proxy.socks_remote_dns", "true"),
        ("network.proxy.socks5_remote_dns", "true"),
        ("network.proxy.no_proxies_on", "\"\""),
        ("network.proxy.allow_hijacking_localhost", "true"),
        ("network.proxy.failover_direct", "false"),
        ("network.proxy.share_proxy_settings", "false"),
        ("network.trr.mode", "5"),
        ("network.dns.disablePrefetch", "true"),
        ("network.prefetch-next", "false"),
        ("network.predictor.enabled", "false"),
        ("network.http.speculative-parallel-limit", "0"),
        ("network.http.http3.enable", "false"),
        ("media.peerconnection.enabled", "false"),
        ("network.captive-portal-service.enabled", "false"),
        ("network.connectivity-service.enabled", "false"),
        ("browser.safebrowsing.malware.enabled", "false"),
        ("browser.safebrowsing.phishing.enabled", "false"),
        ("browser.safebrowsing.downloads.remote.enabled", "false"),
        ("toolkit.telemetry.enabled", "false"),
        ("datareporting.healthreport.uploadEnabled", "false"),
        ("datareporting.policy.dataSubmissionEnabled", "false"),
        ("app.normandy.enabled", "false"),
        ("app.update.auto", "false"),
        ("browser.shell.checkDefaultBrowser", "false"),
        ("browser.startup.homepage_override.mstone", "\"ignore\""),
        ("browser.aboutwelcome.enabled", "false"),
        ("geo.enabled", "false"),
    ];
    prefs.iter().map(|(name, value)| format!("user_pref(\"{name}\", {value});\n")).collect()
}

fn firefox_args(profile: &Path, url: &str) -> Vec<String> {
    vec!["--profile".into(), profile.display().to_string(), "--new-window".into(), url.to_owned()]
}

async fn socks_ready(port: u16) -> bool {
    let attempt = async {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).await.ok()?;
        stream.write_all(&[5, 1, 0]).await.ok()?;
        let mut reply = [0u8; 2];
        stream.read_exact(&mut reply).await.ok()?;
        (reply == [5, 0]).then_some(())
    };
    tokio::time::timeout(PROXY_CHECK, attempt).await.ok().flatten().is_some()
}

fn free_port() -> Result<u16, String> {
    std::net::TcpListener::bind(("127.0.0.1", 0)).and_then(|l| l.local_addr()).map(|a| a.port()).map_err(|e| format!("no free local port for the browser's proxy: {e}"))
}

pub struct Browser {
    config: Config,
    starting: tokio::sync::Mutex<()>,
}

impl Browser {
    pub fn new(config: Config) -> Browser {
        Browser { config, starting: tokio::sync::Mutex::new(()) }
    }

    fn profile(&self, family: Family) -> Result<PathBuf, String> {
        let host: String = Sha256::digest(self.config.host.as_bytes()).iter().take(6).map(|b| format!("{b:02x}")).collect();
        let dir = claudette_dir().join("browser").join(format!("{}-{host}", family.name()));
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(&dir).map_err(|e| format!("could not make {}: {e}", dir.display()))?;
        Ok(dir)
    }

    fn port(profile: &Path) -> Result<u16, String> {
        let file = profile.join("claudette-proxy-port");
        if let Some(port) = std::fs::read_to_string(&file).ok().and_then(|s| s.trim().parse().ok()) {
            return Ok(port);
        }
        let port = free_port()?;
        std::fs::write(&file, port.to_string()).map_err(|e| format!("could not save the proxy port: {e}"))?;
        Ok(port)
    }

    async fn proxy(&self, port: u16) -> Result<(), String> {
        let _held = self.starting.lock().await;
        if socks_ready(port).await {
            return Ok(());
        }
        let mut argv = vec![self.config.ssh[0].clone()];
        argv.extend(["-o", "ControlPath=none", "-o", "ExitOnForwardFailure=yes", "-N", "-T"].map(str::to_owned));
        argv.extend(crate::config::BATCH.iter().map(|s| (*s).to_owned()));
        argv.extend(self.config.ssh[1..].iter().cloned());
        argv.extend(["-D".to_owned(), format!("127.0.0.1:{port}"), self.config.host.clone()]);
        let mut child = tokio::process::Command::new(&argv[0])
            .args(&argv[1..])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .process_group(0)
            .spawn()
            .map_err(|e| format!("could not start ssh for the browser's proxy: {e}"))?;
        if let Some(pid) = child.id() {
            crate::tools::track_group(pid);
        }
        let deadline = tokio::time::Instant::now() + PROXY_START;
        while tokio::time::Instant::now() < deadline {
            if let Ok(Some(status)) = child.try_wait() {
                let mut err = String::new();
                if let Some(mut stderr) = child.stderr.take() {
                    let _ = stderr.read_to_string(&mut err).await;
                }
                return Err(format!("ssh could not open the browser's proxy to {} ({status}): {}", self.config.host, err.trim()));
            }
            if socks_ready(port).await {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let _ = child.kill().await;
        Err(format!("the browser's proxy to {} did not start within {}s", self.config.host, PROXY_START.as_secs()))
    }

    pub async fn open(&self, url: &str) -> Result<(), String> {
        let url = check_url(url)?;
        let (program, family) = find().ok_or("no supported browser on this machine: install Google Chrome, Chromium, Brave, Microsoft Edge or Firefox; nothing was opened")?;
        let profile = self.profile(family)?;
        let port = Self::port(&profile)?;
        self.proxy(port).await?;
        let args = match family {
            Family::Chromium => chromium_args(&profile, port, url.as_str()),
            Family::Firefox => {
                std::fs::write(profile.join("user.js"), firefox_prefs(port)).map_err(|e| format!("could not write the browser's settings: {e}"))?;
                firefox_args(&profile, url.as_str())
            }
        };
        let mut child = std::process::Command::new(&program)
            .args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()
            .map_err(|e| format!("could not start {}: {e}", program.display()))?;
        std::thread::spawn(move || child.wait());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_web_links_open() {
        assert!(check_url("https://claude.ai/oauth/authorize?x=1").is_ok());
        assert!(check_url("http://localhost:4000/callback").is_ok());
        for bad in ["file:///etc/passwd", "javascript:alert(1)", "chrome://settings", "not a url"] {
            assert!(check_url(bad).is_err(), "{bad}");
        }
    }
}

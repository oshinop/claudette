use std::fs::File;
use std::os::unix::fs::DirBuilderExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Mutex;
use std::time::Duration;

use nix::errno::Errno;
use nix::fcntl::{Flock, FlockArg};
use nix::sys::signal::{Signal, kill, killpg};
use nix::unistd::Pid;

use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::config::{Config, claudette_dir};

const PROXY_START: Duration = Duration::from_secs(15);
const PROXY_CHECK: Duration = Duration::from_secs(1);
const VERSION_CHECK: Duration = Duration::from_secs(10);
const LOCK_WAIT: Duration = Duration::from_secs(30);
const PORT_TRIES: usize = 16;
const PROXY_PORT: &str = "claudette-proxy-port";
const PROXY_PID: &str = "claudette-proxy-pid";
const SESSIONS: &str = "claudette-sessions";
const LAUNCH: &str = "claudette-launch";

pub const GATE_TOKEN_BYTES: usize = 16;
const SEED_BYTES: usize = 16;
const SEED: &str = "claudette-seed";
const WINDOWS: [(u32, u32); 6] = [(1000, 700), (1200, 700), (1200, 800), (1400, 800), (1400, 900), (1600, 900)];
const CORES: [usize; 6] = [4, 6, 8, 10, 12, 16];
const BRAVE_WEBRTC_POLICY: &str = "disable_non_proxied_udp";

static JOINED: Mutex<Vec<(PathBuf, String)>> = Mutex::new(Vec::new());

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    Firefox,
    Brave,
}

impl Kind {
    fn id(self) -> &'static str {
        match self {
            Kind::Firefox => "firefox",
            Kind::Brave => "brave",
        }
    }

    fn name(self) -> &'static str {
        match self {
            Kind::Firefox => "Firefox",
            Kind::Brave => "Brave",
        }
    }

    fn min_version(self) -> u32 {
        match self {
            Kind::Firefox => 148,
            Kind::Brave => 105,
        }
    }
}

const MACOS: [(&str, Kind); 2] = [("Firefox.app/Contents/MacOS/firefox", Kind::Firefox), ("Brave Browser.app/Contents/MacOS/Brave Browser", Kind::Brave)];

const LINUX: [(&str, Kind); 4] = [("firefox", Kind::Firefox), ("firefox-esr", Kind::Firefox), ("brave-browser", Kind::Brave), ("brave", Kind::Brave)];

fn find() -> Option<(PathBuf, Kind)> {
    if cfg!(target_os = "macos") {
        let roots = [PathBuf::from("/Applications"), crate::config::home().join("Applications")];
        return MACOS.iter().flat_map(|(app, kind)| roots.iter().map(move |root| (root.join(app), *kind))).find(|(path, _)| path.is_file());
    }
    let path = std::env::var_os("PATH")?;
    LINUX.iter().find_map(|(name, kind)| std::env::split_paths(&path).map(|dir| dir.join(name)).find(|p| p.is_file()).map(|p| (p, *kind)))
}

pub fn random<const N: usize>() -> std::io::Result<[u8; N]> {
    let mut bytes = [0u8; N];
    std::io::Read::read_exact(&mut File::open("/dev/urandom")?, &mut bytes)?;
    Ok(bytes)
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn check_url(url: &str) -> Result<reqwest::Url, String> {
    let parsed = reqwest::Url::parse(url.trim()).map_err(|e| format!("not a URL ({e}): {url}"))?;
    match parsed.scheme() {
        "http" | "https" => Ok(parsed),
        other => Err(format!("only http and https links open in claudette's browser, not {other}:")),
    }
}

pub fn valid_time_zone(name: &str) -> bool {
    name.len() <= 64 && name.split('/').all(|part| !part.is_empty() && part != "." && part != "..") && name.bytes().all(|b| b.is_ascii_alphanumeric() || b"_+-/".contains(&b))
}

struct Open {
    url: String,
    time_zone: String,
}

fn parse_open(request: &str) -> Result<Open, String> {
    let bad = || format!("not a link handed over by claudette's helper: {request}");
    let mut parts = request.split(' ');
    let (Some(port), Some(token), Some(time_zone), None) = (parts.next(), parts.next(), parts.next(), parts.next()) else { return Err(bad()) };
    let port: u16 = port.parse().ok().filter(|&p| p != 0).ok_or_else(bad)?;
    if token.len() != 2 * GATE_TOKEN_BYTES || !token.bytes().all(|b| b.is_ascii_hexdigit()) || !valid_time_zone(time_zone) {
        return Err(bad());
    }
    Ok(Open { url: format!("http://127.0.0.1:{port}/{token}"), time_zone: time_zone.to_owned() })
}

fn major_version(text: &str) -> Option<u32> {
    text.split_whitespace().find(|w| w.contains('.'))?.split('.').next()?.parse().ok()
}

async fn version(program: &Path) -> Result<u32, String> {
    let run = tokio::process::Command::new(program).arg("--version").stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null()).kill_on_drop(true).output();
    let output = tokio::time::timeout(VERSION_CHECK, run)
        .await
        .map_err(|_| format!("{} did not tell its version within {}s; nothing was opened", program.display(), VERSION_CHECK.as_secs()))?
        .map_err(|e| format!("could not ask {} for its version: {e}; nothing was opened", program.display()))?;
    let text = String::from_utf8_lossy(&output.stdout);
    major_version(&text).ok_or_else(|| format!("could not tell which version {} is from {:?}; nothing was opened", program.display(), text.trim()))
}

fn profile_args(kind: Kind, profile: &Path) -> Vec<String> {
    match kind {
        Kind::Firefox => vec!["--profile".into(), profile.display().to_string()],
        Kind::Brave => vec![format!("--user-data-dir={}", profile.display())],
    }
}

struct Persona {
    window: (u32, u32),
    cores: usize,
    time_zone: String,
}

impl Persona {
    fn new(seed: &[u8], time_zone: String) -> Persona {
        let real = std::thread::available_parallelism().map_or(1, |n| n.get());
        let fitting: Vec<usize> = CORES.into_iter().filter(|&c| c <= real).collect();
        let cores = if fitting.is_empty() { real } else { pick(seed, "cores", &fitting) };
        Persona { window: pick(seed, "window", &WINDOWS), cores, time_zone }
    }
}

fn pick<T: Copy>(seed: &[u8], label: &str, from: &[T]) -> T {
    let digest = Sha256::new().chain_update(seed).chain_update(label).finalize();
    let n = u64::from_be_bytes(digest[..8].try_into().expect("a SHA-256 digest has 32 bytes"));
    from[(n % from.len() as u64) as usize]
}

fn seed(profile: &Path) -> Result<String, String> {
    let path = profile.join(SEED);
    if let Ok(text) = std::fs::read_to_string(&path)
        && text.trim().len() == 2 * SEED_BYTES
    {
        return Ok(text.trim().to_owned());
    }
    let fail = |e: std::io::Error| format!("could not give the browser for this machine its own fingerprint: {e}; nothing was opened");
    let fresh = hex(&random::<SEED_BYTES>().map_err(fail)?);
    std::fs::write(&path, &fresh).map_err(fail)?;
    Ok(fresh)
}

fn brave_args(port: u16, persona: &Persona) -> Vec<String> {
    let (width, height) = persona.window;
    [
        format!("--proxy-server=socks5://127.0.0.1:{port}"),
        "--proxy-bypass-list=<-loopback>".into(),
        "--host-resolver-rules=MAP * ~NOTFOUND , EXCLUDE 127.0.0.1".into(),
        "--disable-quic".into(),
        "--disable-background-networking".into(),
        "--disable-component-update".into(),
        "--disable-sync".into(),
        "--disable-breakpad".into(),
        "--disable-domain-reliability".into(),
        "--disable-features=MediaRouter,OptimizationHints".into(),
        "--disable-brave-update".into(),
        "--disable-tor-client-updater-extension".into(),
        "--no-pings".into(),
        "--no-first-run".into(),
        "--no-default-browser-check".into(),
        "--accept-lang=en-US,en".into(),
        format!("--window-size={width},{height}"),
    ]
    .into()
}

fn firefox_prefs(port: u16, persona: &Persona) -> String {
    let (width, height) = persona.window;
    let prefs: [(&str, String); 44] = [
        ("network.proxy.type", "1".into()),
        ("network.proxy.socks", "\"127.0.0.1\"".into()),
        ("network.proxy.socks_port", port.to_string()),
        ("network.proxy.socks_version", "5".into()),
        ("network.proxy.socks_remote_dns", "true".into()),
        ("network.proxy.socks5_remote_dns", "true".into()),
        ("network.proxy.no_proxies_on", "\"\"".into()),
        ("network.proxy.allow_hijacking_localhost", "true".into()),
        ("network.proxy.failover_direct", "false".into()),
        ("network.proxy.allow_bypass", "false".into()),
        ("network.dns.disabled", "true".into()),
        ("network.trr.mode", "5".into()),
        ("network.dns.disablePrefetch", "true".into()),
        ("network.prefetch-next", "false".into()),
        ("network.predictor.enabled", "false".into()),
        ("network.http.speculative-parallel-limit", "0".into()),
        ("network.http.http3.enable", "false".into()),
        ("media.peerconnection.ice.proxy_only", "true".into()),
        ("media.peerconnection.ice.default_address_only", "true".into()),
        ("media.peerconnection.ice.no_host", "true".into()),
        ("network.captive-portal-service.enabled", "false".into()),
        ("network.connectivity-service.enabled", "false".into()),
        ("browser.safebrowsing.malware.enabled", "false".into()),
        ("browser.safebrowsing.phishing.enabled", "false".into()),
        ("browser.safebrowsing.downloads.remote.enabled", "false".into()),
        ("toolkit.telemetry.enabled", "false".into()),
        ("toolkit.telemetry.shutdownPingSender.enabled", "false".into()),
        ("datareporting.healthreport.uploadEnabled", "false".into()),
        ("datareporting.policy.dataSubmissionEnabled", "false".into()),
        ("app.normandy.enabled", "false".into()),
        ("app.update.auto", "false".into()),
        ("browser.shell.checkDefaultBrowser", "false".into()),
        ("browser.startup.homepage_override.mstone", "\"ignore\"".into()),
        ("browser.aboutwelcome.enabled", "false".into()),
        ("geo.enabled", "false".into()),
        ("privacy.fingerprintingProtection", "true".into()),
        (
            "privacy.fingerprintingProtection.overrides",
            "\"+WindowOuterSize,+ScreenRect,+ScreenAvailRect,+RoundWindowSize,+WebGLRandomization,+WebGLRendererConstant,+WebGLVendorSanitize,-NavigatorHWConcurrencyTiered\"".into(),
        ),
        ("privacy.fingerprintingProtection.remoteOverrides.enabled", "false".into()),
        ("privacy.window.maxInnerWidth", width.to_string()),
        ("privacy.window.maxInnerHeight", height.to_string()),
        ("dom.maxHardwareConcurrency", persona.cores.to_string()),
        ("intl.accept_languages", "\"en-US, en\"".into()),
        ("javascript.use_us_english_locale", "true".into()),
        ("intl.regional_prefs.use_os_locales", "false".into()),
    ];
    prefs.iter().map(|(name, value)| format!("user_pref(\"{name}\", {value});\n")).collect()
}

fn seed_brave_preferences(profile: &Path) -> Result<(), String> {
    let dir = profile.join("Default");
    let path = dir.join("Preferences");
    let fail = |e: std::io::Error| format!("could not write the browser's settings to {}: {e}; nothing was opened", path.display());
    let mut prefs = match std::fs::read_to_string(&path) {
        Ok(text) => serde_json::from_str::<serde_json::Value>(&text).ok().filter(serde_json::Value::is_object).ok_or_else(|| format!("{} is not a JSON object; nothing was opened", path.display()))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => serde_json::json!({}),
        Err(e) => return Err(fail(e)),
    };
    if !prefs["webrtc"].is_object() {
        prefs["webrtc"] = serde_json::json!({});
    }
    prefs["webrtc"]["ip_handling_policy"] = serde_json::json!(BRAVE_WEBRTC_POLICY);
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(&dir).map_err(fail)?;
    std::fs::write(&path, prefs.to_string()).map_err(fail)
}

enum Setup {
    UserJs(String),
    BravePreferences,
}

struct Launch {
    args: Vec<String>,
    env: Vec<(&'static str, String)>,
    setup: Setup,
}

impl Launch {
    fn new(kind: Kind, profile: &Path, port: u16, persona: &Persona) -> Launch {
        let mut args = profile_args(kind, profile);
        let time_zone = ("TZ", persona.time_zone.clone());
        match kind {
            Kind::Firefox => Launch { args, env: vec![time_zone, ("MOZ_CRASHREPORTER_DISABLE", "1".into())], setup: Setup::UserJs(firefox_prefs(port, persona)) },
            Kind::Brave => {
                args.extend(brave_args(port, persona));
                Launch { args, env: vec![time_zone], setup: Setup::BravePreferences }
            }
        }
    }

    fn stamp(&self) -> String {
        let setup = match &self.setup {
            Setup::UserJs(prefs) => prefs.clone(),
            Setup::BravePreferences => format!("webrtc.ip_handling_policy={BRAVE_WEBRTC_POLICY}"),
        };
        self.args.iter().cloned().chain(self.env.iter().map(|(name, value)| format!("{name}={value}"))).chain([setup]).collect::<Vec<_>>().join("\n")
    }

    fn prepare(&self, profile: &Path) -> Result<(), String> {
        match &self.setup {
            Setup::UserJs(prefs) => std::fs::write(profile.join("user.js"), prefs).map_err(|e| format!("could not write the browser's settings: {e}; nothing was opened")),
            Setup::BravePreferences => seed_brave_preferences(profile),
        }
    }
}

fn running(profile_args: &[String]) -> Result<bool, String> {
    let marker = format!("{} ", profile_args.join(" "));
    let out = std::process::Command::new("ps").args(["-ax", "-ww", "-o", "command="]).output().map_err(|e| format!("could not list this machine's processes: {e}; nothing was opened"))?;
    Ok(String::from_utf8_lossy(&out.stdout).lines().any(|line| line.contains(&marker)))
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

fn saved<T: std::str::FromStr>(profile: &Path, name: &str) -> Option<T> {
    std::fs::read_to_string(profile.join(name)).ok()?.trim().parse().ok()
}

fn free_port() -> Result<u16, String> {
    std::net::TcpListener::bind(("127.0.0.1", 0)).and_then(|l| l.local_addr()).map(|a| a.port()).map_err(|e| format!("no free local port for the browser's proxy: {e}"))
}

fn fresh_port(profile: &Path) -> Result<u16, String> {
    let dirs = profile.parent().and_then(|dir| std::fs::read_dir(dir).ok());
    let others: Vec<u16> = dirs.into_iter().flatten().flatten().map(|e| e.path()).filter(|p| p != profile).filter_map(|p| saved(&p, PROXY_PORT)).collect();
    for _ in 0..PORT_TRIES {
        let port = free_port()?;
        if !others.contains(&port) {
            return Ok(port);
        }
    }
    Err("no free local port for the browser's proxy that no other remote machine's browser uses".into())
}

fn proxy_pid(profile: &Path, host: &str) -> Option<i32> {
    let (pid, port) = (saved::<i32>(profile, PROXY_PID)?, saved::<u16>(profile, PROXY_PORT)?);
    let out = std::process::Command::new("ps").args(["-ww", "-p", &pid.to_string(), "-o", "command="]).output().ok()?;
    String::from_utf8_lossy(&out.stdout).trim_end().ends_with(&format!(" -D 127.0.0.1:{port} {host}")).then_some(pid)
}

fn locked(profile: &Path) -> std::io::Result<Flock<File>> {
    Flock::lock(lock_file(profile)?, FlockArg::LockExclusive).map_err(|(_, e)| e.into())
}

fn lock_file(profile: &Path) -> std::io::Result<File> {
    File::options().create(true).truncate(false).write(true).open(profile.join("claudette-lock"))
}

async fn lock(profile: &Path) -> Result<Flock<File>, String> {
    let fail = |e: std::io::Error| format!("could not lock {}: {e}", profile.display());
    let mut file = lock_file(profile).map_err(fail)?;
    let deadline = tokio::time::Instant::now() + LOCK_WAIT;
    loop {
        match Flock::lock(file, FlockArg::LockExclusiveNonblock) {
            Ok(lock) => return Ok(lock),
            Err((back, Errno::EWOULDBLOCK)) if tokio::time::Instant::now() < deadline => {
                file = back;
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            Err((_, Errno::EWOULDBLOCK)) => return Err(format!("another claudette session held {} for over {}s; nothing was opened", profile.display(), LOCK_WAIT.as_secs())),
            Err((_, e)) => return Err(fail(e.into())),
        }
    }
}

fn join(profile: &Path, host: &str) -> Result<(), String> {
    let mut joined = JOINED.lock().unwrap();
    if joined.iter().any(|(p, _)| p == profile) {
        return Ok(());
    }
    let fail = |e: std::io::Error| format!("could not record this session in {}: {e}", profile.display());
    let dir = profile.join(SESSIONS);
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(&dir).map_err(fail)?;
    std::fs::write(dir.join(std::process::id().to_string()), "").map_err(fail)?;
    joined.push((profile.to_owned(), host.to_owned()));
    Ok(())
}

fn alive(pid: i32) -> bool {
    matches!(kill(Pid::from_raw(pid), None), Ok(()) | Err(Errno::EPERM))
}

pub fn release() {
    for (profile, host) in JOINED.lock().unwrap().drain(..) {
        let Ok(_lock) = locked(&profile) else { continue };
        let dir = profile.join(SESSIONS);
        let _ = std::fs::remove_file(dir.join(std::process::id().to_string()));
        let mut others = false;
        for pid in std::fs::read_dir(&dir).into_iter().flatten().flatten().filter_map(|e| e.file_name().to_str()?.parse::<i32>().ok()) {
            if alive(pid) {
                others = true;
            } else {
                let _ = std::fs::remove_file(dir.join(pid.to_string()));
            }
        }
        if !others {
            stop_proxy(&profile, &host);
        }
    }
}

fn stop_proxy(profile: &Path, host: &str) {
    let pid = proxy_pid(profile, host);
    let _ = std::fs::remove_file(profile.join(PROXY_PID));
    if let Some(pid) = pid {
        let _ = killpg(Pid::from_raw(pid), Signal::SIGTERM);
    }
}

pub struct Browser {
    config: Config,
}

impl Browser {
    pub fn new(config: Config) -> Browser {
        Browser { config }
    }

    fn profile(&self, kind: Kind) -> Result<PathBuf, String> {
        let host: String = Sha256::digest(self.config.host.as_bytes()).iter().take(6).map(|b| format!("{b:02x}")).collect();
        let dir = claudette_dir().join("browser").join(format!("{}-{host}", kind.id()));
        let fail = |e: std::io::Error| format!("could not make {}: {e}", dir.display());
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(&dir).map_err(fail)?;
        dir.canonicalize().map_err(fail)
    }

    async fn proxy(&self, profile: &Path, browser_running: bool) -> Result<u16, String> {
        let host = &self.config.host;
        let saved_port = saved::<u16>(profile, PROXY_PORT);
        if let Some(port) = saved_port
            && proxy_pid(profile, host).is_some()
            && socks_ready(port).await
        {
            return Ok(port);
        }
        let port = match saved_port {
            Some(port) if browser_running => port,
            _ => fresh_port(profile)?,
        };
        std::fs::write(profile.join(PROXY_PORT), port.to_string()).map_err(|e| format!("could not save the proxy port: {e}"))?;
        let mut argv = vec![self.config.ssh[0].clone()];
        argv.extend(["-o", "ControlPath=none", "-o", "ExitOnForwardFailure=yes", "-N", "-T"].map(str::to_owned));
        argv.extend(crate::config::BATCH.iter().map(|s| (*s).to_owned()));
        argv.extend(self.config.ssh[1..].iter().cloned());
        argv.extend(["-D".to_owned(), format!("127.0.0.1:{port}"), host.clone()]);
        let mut child = tokio::process::Command::new(&argv[0])
            .args(&argv[1..])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .process_group(0)
            .spawn()
            .map_err(|e| format!("could not start ssh for the browser's proxy: {e}"))?;
        let pid = child.id().unwrap_or(0);
        let deadline = tokio::time::Instant::now() + PROXY_START;
        while tokio::time::Instant::now() < deadline {
            if let Ok(Some(status)) = child.try_wait() {
                let mut err = String::new();
                if let Some(mut stderr) = child.stderr.take() {
                    let _ = stderr.read_to_string(&mut err).await;
                }
                return Err(format!("ssh could not open the browser's proxy to {host} on local port {port} ({status}): {}", err.trim()));
            }
            if socks_ready(port).await {
                std::fs::write(profile.join(PROXY_PID), pid.to_string()).map_err(|e| format!("could not save the proxy's process id: {e}"))?;
                return Ok(port);
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let _ = child.kill().await;
        Err(format!("the browser's proxy to {host} did not start within {}s", PROXY_START.as_secs()))
    }

    pub async fn open(&self, request: &str) -> Result<(), String> {
        let open = parse_open(request)?;
        let host = &self.config.host;
        let (program, kind) = find().ok_or("neither Firefox nor Brave is installed on this machine, and claudette opens links only in one of them: install one, then open the link again; nothing was opened")?;
        let version = version(&program).await?;
        let min = kind.min_version();
        if version < min {
            return Err(format!("{} {version} is older than claudette supports ({min} or later), so nothing was opened", kind.name()));
        }
        let profile = self.profile(kind)?;
        let _lock = lock(&profile).await?;
        join(&profile, host)?;
        let running = running(&profile_args(kind, &profile))?;
        let port = self.proxy(&profile, running).await?;
        let persona = Persona::new(seed(&profile)?.as_bytes(), open.time_zone);
        let launch = Launch::new(kind, &profile, port, &persona);
        let stamp = launch.stamp();
        if running {
            if std::fs::read_to_string(profile.join(LAUNCH)).ok().as_deref() != Some(stamp.as_str()) {
                return Err(format!("the {} window claudette opened for {host} still runs with other settings, from another version of claudette or an earlier time zone of {host}: quit it, then open the link again; nothing was opened", kind.name()));
            }
        } else {
            launch.prepare(&profile)?;
            std::fs::write(profile.join(LAUNCH), &stamp).map_err(|e| format!("could not save the browser's settings: {e}; nothing was opened"))?;
        }
        let mut child = std::process::Command::new(&program)
            .args(&launch.args)
            .args(["--new-window", open.url.as_str()])
            .envs(launch.env.iter().map(|(name, value)| (name, value)))
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

    #[test]
    fn the_major_version_comes_from_each_browsers_version_line() {
        for (line, major) in [
            ("Google Chrome 158.0.8127.37 ", 158),
            ("Brave Browser 157.1.99.57", 157),
            ("Chromium 131.0.6778.85 built on Debian 12.8, running on Debian 12.8", 131),
            ("Microsoft Edge 104.0.1293.70", 104),
            ("Mozilla Firefox 128.5.0esr", 128),
            ("Mozilla Firefox 160.0.1", 160),
        ] {
            assert_eq!(major_version(line), Some(major), "{line}");
        }
        assert_eq!(major_version("Firefox"), None);
    }
}

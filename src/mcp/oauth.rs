use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{self, BufRead, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail, ensure};
use nix::fcntl::{Flock, FlockArg};
use oauth2::TokenResponse;
use reqwest::{StatusCode, Url, header};
use rmcp::transport::auth::{
    AuthError, AuthorizationCallback, AuthorizationManager, CredentialStore,
    InMemoryCredentialStore, OAuthClientConfig, StoredCredentials,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::time::timeout;

use super::{
    LATEST_PROTOCOL_VERSION, Source,
    config::{Scope, project_key},
};
use crate::config::claudette_dir;
use crate::json::Object;

const CALLBACK_PATH: &str = "/callback";
const LOGIN_TIMEOUT: Duration = Duration::from_secs(300);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

pub(super) fn supports(config: &Object) -> bool {
    matches!(
        config.get("type").and_then(Value::as_str),
        Some("http" | "streamable-http" | "sse")
    )
}

pub(super) fn client_id(config: &Object) -> Option<&str> {
    config
        .get("oauth")?
        .get("clientId")?
        .as_str()
        .filter(|id| !id.is_empty())
}

pub(super) struct Store {
    path: PathBuf,
    url: String,
}

#[derive(Default, Serialize, Deserialize)]
struct Record {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    client_secret: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    grant: Option<Grant>,
}

#[derive(Serialize, Deserialize)]
struct Grant {
    credentials: StoredCredentials,
    redirect_uri: String,
    challenge: String,
}

impl Grant {
    fn usable_token(&self) -> Option<&str> {
        let token = self.credentials.token_response.as_ref()?;
        if let (Some(received), Some(ttl)) =
            (self.credentials.token_received_at, token.expires_in())
        {
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            if received.saturating_add(ttl.as_secs()) <= now {
                return None;
            }
        }
        Some(token.access_token().secret())
    }
}

impl Store {
    pub(super) fn new(name: &str, config: &Object, cwd: &Path, source: &Source) -> Option<Self> {
        if !supports(config) {
            return None;
        }
        let url = config.get("url")?.as_str()?;
        let (scope, project) = match source {
            Source::User => ("user", None),
            Source::Local => ("local", Some(project_key(cwd))),
            Source::Project(dir) => ("project", Some(project_key(dir))),
            Source::Caller | Source::Plugin(_) => return None,
        };
        let key = json!([scope, project, name, url, client_id(config)]).to_string();
        let mut filename = String::with_capacity(69);
        let hex = b"0123456789abcdef";
        for byte in Sha256::digest(key.as_bytes()) {
            filename.push(char::from(hex[usize::from(byte >> 4)]));
            filename.push(char::from(hex[usize::from(byte & 15)]));
        }
        filename.push_str(".json");
        Some(Self {
            path: claudette_dir().join("mcp-oauth").join(filename),
            url: url.to_owned(),
        })
    }

    pub(super) fn for_scope(name: &str, config: &Object, cwd: &Path, scope: Scope) -> Option<Self> {
        let source = match scope {
            Scope::User => Source::User,
            Scope::Local => Source::Local,
            Scope::Project => Source::Project(cwd.to_path_buf()),
        };
        Self::new(name, config, cwd, &source)
    }

    fn load(&self) -> Result<Record> {
        let file = match OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(&self.path)
        {
            Ok(file) => file,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Record::default()),
            Err(e) => return Err(e).context("could not open MCP OAuth credentials"),
        };
        let meta = file.metadata()?;
        ensure!(
            meta.is_file()
                && meta.mode() & 0o077 == 0
                && meta.uid() == nix::unistd::geteuid().as_raw(),
            "MCP OAuth credentials must be private to this user"
        );
        serde_json::from_reader(file).context("could not read MCP OAuth credentials")
    }

    fn lock_file(path: &Path) -> Result<Flock<File>> {
        let dir = path
            .parent()
            .context("missing MCP OAuth credentials directory")?;
        DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
        let meta = fs::symlink_metadata(dir)?;
        ensure!(
            meta.is_dir()
                && meta.mode() & 0o077 == 0
                && meta.uid() == nix::unistd::geteuid().as_raw(),
            "MCP OAuth credentials directory must be private to this user"
        );
        let file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(path.with_extension("lock"))?;
        Flock::lock(file, FlockArg::LockExclusive)
            .map_err(|(_, e)| e)
            .context("could not lock MCP OAuth credentials")
    }

    async fn lock(&self) -> Result<Flock<File>> {
        let path = self.path.clone();
        tokio::task::spawn_blocking(move || Self::lock_file(&path)).await?
    }

    fn save(&self, record: &Record) -> Result<()> {
        let tmp = self.path.with_extension("tmp");
        unlink(&tmp)?;
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)?;
        let result = (|| -> Result<()> {
            serde_json::to_writer(&mut file, record)?;
            file.sync_all()?;
            fs::rename(&tmp, &self.path)?;
            Ok(())
        })();
        if result.is_err() {
            let _ = unlink(&tmp);
        }
        result.context("could not save MCP OAuth credentials")
    }

    pub(super) fn set_client_secret(&self, secret: String) -> Result<()> {
        checked_url(&self.url)?;
        let _lock = Self::lock_file(&self.path)?;
        self.save(&Record {
            client_secret: Some(secret),
            grant: None,
        })
    }

    pub(super) fn clear(&self) -> Result<()> {
        if !self.path.parent().is_some_and(Path::exists) {
            return Ok(());
        }
        let _lock = Self::lock_file(&self.path)?;
        unlink(&self.path.with_extension("tmp"))?;
        unlink(&self.path)
    }

    pub(super) async fn access_token(&self) -> Result<Option<String>> {
        let Some(grant) = self.load()?.grant else {
            return Ok(None);
        };
        match grant.usable_token() {
            Some(token) => Ok(Some(token.to_owned())),
            None => self.refresh(None, false).await,
        }
    }

    pub(super) async fn refresh_rejected(&self, rejected: Option<&str>) -> Result<bool> {
        if self.load()?.grant.is_none() {
            return Ok(false);
        }
        self.refresh(rejected, true)
            .await
            .map(|token| token.is_some())
    }

    async fn refresh(&self, rejected: Option<&str>, force: bool) -> Result<Option<String>> {
        let _lock = self.lock().await?;
        let mut record = self.load()?;
        let Some(grant) = &mut record.grant else {
            return Ok(None);
        };
        if let Some(token) = grant.usable_token()
            && (!force || rejected.is_some_and(|rejected| rejected != token))
        {
            return Ok(Some(token.to_owned()));
        }
        let (mut manager, store) = manager(checked_url(&self.url)?, &grant.challenge).await?;
        store.save(grant.credentials.clone()).await?;
        if !manager.initialize_from_store().await? {
            return Err(AuthError::AuthorizationRequired.into());
        }
        let mut client = OAuthClientConfig::new(&grant.credentials.client_id, &grant.redirect_uri);
        client.client_secret = record.client_secret.clone();
        manager.configure_client(client)?;
        let token = manager.refresh_token().await.map_err(|e| match e {
            AuthError::TokenRefreshRejected(_) => AuthError::AuthorizationRequired,
            AuthError::TokenRefreshFailed(_) => AuthError::TokenRefreshFailed(
                "the authorization server did not issue a new access token".to_owned(),
            ),
            e => e,
        })?;
        grant.credentials = store
            .load()
            .await?
            .context("OAuth refresh did not save credentials")?;
        self.save(&record)?;
        Ok(Some(token.access_token().secret().to_owned()))
    }

    pub(super) async fn login(
        &self,
        config: &Object,
        headers: &[(String, String)],
        no_browser: bool,
    ) -> Result<()> {
        let url = checked_url(&self.url)?;
        let port = match config.get("oauth").and_then(|o| o.get("callbackPort")) {
            None => 0,
            Some(value) => value
                .as_u64()
                .and_then(|port| u16::try_from(port).ok())
                .filter(|port| *port > 0)
                .context("oauth.callbackPort must be a whole number from 1 to 65535")?,
        };
        let callback = Callback::bind(port).await?;
        let challenge = challenge(&url, config, headers).await?;
        let (mut manager, store) = manager(url, &challenge).await?;
        let scopes = manager.select_initial_scopes(&[]).await;
        let scopes: Vec<_> = scopes.iter().map(String::as_str).collect();
        let client = if let Some(id) = client_id(config) {
            let mut client = OAuthClientConfig::new(id, callback.url.as_str());
            client.client_secret = self.load()?.client_secret;
            manager.configure_client(client.clone())?;
            client
        } else {
            manager
                .register_client("claudette", callback.url.as_str(), &scopes)
                .await?
        };
        let authorization_url = manager.get_authorization_url(&scopes).await?;
        let answer = callback.receive(&authorization_url, no_browser).await?;
        let _lock = self.lock().await?;
        manager
            .exchange_code_for_token_with_issuer(
                &answer.code,
                &answer.csrf_token,
                answer.issuer.as_deref(),
            )
            .await
            .map_err(|e| match e {
                AuthError::TokenExchangeFailed(_) => AuthError::TokenExchangeFailed(
                    "the authorization server did not issue an access token".to_owned(),
                ),
                e => e,
            })?;
        let credentials = store
            .load()
            .await?
            .context("OAuth login did not save credentials")?;
        self.save(&Record {
            client_secret: client.client_secret,
            grant: Some(Grant {
                credentials,
                redirect_uri: client.redirect_uri,
                challenge,
            }),
        })
    }
}

fn unlink(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        result => result.context("could not clear MCP OAuth credentials"),
    }
}

fn checked_url(url: &str) -> Result<Url> {
    let url = Url::parse(url).context("MCP server has an invalid URL")?;
    ensure!(
        url.username().is_empty() && url.password().is_none() && url.fragment().is_none(),
        "MCP OAuth URL must not contain user information or a fragment"
    );
    let loopback = url.host_str().is_some_and(|host| {
        host == "localhost"
            || host
                .trim_matches(['[', ']'])
                .parse::<std::net::IpAddr>()
                .is_ok_and(|ip| ip.is_loopback())
    });
    ensure!(
        url.scheme() == "https" || (url.scheme() == "http" && loopback),
        "MCP OAuth requires HTTPS except on loopback"
    );
    Ok(url)
}

async fn manager(
    url: Url,
    challenge: &str,
) -> Result<(AuthorizationManager, InMemoryCredentialStore)> {
    let mut manager = AuthorizationManager::new(url).await?;
    let resolution = manager
        .resolve_metadata_from_challenge(Some(challenge))
        .await?;
    ensure!(
        resolution.source.is_discovered(),
        "could not discover an OAuth authorization server"
    );
    ensure!(
        resolution
            .metadata
            .code_challenge_methods_supported
            .as_ref()
            .is_some_and(|methods| methods.iter().any(|method| method == "S256")),
        "authorization server must advertise PKCE S256"
    );
    manager.set_metadata(resolution.metadata);
    let store = InMemoryCredentialStore::new();
    manager.set_credential_store(store.clone());
    Ok((manager, store))
}

async fn challenge(url: &Url, config: &Object, headers: &[(String, String)]) -> Result<String> {
    let http = reqwest::Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .build()?;
    let configured = |mut request: reqwest::RequestBuilder| {
        for (key, value) in headers {
            if !key.eq_ignore_ascii_case("authorization") {
                request = request.header(key, value);
            }
        }
        request
    };
    let get = || {
        configured(
            http.get(url.clone())
                .header(header::ACCEPT, "text/event-stream"),
        )
        .send()
    };
    let response = if config.get("type").and_then(Value::as_str) == Some("sse") {
        get().await?
    } else {
        let body = json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":LATEST_PROTOCOL_VERSION,"capabilities":{},"clientInfo":{"name":"claudette","version":crate::VERSION}}});
        let response = configured(
            http.post(url.clone())
                .header(header::ACCEPT, "application/json, text/event-stream")
                .header(header::CONTENT_TYPE, "application/json")
                .header("mcp-protocol-version", LATEST_PROTOCOL_VERSION)
                .body(body.to_string()),
        )
        .send()
        .await?;
        if matches!(response.status().as_u16(), 400 | 404 | 405) {
            get().await?
        } else {
            response
        }
    };
    ensure!(
        response.status() == StatusCode::UNAUTHORIZED,
        "expected an OAuth challenge, but the server returned HTTP {}",
        response.status().as_u16()
    );
    response
        .headers()
        .get_all(header::WWW_AUTHENTICATE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .find_map(bearer_challenge)
        .map(str::to_owned)
        .context("HTTP 401 did not include a Bearer challenge")
}

fn bearer_challenge(value: &str) -> Option<&str> {
    let (mut quoted, mut escaped) = (false, false);
    let mut cuts = value.char_indices().filter_map(|(i, ch)| {
        match ch {
            _ if escaped => escaped = false,
            '\\' if quoted => escaped = true,
            '"' => quoted = !quoted,
            ',' if !quoted => return Some(i),
            _ => {}
        }
        None
    });
    let mut start = 0;
    let mut bearer = None;
    for end in cuts.by_ref().chain(std::iter::once(value.len())) {
        let part = value[start..end].trim_start();
        let word = part
            .split(|ch: char| ch.is_ascii_whitespace() || ch == '=')
            .next()
            .unwrap_or_default();
        if !word.is_empty() && !part[word.len()..].trim_start().starts_with('=') {
            if let Some(bearer) = bearer {
                return Some(value[bearer..start - 1].trim());
            }
            if word.eq_ignore_ascii_case("Bearer") {
                bearer = Some(start);
            }
        }
        start = end.saturating_add(1);
    }
    bearer.map(|start| value[start..].trim())
}

struct Callback {
    listener: TcpListener,
    url: Url,
}

impl Callback {
    async fn bind(port: u16) -> Result<Self> {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port))
            .await
            .context("could not bind OAuth callback on loopback")?;
        let port = listener.local_addr()?.port();
        let url = Url::parse(&format!("http://localhost:{port}{CALLBACK_PATH}"))?;
        Ok(Self { listener, url })
    }

    async fn receive(
        &self,
        authorization_url: &str,
        no_browser: bool,
    ) -> Result<AuthorizationCallback> {
        let auth = Url::parse(authorization_url)?;
        let state = auth
            .query_pairs()
            .find(|(name, _)| name == "state")
            .map(|(_, value)| value.into_owned())
            .context("authorization URL did not include state")?;
        let browser = self.browser_callback(&state);
        tokio::pin!(browser);
        let mut early_answer = None;
        if no_browser
            || !tokio::select! {
                opened = open_browser(authorization_url) => opened,
                answer = &mut browser => { early_answer = Some(answer); true },
            }
        {
            println!("Visit this URL to authorize:\n  {authorization_url}\n");
        }
        println!("Waiting for authorization… (^C to cancel)");
        print!("Or paste the redirect URL here: ");
        io::stdout().flush()?;
        if let Some(answer) = early_answer {
            println!();
            return answer;
        }
        let paste = async {
            let Some(pasted) = paste_redirect().await? else {
                return std::future::pending().await;
            };
            self.validate(
                &Url::parse(pasted.trim()).context("invalid redirect URL")?,
                &state,
            )
        };
        let result = timeout(LOGIN_TIMEOUT, async {
            tokio::select! {
                answer = &mut browser => answer,
                pasted = paste => pasted,
                _ = tokio::signal::ctrl_c() => bail!("authentication cancelled"),
            }
        })
        .await;
        println!();
        result.context("timed out waiting for authorization")?
    }

    fn validate(&self, url: &Url, state: &str) -> Result<AuthorizationCallback> {
        ensure!(
            url.origin() == self.url.origin()
                && url.path() == CALLBACK_PATH
                && url.username().is_empty()
                && url.password().is_none()
                && url.fragment().is_none(),
            "redirect URL does not match {}",
            self.url
        );
        let mut states = url.query_pairs().filter(|(name, _)| name == "state");
        ensure!(
            states.next().is_some_and(|(_, value)| value == state) && states.next().is_none(),
            "redirect URL has invalid OAuth state"
        );
        ensure!(
            !url.query_pairs().any(|(name, _)| name == "error"),
            "authorization denied by server"
        );
        Ok(AuthorizationCallback::from_redirect_url(url.as_str())?)
    }

    async fn browser_callback(&self, state: &str) -> Result<AuthorizationCallback> {
        loop {
            let (mut stream, _) = self.listener.accept().await?;
            let answer = timeout(Duration::from_secs(3), read_callback(&mut stream)).await;
            let Ok(Ok(target)) = answer else { continue };
            let answer = self.url.join(&target).ok().and_then(|url| {
                let matching_state = url
                    .query_pairs()
                    .any(|(name, value)| name == "state" && value == state);
                (url.path() == CALLBACK_PATH && matching_state).then(|| self.validate(&url, state))
            });
            let (status, body) = if answer.as_ref().is_some_and(Result::is_ok) {
                ("200 OK", "Callback received. Return to your terminal.")
            } else {
                ("400 Bad Request", "Not a valid OAuth callback.")
            };
            let response = format!(
                "HTTP/1.1 {status}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes()).await;
            if let Some(answer) = answer {
                return answer;
            }
        }
    }
}

async fn read_callback(stream: &mut TcpStream) -> Result<String> {
    let mut reader = BufReader::new(stream);
    let mut line = Vec::new();
    loop {
        let bytes = reader.fill_buf().await?;
        ensure!(!bytes.is_empty(), "incomplete callback request");
        let end = bytes.iter().position(|b| *b == b'\n');
        let n = end.map_or(bytes.len(), |pos| pos + 1);
        ensure!(line.len() + n <= 8192, "callback request is too long");
        line.extend_from_slice(&bytes[..n]);
        reader.consume(n);
        if end.is_some() {
            break;
        }
    }
    let mut words = std::str::from_utf8(&line)?.split_ascii_whitespace();
    ensure!(words.next() == Some("GET"), "callback request must be GET");
    Ok(words
        .next()
        .filter(|path| path.starts_with('/') && !path.starts_with("//"))
        .context("callback request has no path")?
        .to_owned())
}

async fn open_browser(url: &str) -> bool {
    let opener = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    let mut command = tokio::process::Command::new(opener);
    command
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    match timeout(Duration::from_secs(2), command.status()).await {
        Ok(status) => status.is_ok_and(|status| status.success()),
        Err(_) => true,
    }
}

async fn paste_redirect() -> Result<Option<String>> {
    let (reply, answer) = tokio::sync::oneshot::channel();
    std::thread::Builder::new()
        .name("oauth-redirect".into())
        .spawn(move || {
            let mut line = String::new();
            let result = io::stdin()
                .lock()
                .read_line(&mut line)
                .map(|n| (n != 0).then_some(line));
            let _ = reply.send(result);
        })?;
    Ok(answer.await??)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bearer_challenge_excludes_other_schemes_and_keeps_quoted_commas() {
        let header = r#"Basic realm="old, Bearer fake", Bearer realm="mcp, tools", scope="tools:read", Digest realm="other""#;
        assert_eq!(
            bearer_challenge(header),
            Some(r#"Bearer realm="mcp, tools", scope="tools:read""#)
        );
    }

    #[tokio::test]
    async fn callback_rejects_valid_state_on_the_wrong_port() {
        let callback = Callback::bind(0).await.unwrap();
        let mut redirect = callback.url.clone();
        let wrong_port = if redirect.port() == Some(1) { 2 } else { 1 };
        redirect.set_port(Some(wrong_port)).unwrap();
        redirect.set_query(Some("code=code&state=expected"));
        assert!(callback.validate(&redirect, "expected").is_err());
    }
}

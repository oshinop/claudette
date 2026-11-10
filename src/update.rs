//! `claudette +update`: replaces the builds the install script put in place with a release's, and the
//! notice of a newer release that claudette's own commands show on a terminal.
//!
//! The builds are renamed into place, so a session already running keeps the build it started with
//! (see [`crate::tunnel`], which holds its builds open), and the remote machine gets the new build on
//! the next session.

use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, bail, ensure};
use sha2::{Digest, Sha256};
use tokio::process::Command;

use crate::VERSION;
use crate::tunnel::{PLATFORMS, platform};

const REPO: &str = "oshinop/claudette";
/// How long the notice may hold up the command it follows.
pub const CHECK_TIMEOUT: Duration = Duration::from_secs(2);
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(600);

/// `0.10.1` and `v0.10.1` as numbers, so 0.10 comes after 0.9.
pub fn version_tuple(v: &str) -> Vec<u64> {
    v.trim_start_matches('v').split('.').map(|p| p.parse().unwrap_or(0)).collect()
}

fn is_newer(tag: &str, current: &str) -> bool {
    version_tuple(tag) > version_tuple(current)
}

fn client(timeout: Duration) -> anyhow::Result<reqwest::Client> {
    Ok(reqwest::Client::builder().user_agent(format!("claudette/{VERSION}")).timeout(timeout).build()?)
}

/// The latest release's tag, from where GitHub's `releases/latest` page leads; `None` before the
/// first release. Unlike GitHub's API, the page has no rate limit for clients without a token.
async fn latest_tag(client: &reqwest::Client) -> anyhow::Result<Option<String>> {
    let response = client.get(format!("https://github.com/{REPO}/releases/latest")).send().await?.error_for_status()?;
    Ok(response.url().path().rsplit_once("/releases/tag/").map(|(_, tag)| tag.to_owned()).filter(|t| !t.is_empty()))
}

/// The line claudette's own commands end with when a newer release is out. GitHub is asked each
/// time, while the command runs; any failure means no notice.
pub async fn notice(prefix: String) -> Option<String> {
    let tag = latest_tag(&client(CHECK_TIMEOUT).ok()?).await.ok()??;
    is_newer(&tag, VERSION).then(|| format!("claudette {} is out (this is {VERSION}): run `claudette {prefix}update`", tag.trim_start_matches('v')))
}

async fn fetch(client: &reqwest::Client, url: &str) -> anyhow::Result<Vec<u8>> {
    let response = client.get(url).send().await.and_then(reqwest::Response::error_for_status).with_context(|| format!("cannot download {url}"))?;
    Ok(response.bytes().await.with_context(|| format!("cannot download {url}"))?.to_vec())
}

/// The checksum `SHA256SUMS` lists for `name` (`<sum>  <name>`, or `*<name>` in binary mode).
fn listed_sum<'a>(sums: &'a str, name: &str) -> Option<&'a str> {
    sums.lines().find_map(|line| {
        let (sum, file) = line.split_once(char::is_whitespace)?;
        (file.trim().trim_start_matches('*') == name).then_some(sum)
    })
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Files written beside the builds, removed unless renamed into place.
struct Staged(Vec<(PathBuf, PathBuf)>);

impl Drop for Staged {
    fn drop(&mut self) {
        for (temp, _) in &self.0 {
            let _ = std::fs::remove_file(temp);
        }
    }
}

/// `claudette +update [version]`: the latest release, or the one named.
pub async fn update(version: Option<&str>) -> anyhow::Result<i32> {
    let exe = std::env::current_exe().and_then(std::fs::canonicalize).context("cannot find this claudette's executable")?;
    let own = format!("claudette-{}", platform());
    let dir = exe.parent().context("this claudette's executable has no folder")?;
    ensure!(
        exe.file_name().is_some_and(|n| *n == *own),
        "{} was not installed by the install script, which names the builds claudette-<platform>; update it the way it was installed",
        exe.display()
    );
    let client = client(DOWNLOAD_TIMEOUT)?;
    // A mirror, as for the install script, serves one release's files.
    let mirror = std::env::var("CLAUDETTE_DOWNLOAD_URL").ok().map(|u| u.trim().trim_end_matches('/').to_owned()).filter(|u| !u.is_empty());
    let (tag, base) = match mirror {
        Some(mirror) => {
            ensure!(version.is_none(), "CLAUDETTE_DOWNLOAD_URL serves one release, so no version can be picked");
            (None, mirror)
        }
        None => {
            let tag = match version {
                Some(v) => format!("v{}", v.trim_start_matches('v')),
                None => latest_tag(&client).await.context("cannot find the latest release")?.context("no claudette release is published yet")?,
            };
            if version.is_none() && !is_newer(&tag, VERSION) {
                println!("claudette {VERSION} is up to date");
                return Ok(0);
            }
            let base = format!("https://github.com/{REPO}/releases/download/{tag}");
            (Some(tag), base)
        }
    };
    eprintln!("Downloading claudette{} from {base}", tag.as_deref().map(|t| format!(" {t}")).unwrap_or_default());
    let sums = String::from_utf8(fetch(&client, &format!("{base}/SHA256SUMS")).await?).context("SHA256SUMS is not text")?;
    let mut builds = Vec::with_capacity(PLATFORMS.len());
    for p in PLATFORMS {
        let name = format!("claudette-{p}");
        let want = listed_sum(&sums, &name).with_context(|| format!("SHA256SUMS has no entry for {name}"))?;
        let bytes = fetch(&client, &format!("{base}/{name}")).await?;
        ensure!(hex(&Sha256::digest(&bytes)) == want, "{name} does not match its checksum");
        builds.push((name, bytes));
    }

    // Every build is written beside the old ones first, so a failure leaves the old ones whole.
    let mut staged = Staged(Vec::with_capacity(builds.len()));
    for (name, bytes) in &builds {
        let temp = dir.join(format!(".{name}.update-{}", std::process::id()));
        write_executable(&temp, bytes).with_context(|| format!("cannot write {}", temp.display()))?;
        staged.0.push((temp, dir.join(name)));
    }
    let new_own = &staged.0.iter().find(|(_, path)| path.file_name().is_some_and(|n| *n == *own)).expect("every platform is staged").0;
    let new_version = run_version(new_own).await?;
    if version.is_none() && !is_newer(&new_version, VERSION) {
        println!("claudette {VERSION} is up to date");
        return Ok(0);
    }
    // This machine's build last: until then, the link still finds a whole release.
    staged.0.sort_by_key(|(_, path)| path.file_name().is_some_and(|n| *n == *own));
    while let Some((temp, path)) = staged.0.first() {
        std::fs::rename(temp, path).with_context(|| format!("cannot replace {}", path.display()))?;
        staged.0.remove(0);
    }
    println!("Updated claudette {VERSION} to {new_version}.");
    println!("Sessions already running keep {VERSION}; the remote machine gets {new_version} on the next session.");
    Ok(0)
}

fn write_executable(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o755).open(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

/// What a downloaded build says its version is: proof that it runs here.
async fn run_version(build: &Path) -> anyhow::Result<String> {
    let out = Command::new(build).arg("+version").env("CLAUDETTE_PREFIX", "+").output().await.context("the downloaded claudette does not run here")?;
    let version = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    if !out.status.success() || version.is_empty() {
        bail!("the downloaded claudette does not run here: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(version)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_compare_as_numbers_and_a_tag_may_carry_a_v() {
        assert!(is_newer("v0.10.0", "0.9.9"));
        assert!(is_newer("0.2.0", "0.1.12"));
        assert!(!is_newer("v0.1.0", "0.1.0"));
        assert!(!is_newer("v0.1.0", "0.2.0"));
    }

    #[test]
    fn a_checksum_is_found_for_its_file_only_in_either_mode() {
        let sums = "aaa  claudette-darwin-arm64\nbbb *claudette-linux-x64\nccc  claudette-linux-x64.sig\n";
        assert_eq!(listed_sum(sums, "claudette-darwin-arm64"), Some("aaa"));
        assert_eq!(listed_sum(sums, "claudette-linux-x64"), Some("bbb"));
        assert_eq!(listed_sum(sums, "claudette-linux-arm64"), None);
    }
}

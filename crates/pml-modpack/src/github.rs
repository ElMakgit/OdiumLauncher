//! Finds the newest `.mrpack` published in a GitHub repository.
//!
//! A source stores only the repository page, for example
//! `https://github.com/ElMakgit/Odium`. On every launch the launcher asks the
//! GitHub API for the repository root first and for the latest release asset
//! second, so an author can upload `Pack 2.4.0.mrpack` and every player picks
//! it up without editing `config.json`.
//!
//! Responses are cached together with their `ETag`. GitHub does not charge a
//! conditional request answered with `304 Not Modified` against the anonymous
//! rate limit, so an unchanged pack costs one small request. A short freshness
//! window keeps the startup check and the pipeline from asking twice, and the
//! cache is also the offline fallback.

use std::cmp::Ordering;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures_util::StreamExt;
use pml_downloader::{hash_bytes, HashAlgo};
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};

use crate::ModpackError;

const API: &str = "https://api.github.com";
const ACCEPT: &str = "application/vnd.github+json";
const API_VERSION: &str = "2022-11-28";
/// Two checks closer than this reuse the cached answer. The startup refresh
/// and the pipeline start within seconds of each other; a later manual check
/// reaches GitHub again, so "Проверить обновления" stays honest. Conditional
/// `304` answers do not consume the anonymous rate limit.
const FRESH_FOR: Duration = Duration::from_secs(60);
const MAX_API_BYTES: usize = 8 * 1024 * 1024;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);

/// The archive the launcher should install for a GitHub source.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedMrpack {
    /// Direct HTTPS link to the archive.
    pub url: String,
    /// Original file name, used in logs and for the version label.
    pub file_name: String,
    /// Version label derived from the file name, for example `2.3.1`.
    pub version: String,
    /// GitHub blob SHA or release asset identity. When it changes, the cached
    /// archive must be revalidated without the old `ETag`.
    pub validator: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Origin {
    Contents,
    Release,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ResolveCache {
    source_url: String,
    fetched_at: u64,
    #[serde(default)]
    etag: Option<String>,
    origin: Origin,
    resolved: ResolvedMrpack,
}

#[derive(Debug, Deserialize)]
struct ContentEntry {
    name: String,
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    sha: String,
    #[serde(default)]
    size: u64,
    #[serde(default)]
    download_url: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Release {
    #[serde(default)]
    tag_name: String,
    #[serde(default)]
    assets: Vec<ReleaseAsset>,
}

#[derive(Debug, Deserialize)]
struct ReleaseAsset {
    name: String,
    #[serde(default)]
    id: u64,
    #[serde(default)]
    size: u64,
    #[serde(default)]
    browser_download_url: String,
    #[serde(default)]
    updated_at: String,
}

type Found = (ResolvedMrpack, Origin, Option<String>);

/// Resolves the newest `.mrpack` for a GitHub repository page.
pub async fn resolve_latest(
    client: &reqwest::Client,
    source_url: &str,
    cache_dir: &Path,
) -> Result<ResolvedMrpack, ModpackError> {
    let repo = pml_config::github_repo_from_url(source_url).ok_or_else(|| {
        ModpackError::Validation(format!(
            "источник не похож на репозиторий GitHub: {source_url}"
        ))
    })?;
    let cache_root = cache_dir.join("github");
    tokio::fs::create_dir_all(&cache_root)
        .await
        .map_err(|source| ModpackError::Io {
            path: cache_root.clone(),
            source,
        })?;
    let key = hash_bytes(HashAlgo::Sha256, source_url.as_bytes())[..16].to_owned();
    let cache_path = cache_root.join(format!("{key}.json"));
    let cached = read_cache(&cache_path)
        .await
        .filter(|cache| cache.source_url == source_url);

    if let Some(cache) = &cached {
        if cache_age(cache) < FRESH_FOR {
            return Ok(cache.resolved.clone());
        }
    }

    let origin = cached
        .as_ref()
        .map_or(Origin::Contents, |cache| cache.origin);
    let result = match origin {
        Origin::Contents => match resolve_contents(client, &repo, cached.as_ref()).await {
            Ok(Some(found)) => Ok(Some(found)),
            Ok(None) => {
                // The repository root has no archive; the author may publish
                // only release assets. Reuse a release ETag if there is one.
                let release_cache = cached
                    .as_ref()
                    .filter(|cache| cache.origin == Origin::Release);
                resolve_release(client, &repo, release_cache).await
            }
            Err(error) => Err(error),
        },
        Origin::Release => resolve_release(client, &repo, cached.as_ref()).await,
    };

    match result {
        Ok(Some((resolved, origin, etag))) => {
            write_cache(&cache_path, source_url, origin, etag, &resolved).await;
            Ok(resolved)
        }
        Ok(None) => Err(no_pack_error(&repo)),
        Err(error) => match cached {
            Some(cache) => {
                tracing::warn!(
                    %error,
                    %source_url,
                    file = %cache.resolved.file_name,
                    "GitHub resolution failed, using the cached pack"
                );
                Ok(cache.resolved)
            }
            None => Err(error),
        },
    }
}

async fn resolve_contents(
    client: &reqwest::Client,
    repo: &str,
    cache: Option<&ResolveCache>,
) -> Result<Option<Found>, ModpackError> {
    let url = format!("{API}/repos/{repo}/contents/");
    let mut request = api_request(client, &url);
    let cached = cache.filter(|cache| cache.origin == Origin::Contents);
    if let Some(cache) = cached {
        if let Some(etag) = &cache.etag {
            request = request.header(reqwest::header::IF_NONE_MATCH, etag);
        }
    }
    let response = request.send().await?;
    let etag = header_value(&response, reqwest::header::ETAG);

    if response.status() == StatusCode::NOT_MODIFIED {
        return match cached {
            Some(cache) => Ok(Some((
                cache.resolved.clone(),
                Origin::Contents,
                etag.or_else(|| cache.etag.clone()),
            ))),
            None => Err(ModpackError::Validation(
                "GitHub ответил 304 без локального кэша".to_owned(),
            )),
        };
    }

    let status = response.status();
    if status == StatusCode::NOT_FOUND {
        // An empty repository answers 404 here; release assets may still
        // exist, so let the caller try `releases/latest`.
        return Ok(None);
    }
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        return Err(api_error("список файлов репозитория", status, &body));
    }

    let entries: Vec<ContentEntry> = serde_json::from_slice(&read_limited(response).await?)
        .map_err(|error| ModpackError::Validation(format!("ответ GitHub не разобран: {error}")))?;
    let Some(entry) = entries
        .iter()
        .filter(|entry| {
            entry.kind == "file"
                && is_mrpack_name(&entry.name)
                && entry
                    .download_url
                    .as_deref()
                    .is_some_and(|url| url.starts_with("https://"))
        })
        .max_by(|left, right| natural_cmp(&left.name, &right.name))
    else {
        return Ok(None);
    };

    let url = entry
        .download_url
        .clone()
        .ok_or_else(|| ModpackError::Validation("у файла нет ссылки на скачивание".to_owned()))?;
    let validator = if entry.sha.is_empty() {
        format!("contents:{}:{}", entry.name, entry.size)
    } else {
        format!("blob:{}", entry.sha)
    };
    let resolved = ResolvedMrpack {
        url,
        file_name: entry.name.clone(),
        version: version_label(&entry.name),
        validator,
    };
    tracing::info!(
        %repo,
        file = %resolved.file_name,
        version = %resolved.version,
        "resolved the newest .mrpack in the repository root"
    );
    Ok(Some((resolved, Origin::Contents, etag)))
}

async fn resolve_release(
    client: &reqwest::Client,
    repo: &str,
    cache: Option<&ResolveCache>,
) -> Result<Option<Found>, ModpackError> {
    let url = format!("{API}/repos/{repo}/releases/latest");
    let mut request = api_request(client, &url);
    let cached = cache.filter(|cache| cache.origin == Origin::Release);
    if let Some(cache) = cached {
        if let Some(etag) = &cache.etag {
            request = request.header(reqwest::header::IF_NONE_MATCH, etag);
        }
    }
    let response = request.send().await?;
    let etag = header_value(&response, reqwest::header::ETAG);

    if response.status() == StatusCode::NOT_MODIFIED {
        return match cached {
            Some(cache) => Ok(Some((
                cache.resolved.clone(),
                Origin::Release,
                etag.or_else(|| cache.etag.clone()),
            ))),
            None => Err(ModpackError::Validation(
                "GitHub ответил 304 без локального кэша".to_owned(),
            )),
        };
    }

    let status = response.status();
    if status == StatusCode::NOT_FOUND {
        // The repository itself exists (the contents request succeeded), so a
        // missing `releases/latest` simply means there are no releases yet.
        return Ok(None);
    }
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        return Err(api_error("последний релиз", status, &body));
    }

    let release: Release = serde_json::from_slice(&read_limited(response).await?)
        .map_err(|error| ModpackError::Validation(format!("ответ GitHub не разобран: {error}")))?;
    let Some(asset) = release
        .assets
        .iter()
        .filter(|asset| {
            is_mrpack_name(&asset.name) && asset.browser_download_url.starts_with("https://")
        })
        .max_by(|left, right| natural_cmp(&left.name, &right.name))
    else {
        return Ok(None);
    };

    let validator = if asset.id != 0 {
        format!("asset:{}:{}", asset.id, asset.size)
    } else if !asset.updated_at.is_empty() {
        format!("updated:{}:{}", asset.updated_at, asset.size)
    } else {
        format!("size:{}", asset.size)
    };
    let version = {
        let from_name = version_label(&asset.name);
        if from_name == "github" && !release.tag_name.is_empty() {
            version_label(&release.tag_name)
        } else {
            from_name
        }
    };
    let resolved = ResolvedMrpack {
        url: asset.browser_download_url.clone(),
        file_name: asset.name.clone(),
        version,
        validator,
    };
    tracing::info!(
        %repo,
        file = %resolved.file_name,
        version = %resolved.version,
        tag = %release.tag_name,
        "resolved the newest .mrpack from the latest release"
    );
    Ok(Some((resolved, Origin::Release, etag)))
}

fn api_request(client: &reqwest::Client, url: &str) -> reqwest::RequestBuilder {
    client
        .get(url)
        .header(reqwest::header::ACCEPT, ACCEPT)
        .header("X-GitHub-Api-Version", API_VERSION)
        .timeout(REQUEST_TIMEOUT)
}

async fn read_limited(response: reqwest::Response) -> Result<Vec<u8>, ModpackError> {
    let mut body = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        if body.len().saturating_add(chunk.len()) > MAX_API_BYTES {
            return Err(ModpackError::Validation(format!(
                "ответ GitHub больше {} МиБ",
                MAX_API_BYTES / (1024 * 1024)
            )));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

fn header_value(response: &reqwest::Response, name: reqwest::header::HeaderName) -> Option<String> {
    response
        .headers()
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
}

async fn read_cache(path: &Path) -> Option<ResolveCache> {
    let bytes = tokio::fs::read(path).await.ok()?;
    serde_json::from_slice(&bytes).ok()
}

async fn write_cache(
    path: &Path,
    source_url: &str,
    origin: Origin,
    etag: Option<String>,
    resolved: &ResolvedMrpack,
) {
    let cache = ResolveCache {
        source_url: source_url.to_owned(),
        fetched_at: now_secs(),
        etag,
        origin,
        resolved: resolved.clone(),
    };
    let Ok(json) = serde_json::to_vec(&cache) else {
        return;
    };
    let temporary: PathBuf = path.with_extension("json.tmp");
    if tokio::fs::write(&temporary, json).await.is_ok() {
        let _ = tokio::fs::rename(&temporary, path).await;
    }
}

fn cache_age(cache: &ResolveCache) -> Duration {
    Duration::from_secs(now_secs().saturating_sub(cache.fetched_at))
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

fn api_error(what: &str, status: StatusCode, body: &str) -> ModpackError {
    let hint = match status {
        StatusCode::FORBIDDEN if body.to_ascii_lowercase().contains("rate limit") => {
            " (исчерпан лимит запросов GitHub, попробуйте позже)"
        }
        StatusCode::NOT_FOUND => " (репозиторий не найден или закрыт)",
        _ => "",
    };
    ModpackError::Validation(format!("GitHub: {what}: HTTP {status}{hint}"))
}

fn no_pack_error(repo: &str) -> ModpackError {
    ModpackError::Validation(format!(
        "в репозитории {repo} нет .mrpack: загрузите архив в корень репозитория \
         или прикрепите его к последнему релизу"
    ))
}

fn is_mrpack_name(name: &str) -> bool {
    name.to_ascii_lowercase().ends_with(".mrpack")
}

/// Natural comparison so `2.10` sorts after `2.9` instead of before it, and
/// `2.3.1` sorts after `2.3`. File names are the only version source GitHub
/// gives us for repository contents.
fn natural_cmp(left: &str, right: &str) -> Ordering {
    natural_key(left).cmp(&natural_key(right))
}

#[derive(Debug, PartialEq, Eq)]
enum Component {
    Number(Vec<u8>),
    Text(String),
}

impl Ord for Component {
    fn cmp(&self, other: &Self) -> Ordering {
        match (self, other) {
            (Component::Number(left), Component::Number(right)) => {
                left.len().cmp(&right.len()).then_with(|| left.cmp(right))
            }
            (Component::Text(left), Component::Text(right)) => left.cmp(right),
            (Component::Number(_), Component::Text(_)) => Ordering::Less,
            (Component::Text(_), Component::Number(_)) => Ordering::Greater,
        }
    }
}

impl PartialOrd for Component {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

fn natural_key(value: &str) -> Vec<Component> {
    let stem = without_mrpack_extension(value);
    let bytes = stem.as_bytes();
    let mut key = Vec::new();
    let mut index = 0usize;
    while index < bytes.len() {
        if bytes[index].is_ascii_digit() {
            let start = index;
            while index < bytes.len() && bytes[index].is_ascii_digit() {
                index += 1;
            }
            key.push(Component::Number(
                trim_leading_zeros(&bytes[start..index]).to_vec(),
            ));
        } else if bytes[index].is_ascii_alphabetic() {
            let start = index;
            while index < bytes.len() && bytes[index].is_ascii_alphabetic() {
                index += 1;
            }
            key.push(Component::Text(stem[start..index].to_ascii_lowercase()));
        } else {
            index += 1;
        }
    }
    key
}

fn without_mrpack_extension(name: &str) -> &str {
    let bytes = name.as_bytes();
    if bytes.len() >= 7 && bytes[bytes.len() - 7..].eq_ignore_ascii_case(b".mrpack") {
        &name[..name.len() - 7]
    } else {
        name
    }
}

fn trim_leading_zeros(value: &[u8]) -> &[u8] {
    let first = value
        .iter()
        .position(|byte| *byte != b'0')
        .unwrap_or(value.len());
    &value[first..]
}

/// Turns `EblyaCraft Don's Edit 2.3.1.mrpack` into `2.3.1` and a nameless
/// `pack.mrpack` into `pack`. The result is safe for `Manifest::version`, which
/// is also used for backup decisions and must not contain spaces or slashes.
fn version_label(file_name: &str) -> String {
    let stem = without_mrpack_extension(file_name);
    let token = stem
        .split(|character: char| character.is_whitespace() || character == '-' || character == '_')
        .rev()
        .find(|token| token.chars().any(|character| character.is_ascii_digit()));
    let label = token.unwrap_or(stem).trim_start_matches(['v', 'V']);
    let mut cleaned = String::new();
    for character in label.chars() {
        if character.is_ascii_alphanumeric() || matches!(character, '.' | '+' | '-') {
            if cleaned.len() >= 64 {
                break;
            }
            cleaned.push(character);
        } else if !cleaned.is_empty() && !cleaned.ends_with('-') {
            cleaned.push('-');
        }
    }
    let cleaned = cleaned.trim_matches(['-', '.']);
    if cleaned.is_empty() {
        "github".to_owned()
    } else {
        cleaned.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn natural_sort_puts_the_higher_version_last() {
        let mut names = vec![
            "Pack 2.9.mrpack",
            "Pack 2.10.mrpack",
            "Pack 2.3.1.mrpack",
            "Pack 2.3.mrpack",
        ];
        names.sort_by(|left, right| natural_cmp(left, right));
        assert_eq!(
            names,
            vec![
                "Pack 2.3.mrpack",
                "Pack 2.3.1.mrpack",
                "Pack 2.9.mrpack",
                "Pack 2.10.mrpack",
            ]
        );
    }

    #[test]
    fn version_labels_are_safe() {
        assert_eq!(version_label("EblyaCraft Don's Edit 2.3.1.mrpack"), "2.3.1");
        assert_eq!(version_label("pack-2.10.mrpack"), "2.10");
        assert_eq!(version_label("v2.3.1.mrpack"), "2.3.1");
        assert_eq!(version_label("My Pack.mrpack"), "My-Pack");
        assert_eq!(version_label(".mrpack"), "github");
    }

    #[test]
    fn newest_content_entry_wins() {
        let entries = [
            ContentEntry {
                name: "Pack 2.9.mrpack".to_owned(),
                kind: "file".to_owned(),
                sha: "a".to_owned(),
                size: 1,
                download_url: Some("https://example.com/2.9.mrpack".to_owned()),
            },
            ContentEntry {
                name: "Pack 2.10.mrpack".to_owned(),
                kind: "file".to_owned(),
                sha: "b".to_owned(),
                size: 1,
                download_url: Some("https://example.com/2.10.mrpack".to_owned()),
            },
            ContentEntry {
                name: "notes.txt".to_owned(),
                kind: "file".to_owned(),
                sha: "c".to_owned(),
                size: 1,
                download_url: Some("https://example.com/notes.txt".to_owned()),
            },
        ];
        let picked = entries
            .iter()
            .filter(|entry| is_mrpack_name(&entry.name))
            .max_by(|left, right| natural_cmp(&left.name, &right.name))
            .expect("a pack");
        assert_eq!(picked.name, "Pack 2.10.mrpack");
    }
}

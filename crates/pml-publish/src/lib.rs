//! GitHub publishing and portable player distribution for Odium Launcher.
//!
//! Two pack formats are supported:
//!
//! * `.mrpack` — uploaded as a GitHub release asset. Players download the
//!   archive straight from the release URL and the launcher converts its index
//!   into an instance.
//! * manifest packs — `manifest.json` plus `overrides/**` are pushed through the
//!   Git Data API in a single commit; the manifest then references raw GitHub
//!   URLs for the files hosted by the publisher.
//!
//! Both flows can additionally build a ready-to-send player folder (`dist/`)
//! with the launcher executable, a preconfigured `launcher_data/config.json`,
//! `README.txt` and a ZIP archive.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use futures_util::{StreamExt, TryStreamExt};
use pml_config::{save_json_atomic, Config, ServerSpec, Source, SourceKind};
use pml_downloader::HashAlgo;
use reqwest::header::{self, HeaderValue};
use reqwest::{Method, StatusCode};
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::{json, Value};
use thiserror::Error;

const API: &str = "https://api.github.com";
const USER_AGENT: &str = "OdiumLauncher-publisher";
const UPLOAD_CHUNK: usize = 64 * 1024;
const BLOB_CONCURRENCY: usize = 8;
/// GitHub can be slow when many blobs are uploaded; a stalled socket must not
/// block the publisher forever.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);
const MAX_ATTEMPTS: usize = 3;

/// Progress callback payload. The UI maps these to a log line or a progress bar.
#[derive(Debug, Clone)]
pub enum Progress {
    Stage(String),
    Upload { sent: u64, total: u64 },
}

type Reporter = Arc<dyn Fn(Progress) + Send + Sync>;

fn retryable_status(status: StatusCode) -> bool {
    status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error()
}

fn retryable_network(error: &reqwest::Error) -> bool {
    error.is_timeout() || error.is_connect() || error.is_request() || error.is_body()
}

/// Exponential backoff with ±20% jitter so parallel blob uploads do not retry
/// in lockstep after a rate limit.
fn retry_delay(attempt: usize) -> Duration {
    let base = 500u64 << attempt.min(6);
    let spread = base / 5;
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.subsec_nanos())
        .unwrap_or(0) as u64;
    let jitter = if spread == 0 {
        0
    } else {
        nanos % (spread * 2 + 1)
    };
    Duration::from_millis(base - spread + jitter)
}

fn stage(report: &Reporter, text: impl Into<String>) {
    report(Progress::Stage(text.into()));
}

#[derive(Debug, Error)]
pub enum PublishError {
    #[error("некорректный репозиторий: {0}")]
    Repo(String),
    #[error("некорректный GitHub-токен: {0}")]
    Token(String),
    #[error("файл не найден: {0}")]
    MissingFile(String),
    #[error("GitHub: {0}")]
    Api(String),
    #[error("сеть: {0}")]
    Http(#[from] reqwest::Error),
    #[error("файл: {0}")]
    Io(#[from] std::io::Error),
    #[error("JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("config.json: {0}")]
    Config(#[from] pml_config::ConfigError),
    #[error("хеш файла: {0}")]
    Hash(#[from] pml_downloader::DownloadError),
    #[error("ZIP: {0}")]
    Zip(String),
}

/// `owner/name` pair parsed from user input such as `name/repo`,
/// `https://github.com/name/repo` or `name/repo.git`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Repo {
    pub owner: String,
    pub name: String,
}

impl Repo {
    pub fn parse(input: &str) -> Result<Self, PublishError> {
        let trimmed = input.trim().trim_end_matches('/');
        let trimmed = trimmed
            .strip_prefix("https://github.com/")
            .or_else(|| trimmed.strip_prefix("http://github.com/"))
            .unwrap_or(trimmed);
        let mut parts = trimmed.split('/');
        let owner = parts.next().unwrap_or_default().trim();
        let name = parts
            .next()
            .unwrap_or_default()
            .trim()
            .trim_end_matches(".git");
        if owner.is_empty() || name.is_empty() || parts.next().is_some() {
            return Err(PublishError::Repo(format!(
                "ожидается owner/name, получено: {input}"
            )));
        }
        for value in [owner, name] {
            if !value
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
            {
                return Err(PublishError::Repo(format!(
                    "недопустимое имя репозитория: {value}"
                )));
            }
        }
        Ok(Self {
            owner: owner.to_owned(),
            name: name.to_owned(),
        })
    }

    pub fn slug(&self) -> String {
        format!("{}/{}", self.owner, self.name)
    }
}

/// A loaded `.mrpack` publish request.
#[derive(Debug, Clone)]
pub struct MrpackPublish {
    pub repo: String,
    pub tag: String,
    pub release_name: String,
    pub release_body: String,
    pub archive: PathBuf,
    pub asset_name: String,
    pub create_repo: bool,
    pub private: bool,
}

/// A manifest pack (`manifest.json` + `overrides/`) publish request.
#[derive(Debug, Clone)]
pub struct ManifestPublish {
    pub repo: String,
    pub branch: String,
    pub pack_dir: PathBuf,
    pub commit_message: String,
    pub create_repo: bool,
    pub private: bool,
}

#[derive(Debug, Clone)]
pub struct PublishResult {
    /// URL the launcher must put into `Source.manifest_url`.
    pub pack_url: String,
    /// Human-readable page: the release or the repository tree.
    pub page_url: String,
}

/// Minimal GitHub REST client. The token is kept in memory only.
pub struct Publisher {
    client: reqwest::Client,
    token: String,
}

impl Publisher {
    pub fn new(token: impl Into<String>) -> Result<Self, PublishError> {
        // Copy-pasted tokens often carry a trailing newline or spaces. Reject a
        // token that is blank or contains a header-breaking character instead
        // of silently sending an empty `Authorization` header, which turns a
        // clear input mistake into a confusing 401 from GitHub.
        let token = token.into().trim().to_owned();
        if token.is_empty() {
            return Err(PublishError::Token("токен пуст".to_owned()));
        }
        if token.len() > 512 || !token.chars().all(|character| character.is_ascii_graphic()) {
            return Err(PublishError::Token(
                "токен содержит пробелы или недопустимые символы".to_owned(),
            ));
        }
        let client = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            .build()?;
        Ok(Self { client, token })
    }

    fn authorized(&self, builder: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        let value = HeaderValue::from_str(&format!("Bearer {}", self.token))
            .unwrap_or_else(|_| HeaderValue::from_static(""));
        builder
            .header(header::AUTHORIZATION, value)
            .header(header::ACCEPT, "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
    }

    fn request(&self, method: Method, url: &str) -> reqwest::RequestBuilder {
        self.authorized(self.client.request(method, url))
    }

    async fn send_raw(
        &self,
        request: reqwest::RequestBuilder,
    ) -> Result<(StatusCode, String), PublishError> {
        let mut last_error: Option<PublishError> = None;
        for attempt in 0..MAX_ATTEMPTS {
            let Some(attempt_request) = request.try_clone() else {
                return Err(PublishError::Api(
                    "не удалось повторить запрос: тело запроса нельзя клонировать".to_owned(),
                ));
            };
            match attempt_request.send().await {
                Ok(response) => {
                    let status = response.status();
                    let text = match response.text().await {
                        Ok(text) => text,
                        Err(error) => {
                            if attempt + 1 < MAX_ATTEMPTS {
                                last_error = Some(PublishError::Http(error));
                                let delay = retry_delay(attempt);
                                tracing::warn!(
                                    attempt = attempt + 1,
                                    delay_ms = delay.as_millis(),
                                    "GitHub response body failed, retrying"
                                );
                                tokio::time::sleep(delay).await;
                                continue;
                            }
                            return Err(error.into());
                        }
                    };
                    if attempt + 1 < MAX_ATTEMPTS && retryable_status(status) {
                        let delay = retry_delay(attempt);
                        tracing::warn!(
                            attempt = attempt + 1,
                            status = status.as_u16(),
                            delay_ms = delay.as_millis(),
                            "GitHub request failed, retrying"
                        );
                        last_error = Some(api_error("запрос к GitHub", status, &text));
                        tokio::time::sleep(delay).await;
                        continue;
                    }
                    return Ok((status, text));
                }
                Err(error) => {
                    if attempt + 1 < MAX_ATTEMPTS && retryable_network(&error) {
                        let delay = retry_delay(attempt);
                        tracing::warn!(
                            attempt = attempt + 1,
                            delay_ms = delay.as_millis(),
                            %error,
                            "GitHub network request failed, retrying"
                        );
                        last_error = Some(PublishError::Http(error));
                        tokio::time::sleep(delay).await;
                        continue;
                    }
                    return Err(error.into());
                }
            }
        }
        Err(last_error.unwrap_or_else(|| {
            PublishError::Api("запрос к GitHub не удался без конкретной ошибки".to_owned())
        }))
    }

    async fn ensure_repo(
        &self,
        repo: &Repo,
        create: bool,
        private: bool,
        report: &Reporter,
    ) -> Result<(), PublishError> {
        stage(report, format!("Проверка репозитория {}…", repo.slug()));
        let (status, text) = self
            .send_raw(self.request(Method::GET, &format!("{API}/repos/{}", repo.slug())))
            .await?;
        if status.is_success() {
            return Ok(());
        }
        if status != StatusCode::NOT_FOUND {
            return Err(api_error("проверка репозитория", status, &text));
        }
        if !create {
            return Err(PublishError::Api(format!(
                "репозиторий {} не найден. Включите «Создать репозиторий».",
                repo.slug()
            )));
        }

        let (status, text) = self
            .send_raw(self.request(Method::GET, &format!("{API}/user")))
            .await?;
        if !status.is_success() {
            return Err(api_error("проверка токена", status, &text));
        }
        #[derive(Deserialize)]
        struct User {
            login: String,
        }
        let user: User = parse_json(&text)?;
        if !user.login.eq_ignore_ascii_case(&repo.owner) {
            return Err(PublishError::Api(format!(
                "токен принадлежит {}, поэтому репозиторий можно создать только как {}/{}. \
                 Для чужой организации создайте репозиторий вручную.",
                user.login, user.login, repo.name
            )));
        }

        stage(report, format!("Создание репозитория {}…", repo.slug()));
        let body = json!({
            "name": repo.name,
            "private": private,
            "description": "Odium Launcher modpack",
            "auto_init": false,
        });
        let (status, text) = self
            .send_raw(
                self.request(Method::POST, &format!("{API}/user/repos"))
                    .json(&body),
            )
            .await?;
        if !status.is_success() {
            return Err(api_error("создание репозитория", status, &text));
        }
        Ok(())
    }

    /// Uploads an `.mrpack` as a GitHub release asset.
    pub async fn publish_mrpack<F>(
        &self,
        request: &MrpackPublish,
        report: F,
    ) -> Result<PublishResult, PublishError>
    where
        F: Fn(Progress) + Send + Sync + 'static,
    {
        let report: Reporter = Arc::new(report);
        let repo = Repo::parse(&request.repo)?;
        let tag = sanitize_tag(&request.tag);
        let asset_name = sanitize_asset_name(&request.asset_name);
        if !request.archive.is_file() {
            return Err(PublishError::MissingFile(
                request.archive.display().to_string(),
            ));
        }

        self.ensure_repo(&repo, request.create_repo, request.private, &report)
            .await?;

        stage(&report, format!("Создание релиза {tag}…"));
        let release_body = json!({
            "tag_name": tag,
            "name": if request.release_name.trim().is_empty() { tag.as_str() } else { request.release_name.as_str() },
            "body": request.release_body,
            "draft": false,
            "prerelease": false,
        });
        let releases_url = format!("{API}/repos/{}/releases", repo.slug());
        let (status, text) = self
            .send_raw(
                self.request(Method::POST, &releases_url)
                    .json(&release_body),
            )
            .await?;
        let release: Release = if status == StatusCode::UNPROCESSABLE_ENTITY {
            stage(&report, "Релиз уже существует, обновляем ассеты…");
            let url = format!("{API}/repos/{}/releases/tags/{tag}", repo.slug());
            let (status, text) = self.send_raw(self.request(Method::GET, &url)).await?;
            if !status.is_success() {
                return Err(api_error("поиск релиза", status, &text));
            }
            parse_json(&text)?
        } else if status.is_success() {
            parse_json(&text)?
        } else {
            return Err(api_error("создание релиза", status, &text));
        };

        // Re-uploading the same version must not fail with "asset already
        // exists", so remove the previous asset with this name first. The list
        // endpoint is paginated; a release with more than 30 assets used to
        // hide the old copy on page 2.
        self.delete_asset_named(&repo, release.id, &asset_name, &report)
            .await?;

        stage(&report, format!("Загрузка {}…", asset_name));
        let total = tokio::fs::metadata(&request.archive).await?.len();

        let upload_base = release
            .upload_url
            .split('{')
            .next()
            .unwrap_or(&release.upload_url)
            .to_owned();
        let asset = self
            .upload_asset(
                &repo,
                release.id,
                &upload_base,
                &asset_name,
                &request.archive,
                total,
                &report,
            )
            .await?;
        stage(&report, "Готово");
        report(Progress::Upload { sent: total, total });

        Ok(PublishResult {
            pack_url: asset.browser_download_url,
            page_url: release.html_url,
        })
    }

    /// Lists every asset of a release, following GitHub's `per_page` pages.
    async fn list_release_assets(
        &self,
        repo: &Repo,
        release_id: u64,
    ) -> Result<Vec<Asset>, PublishError> {
        let mut assets = Vec::new();
        for page in 1..=100u32 {
            let url = format!(
                "{API}/repos/{}/releases/{release_id}/assets?per_page=100&page={page}",
                repo.slug()
            );
            let (status, text) = self.send_raw(self.request(Method::GET, &url)).await?;
            if !status.is_success() {
                return Err(api_error("список ассетов релиза", status, &text));
            }
            let page_assets: Vec<Asset> = parse_json(&text)?;
            let received = page_assets.len();
            assets.extend(page_assets);
            if received < 100 {
                break;
            }
        }
        Ok(assets)
    }

    async fn delete_asset_named(
        &self,
        repo: &Repo,
        release_id: u64,
        asset_name: &str,
        report: &Reporter,
    ) -> Result<(), PublishError> {
        for asset in self
            .list_release_assets(repo, release_id)
            .await?
            .into_iter()
            .filter(|asset| asset.name.eq_ignore_ascii_case(asset_name))
        {
            stage(report, format!("Удаление старого ассета {}…", asset.name));
            let url = format!("{API}/repos/{}/releases/assets/{}", repo.slug(), asset.id);
            let (status, text) = self.send_raw(self.request(Method::DELETE, &url)).await?;
            if !status.is_success() && status != StatusCode::NOT_FOUND {
                return Err(api_error("удаление старого ассета", status, &text));
            }
        }
        Ok(())
    }

    /// Uploads the archive with a fresh streaming body on every attempt.
    /// `reqwest::Body::wrap_stream` is not cloneable, so the previous generic
    /// retry always failed before the first socket write.
    #[allow(clippy::too_many_arguments)]
    async fn upload_asset(
        &self,
        repo: &Repo,
        release_id: u64,
        upload_base: &str,
        asset_name: &str,
        archive: &Path,
        total: u64,
        report: &Reporter,
    ) -> Result<Asset, PublishError> {
        let mut last_error: Option<PublishError> = None;
        for attempt in 0..MAX_ATTEMPTS {
            let file = tokio::fs::File::open(archive).await?;
            let stream_report = report.clone();
            let stream = futures_util::stream::try_unfold((file, 0u64), move |(mut file, sent)| {
                let stream_report = stream_report.clone();
                async move {
                    use tokio::io::AsyncReadExt as _;

                    let mut buffer = vec![0u8; UPLOAD_CHUNK];
                    let read = file.read(&mut buffer).await?;
                    if read == 0 {
                        return Ok::<_, std::io::Error>(None);
                    }
                    buffer.truncate(read);
                    let sent = sent + read as u64;
                    stream_report(Progress::Upload { sent, total });
                    Ok(Some((buffer, (file, sent))))
                }
            });

            let upload = self.authorized(
                self.client
                    .post(upload_base)
                    .query(&[("name", asset_name)])
                    .header(header::CONTENT_TYPE, "application/octet-stream")
                    .header(header::CONTENT_LENGTH, total)
                    .body(reqwest::Body::wrap_stream(stream)),
            );
            match upload.send().await {
                Ok(response) => {
                    let status = response.status();
                    let text = match response.text().await {
                        Ok(text) => text,
                        Err(error) if retryable_network(&error) && attempt + 1 < MAX_ATTEMPTS => {
                            let delay = retry_delay(attempt);
                            tracing::warn!(
                                attempt = attempt + 1,
                                delay_ms = delay.as_millis(),
                                "GitHub upload response failed, retrying"
                            );
                            last_error = Some(PublishError::Http(error));
                            tokio::time::sleep(delay).await;
                            continue;
                        }
                        Err(error) => return Err(error.into()),
                    };
                    if status.is_success() {
                        return parse_json(&text);
                    }
                    if status == StatusCode::UNPROCESSABLE_ENTITY && text.contains("already_exists")
                    {
                        // Another publisher (or a previous interrupted run)
                        // created the asset between our list and this upload.
                        self.delete_asset_named(repo, release_id, asset_name, report)
                            .await?;
                        last_error = Some(api_error("загрузка ассета", status, &text));
                        continue;
                    }
                    if retryable_status(status) && attempt + 1 < MAX_ATTEMPTS {
                        let delay = retry_delay(attempt);
                        tracing::warn!(
                            attempt = attempt + 1,
                            status = status.as_u16(),
                            delay_ms = delay.as_millis(),
                            "GitHub upload failed, retrying"
                        );
                        last_error = Some(api_error("загрузка ассета", status, &text));
                        tokio::time::sleep(delay).await;
                        continue;
                    }
                    return Err(api_error("загрузка ассета", status, &text));
                }
                Err(error) => {
                    if retryable_network(&error) && attempt + 1 < MAX_ATTEMPTS {
                        let delay = retry_delay(attempt);
                        tracing::warn!(
                            attempt = attempt + 1,
                            delay_ms = delay.as_millis(),
                            %error,
                            "GitHub upload network error, retrying"
                        );
                        last_error = Some(PublishError::Http(error));
                        tokio::time::sleep(delay).await;
                        continue;
                    }
                    return Err(error.into());
                }
            }
        }
        Err(last_error.unwrap_or_else(|| {
            PublishError::Api("загрузка ассета не удалась без конкретной ошибки".to_owned())
        }))
    }

    /// Pushes `manifest.json` and `overrides/**` in one commit and returns the
    /// raw manifest URL.
    pub async fn publish_manifest<F>(
        &self,
        request: &ManifestPublish,
        report: F,
    ) -> Result<PublishResult, PublishError>
    where
        F: Fn(Progress) + Send + Sync + 'static,
    {
        let report: Reporter = Arc::new(report);
        let repo = Repo::parse(&request.repo)?;
        let branch = sanitize_branch(&request.branch);
        let manifest_path = request.pack_dir.join("manifest.json");
        if !manifest_path.is_file() {
            return Err(PublishError::MissingFile(
                manifest_path.display().to_string(),
            ));
        }

        self.ensure_repo(&repo, request.create_repo, request.private, &report)
            .await?;

        stage(&report, "Подготовка manifest.json…");
        let raw_base = format!(
            "https://raw.githubusercontent.com/{}/{}/{branch}",
            repo.owner, repo.name
        );
        let mut manifest: Value = serde_json::from_slice(&tokio::fs::read(&manifest_path).await?)?;
        let overrides = collect_overrides(&request.pack_dir.join("overrides"))?;
        let (rewritten, added) = rewrite_manifest(&mut manifest, &raw_base, &overrides).await?;
        stage(
            &report,
            format!(
                "Manifest: {} файлов, {rewritten} URL обновлено, {added} добавлено",
                overrides.len()
            ),
        );
        let manifest_bytes = serde_json::to_vec_pretty(&manifest)?;

        let mut blobs: Vec<(String, BlobSource)> = vec![(
            "manifest.json".to_owned(),
            BlobSource::Bytes(manifest_bytes),
        )];
        for (relative, path) in &overrides {
            blobs.push((
                format!("overrides/{relative}"),
                BlobSource::Path(path.clone()),
            ));
        }

        stage(&report, "Чтение состояния ветки…");
        let parent = self.branch_state(&repo, &branch, &report).await?;

        stage(
            &report,
            format!("Загрузка {} файлов в GitHub…", blobs.len()),
        );
        let total = blobs.len();
        let completed = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let repo_for_blobs = repo.clone();
        let entries = futures_util::stream::iter(blobs)
            .map(|(repo_path, source)| {
                let report = report.clone();
                let repo = repo_for_blobs.clone();
                let completed = completed.clone();
                async move {
                    let bytes = match source {
                        BlobSource::Bytes(bytes) => bytes,
                        BlobSource::Path(path) => tokio::fs::read(path).await?,
                    };
                    let sha = self.create_blob(&repo, &bytes).await?;
                    let done = completed.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                    stage(&report, format!("Загружено {done}/{total} файлов…"));
                    Ok::<_, PublishError>((repo_path, sha))
                }
            })
            .buffer_unordered(BLOB_CONCURRENCY)
            .try_collect::<Vec<_>>()
            .await?;

        stage(&report, "Создание дерева и коммита…");
        let tree_entries: Vec<Value> = entries
            .iter()
            .map(|(path, sha)| json!({"path": path, "mode": "100644", "type": "blob", "sha": sha}))
            .collect();
        let mut tree_body = json!({ "tree": tree_entries });
        if let Some((_, base_tree)) = &parent {
            tree_body["base_tree"] = json!(base_tree);
        }
        let tree: GitObject = self
            .post_json(
                &format!("{API}/repos/{}/git/trees", repo.slug()),
                &tree_body,
                "создание дерева",
            )
            .await?;

        let mut commit_body = json!({
            "message": if request.commit_message.trim().is_empty() { "Publish Odium Launcher pack" } else { request.commit_message.trim() },
            "tree": tree.sha,
        });
        if let Some((parent_sha, _)) = &parent {
            commit_body["parents"] = json!([parent_sha]);
        }
        let commit: GitObject = self
            .post_json(
                &format!("{API}/repos/{}/git/commits", repo.slug()),
                &commit_body,
                "создание коммита",
            )
            .await?;

        if parent.is_some() {
            let body = json!({ "sha": commit.sha, "force": false });
            let url = format!("{API}/repos/{}/git/refs/heads/{branch}", repo.slug());
            let (status, text) = self
                .send_raw(self.request(Method::PATCH, &url).json(&body))
                .await?;
            if !status.is_success() {
                return Err(api_error("обновление ветки", status, &text));
            }
        } else {
            let body = json!({ "ref": format!("refs/heads/{branch}"), "sha": commit.sha });
            let (status, text) = self
                .send_raw(
                    self.request(
                        Method::POST,
                        &format!("{API}/repos/{}/git/refs", repo.slug()),
                    )
                    .json(&body),
                )
                .await?;
            if !status.is_success() {
                return Err(api_error("создание ветки", status, &text));
            }
        }

        stage(&report, "Готово");
        Ok(PublishResult {
            pack_url: format!("{raw_base}/manifest.json"),
            page_url: format!("https://github.com/{}/tree/{branch}", repo.slug()),
        })
    }

    async fn post_json<T: DeserializeOwned>(
        &self,
        url: &str,
        body: &Value,
        context: &str,
    ) -> Result<T, PublishError> {
        let (status, text) = self
            .send_raw(self.request(Method::POST, url).json(body))
            .await?;
        if !status.is_success() {
            return Err(api_error(context, status, &text));
        }
        parse_json(&text)
    }

    async fn create_blob(&self, repo: &Repo, bytes: &[u8]) -> Result<String, PublishError> {
        let body = json!({
            "content": base64::engine::general_purpose::STANDARD.encode(bytes),
            "encoding": "base64",
        });
        let blob: GitObject = self
            .post_json(
                &format!("{API}/repos/{}/git/blobs", repo.slug()),
                &body,
                "загрузка файла",
            )
            .await?;
        Ok(blob.sha)
    }

    /// Returns `(commit_sha, tree_sha)` for the branch, creating it from the
    /// repository default branch when needed. `None` means an empty repository.
    async fn branch_state(
        &self,
        repo: &Repo,
        branch: &str,
        report: &Reporter,
    ) -> Result<Option<(String, String)>, PublishError> {
        let url = format!("{API}/repos/{}/git/ref/heads/{branch}", repo.slug());
        let (status, text) = self.send_raw(self.request(Method::GET, &url)).await?;
        if status.is_success() {
            #[derive(Deserialize)]
            struct GitRef {
                object: GitObject,
            }
            let reference: GitRef = parse_json(&text)?;
            return self.commit_tree(repo, &reference.object.sha).await;
        }
        if status != StatusCode::NOT_FOUND && status != StatusCode::CONFLICT {
            return Err(api_error("чтение ветки", status, &text));
        }

        // The branch does not exist yet. If the repository has another default
        // branch, branch off it; otherwise this is an empty repository.
        let (status, text) = self
            .send_raw(self.request(Method::GET, &format!("{API}/repos/{}", repo.slug())))
            .await?;
        if !status.is_success() {
            return Err(api_error("чтение репозитория", status, &text));
        }
        #[derive(Deserialize)]
        struct RepoInfo {
            #[serde(default)]
            default_branch: String,
        }
        let info: RepoInfo = parse_json(&text)?;
        if info.default_branch.is_empty() || info.default_branch == branch {
            return Ok(None);
        }

        let default_url = format!(
            "{API}/repos/{}/git/ref/heads/{}",
            repo.slug(),
            info.default_branch
        );
        let (status, text) = self
            .send_raw(self.request(Method::GET, &default_url))
            .await?;
        if !status.is_success() {
            return Ok(None);
        }
        #[derive(Deserialize)]
        struct GitRef {
            object: GitObject,
        }
        let reference: GitRef = parse_json(&text)?;
        stage(
            report,
            format!("Создание ветки {branch} от {}…", info.default_branch),
        );
        let body = json!({ "ref": format!("refs/heads/{branch}"), "sha": reference.object.sha });
        let (status, text) = self
            .send_raw(
                self.request(
                    Method::POST,
                    &format!("{API}/repos/{}/git/refs", repo.slug()),
                )
                .json(&body),
            )
            .await?;
        if !status.is_success() {
            return Err(api_error("создание ветки", status, &text));
        }
        self.commit_tree(repo, &reference.object.sha).await
    }

    async fn commit_tree(
        &self,
        repo: &Repo,
        commit_sha: &str,
    ) -> Result<Option<(String, String)>, PublishError> {
        let url = format!("{API}/repos/{}/git/commits/{commit_sha}", repo.slug());
        let (status, text) = self.send_raw(self.request(Method::GET, &url)).await?;
        if !status.is_success() {
            return Err(api_error("чтение коммита", status, &text));
        }
        #[derive(Deserialize)]
        struct Commit {
            tree: GitObject,
        }
        let commit: Commit = parse_json(&text)?;
        Ok(Some((commit_sha.to_owned(), commit.tree.sha)))
    }
}

#[derive(Deserialize)]
struct Release {
    id: u64,
    upload_url: String,
    html_url: String,
}

#[derive(Deserialize)]
struct Asset {
    id: u64,
    name: String,
    browser_download_url: String,
}

#[derive(Deserialize)]
struct GitObject {
    sha: String,
}

enum BlobSource {
    Bytes(Vec<u8>),
    Path(PathBuf),
}

fn parse_json<T: DeserializeOwned>(text: &str) -> Result<T, PublishError> {
    serde_json::from_str(text).map_err(PublishError::Json)
}

fn api_error(context: &str, status: StatusCode, body: &str) -> PublishError {
    let snippet: String = body.chars().take(400).collect();
    PublishError::Api(format!("{context}: HTTP {status} {snippet}"))
}

fn collect_overrides(root: &Path) -> Result<BTreeMap<String, PathBuf>, PublishError> {
    let mut files = BTreeMap::new();
    if !root.is_dir() {
        return Ok(files);
    }
    for entry in walkdir::WalkDir::new(root) {
        let entry = entry.map_err(|error| PublishError::Io(error.into()))?;
        if !entry.file_type().is_file() {
            continue;
        }
        let relative = entry
            .path()
            .strip_prefix(root)
            .map_err(|_| PublishError::MissingFile(root.display().to_string()))?
            .to_string_lossy()
            .replace('\\', "/");
        files.insert(relative, entry.path().to_path_buf());
    }
    Ok(files)
}

/// Rewrites hosted-file URLs in the manifest and appends override files that
/// are missing from it. Returns `(rewritten, added)`.
async fn rewrite_manifest(
    manifest: &mut Value,
    raw_base: &str,
    overrides: &BTreeMap<String, PathBuf>,
) -> Result<(usize, usize), PublishError> {
    let mut rewritten = 0;
    for key in ["files", "optional_files"] {
        if let Some(array) = manifest.get_mut(key).and_then(Value::as_array_mut) {
            for entry in array.iter_mut() {
                let Some(path) = entry.get("path").and_then(Value::as_str) else {
                    continue;
                };
                if let Some(local) = overrides.get(path) {
                    entry["url"] = json!(format!("{raw_base}/overrides/{}", encode_path(path)));
                    // The URL now points at the freshly uploaded override, not
                    // at the original third-party file, so the old sha256/size
                    // would fail verification on every client.
                    entry["sha256"] =
                        json!(pml_downloader::hash_file(local, HashAlgo::Sha256).await?);
                    entry["size"] = json!(std::fs::metadata(local)?.len());
                    rewritten += 1;
                }
            }
        }
    }

    let mut added = 0;
    for (relative, path) in overrides {
        let listed = ["files", "optional_files"].iter().any(|key| {
            manifest
                .get(*key)
                .and_then(Value::as_array)
                .map(|array| {
                    array
                        .iter()
                        .any(|entry| entry.get("path").and_then(Value::as_str) == Some(relative))
                })
                .unwrap_or(false)
        });
        if listed {
            continue;
        }
        if !manifest.get("files").map(Value::is_array).unwrap_or(false) {
            manifest["files"] = json!([]);
        }
        let sha256 = pml_downloader::hash_file(path, HashAlgo::Sha256).await?;
        let size = std::fs::metadata(path)?.len();
        let entry = json!({
            "path": relative,
            "url": format!("{raw_base}/overrides/{}", encode_path(relative)),
            "sha256": sha256,
            "size": size,
        });
        if let Some(array) = manifest.get_mut("files").and_then(Value::as_array_mut) {
            array.push(entry);
            added += 1;
        }
    }
    Ok((rewritten, added))
}

/// Percent-encodes each path segment so spaces, brackets and backticks produce
/// a valid raw GitHub URL.
pub fn encode_path(path: &str) -> String {
    path.split('/')
        .map(|segment| {
            let mut out = String::with_capacity(segment.len());
            for byte in segment.bytes() {
                if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
                    out.push(byte as char);
                } else {
                    out.push_str(&format!("%{byte:02X}"));
                }
            }
            out
        })
        .collect::<Vec<_>>()
        .join("/")
}

pub fn sanitize_tag(tag: &str) -> String {
    let cleaned: String = tag
        .trim()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '+') {
                c
            } else {
                '-'
            }
        })
        .collect();
    let cleaned = cleaned.trim_matches('-').to_owned();
    if cleaned.is_empty() {
        "v1.0.0".to_owned()
    } else {
        cleaned
    }
}

pub fn sanitize_branch(branch: &str) -> String {
    let cleaned: String = branch
        .trim()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '/') {
                c
            } else {
                '-'
            }
        })
        .collect();
    let cleaned = cleaned.trim_matches('/').to_owned();
    if cleaned.is_empty() || cleaned.contains("..") {
        "main".to_owned()
    } else {
        cleaned
    }
}

pub fn sanitize_asset_name(name: &str) -> String {
    let mut cleaned = String::new();
    let mut previous_dash = false;
    for character in name.trim().chars() {
        let allowed = character.is_ascii_alphanumeric()
            || matches!(character, '-' | '_' | '.' | '+')
            || !character.is_ascii();
        if allowed {
            cleaned.push(character);
            previous_dash = false;
        } else if !previous_dash {
            cleaned.push('-');
            previous_dash = true;
        }
    }
    let cleaned = cleaned.trim_matches('-').to_owned();
    if cleaned.is_empty() {
        "pack.mrpack".to_owned()
    } else if cleaned.to_ascii_lowercase().ends_with(".mrpack") {
        cleaned
    } else {
        format!("{cleaned}.mrpack")
    }
}

/// Turns a repository name into a valid `Source.id` (`[a-z0-9_-]`).
pub fn sanitize_source_id(name: &str) -> String {
    let mut cleaned = String::new();
    let mut previous_dash = false;
    for character in name.trim().to_ascii_lowercase().chars() {
        let allowed = character.is_ascii_alphanumeric() || matches!(character, '-' | '_');
        if allowed {
            cleaned.push(character);
            previous_dash = false;
        } else if !previous_dash {
            cleaned.push('-');
            previous_dash = true;
        }
    }
    let cleaned = cleaned.trim_matches('-').to_owned();
    if cleaned.is_empty() {
        "pack".to_owned()
    } else {
        cleaned
    }
}

/// Everything needed to build a player-ready `dist/` folder.
#[derive(Debug, Clone)]
pub struct DistRequest {
    pub output_dir: PathBuf,
    pub source_id: String,
    pub source_name: String,
    pub pack_url: String,
    pub kind: SourceKind,
    pub server: ServerSpec,
    pub xms: u32,
    pub xmx: u32,
    pub pack_label: String,
    pub exe: Option<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct DistResult {
    pub dir: PathBuf,
    pub zip: PathBuf,
    pub exe_copied: bool,
}

/// Guards the one recursive delete in [`build_dist`]. The output folder is
/// replaced wholesale, so an accidental `Documents` or the repository root in
/// the path field must be refused instead of erased.
fn ensure_dist_target(dir: &Path) -> Result<(), PublishError> {
    let Some(name) = dir.file_name().and_then(|name| name.to_str()) else {
        return Err(PublishError::MissingFile(format!(
            "некорректная папка сборки: {}",
            dir.display()
        )));
    };
    if name.is_empty() || name == "." || name == ".." || dir.parent().is_none() {
        return Err(PublishError::MissingFile(format!(
            "некорректная папка сборки: {}",
            dir.display()
        )));
    }
    // Refuse a folder that contains the working directory or the running
    // executable: the player never wants either of them deleted.
    let canonical = std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
    if let Ok(cwd) = std::env::current_dir() {
        let cwd = std::fs::canonicalize(&cwd).unwrap_or(cwd);
        if cwd.starts_with(&canonical) {
            return Err(PublishError::MissingFile(format!(
                "папка сборки {} содержит текущий каталог",
                dir.display()
            )));
        }
    }
    if let Ok(exe) = std::env::current_exe() {
        let exe = std::fs::canonicalize(&exe).unwrap_or(exe);
        if exe.starts_with(&canonical) {
            return Err(PublishError::MissingFile(format!(
                "папка сборки {} содержит запущенный лаунчер",
                dir.display()
            )));
        }
    }
    if dir.exists() {
        // Only replace an earlier dist build. Any other existing folder was
        // selected by mistake and is left for the user to inspect.
        let looks_like_dist = dir.join(pml_config::PORTABLE_MARKER).is_file()
            || dir.join("launcher_data").is_dir()
            || dir.join("OdiumLauncher.exe").is_file();
        if !looks_like_dist {
            return Err(PublishError::MissingFile(format!(
                "папка {} не похожа на предыдущую сборку; удалите её вручную",
                dir.display()
            )));
        }
    }
    Ok(())
}

/// Writes `launcher_data/config.json`, copies the launcher executable, writes
/// `README.txt` and packs everything into a ZIP next to the output folder.
pub fn build_dist(request: &DistRequest) -> Result<DistResult, PublishError> {
    let dir = request.output_dir.clone();
    ensure_dist_target(&dir)?;
    let parent = dir.parent().unwrap_or_else(|| Path::new("."));
    let Some(name) = dir.file_name() else {
        return Err(PublishError::MissingFile(format!(
            "некорректная папка сборки: {}",
            dir.display()
        )));
    };
    // Build into a sibling staging folder, then swap it in. A failed build never
    // leaves a half-written dist, and a successful build never ships files from
    // a previous run.
    let staging = parent.join(format!(".{}.building", name.to_string_lossy()));
    if staging.exists() {
        std::fs::remove_dir_all(&staging)?;
    }
    let launcher_data = staging.join("launcher_data");
    std::fs::create_dir_all(&launcher_data)?;

    let config = Config {
        nick: "Player".to_owned(),
        sources: vec![Source {
            id: request.source_id.clone(),
            name: request.source_name.clone(),
            manifest_url: request.pack_url.clone(),
            kind: Some(request.kind),
        }],
        active_source_id: request.source_id.clone(),
        jvm: pml_config::JvmConfig {
            xms: request.xms,
            xmx: request.xmx,
            // The player's launcher replaces this with half of *their* RAM on
            // first start, so a dist built on a 32 GiB machine is still safe
            // on an 8 GiB laptop.
            auto_xmx: true,
        },
        server: request.server.clone(),
        close_launcher_on_start: true,
        ..Config::default()
    };
    save_json_atomic(&launcher_data.join("config.json"), &config)?;

    let exe_source = match &request.exe {
        Some(path) => path.clone(),
        None => std::env::current_exe()?,
    };
    let mut exe_copied = false;
    if exe_source.is_file() {
        std::fs::copy(&exe_source, staging.join("OdiumLauncher.exe"))?;
        exe_copied = true;
    }
    // A player folder must stay self-contained: without the marker the
    // launcher would move `launcher_data` into %LOCALAPPDATA% on first start,
    // which breaks the "copy the folder to another PC" promise.
    std::fs::write(
        staging.join(pml_config::PORTABLE_MARKER),
        "Odium Launcher portable build\n",
    )?;

    let readme = format!(
        "Odium Launcher pack\r\n\
         ===================\r\n\r\n\
         Сборка:    {}\r\n\
         Параметры: {}\r\n\r\n\
         Как играть:\r\n\
         1. Держите OdiumLauncher.exe, portable.flag и папку launcher_data рядом.\r\n\
         2. Запустите OdiumLauncher.exe.\r\n\
         3. Введите ник и нажмите «Играть».\r\n\r\n\
         portable.flag заставляет лаунчер хранить все данные в launcher_data\r\n\
         рядом с exe, поэтому папку можно целиком перенести на другой компьютер.\r\n\r\n\
         Лаунчер сам скачает Java, загрузчик, моды и конфиги.\r\n\
         Сервер добавится в список сетевых игр.\r\n",
        request.source_name, request.pack_label
    );
    std::fs::write(staging.join("README.txt"), readme)?;
    write_checksums(&staging)?;

    let zip_path = parent.join(format!("OdiumLauncher-{}.zip", request.source_id));
    write_zip(&staging, &zip_path)?;
    // A second copy of the digest next to the archive lets a user verify the
    // download before extracting it, without trusting the archive contents.
    let zip_hash = pml_downloader::hash_file_blocking(&zip_path, pml_downloader::HashAlgo::Sha256)?;
    std::fs::write(
        zip_path.with_extension("zip.sha256"),
        format!(
            "{zip_hash}  {}\n",
            zip_path.file_name().unwrap_or_default().to_string_lossy()
        ),
    )?;

    if dir.exists() {
        std::fs::remove_dir_all(&dir)?;
    }
    std::fs::rename(&staging, &dir)?;

    Ok(DistResult {
        dir,
        zip: zip_path,
        exe_copied,
    })
}

fn write_checksums(root: &Path) -> Result<(), PublishError> {
    use pml_downloader::{hash_file_blocking, HashAlgo};

    let mut entries = Vec::new();
    for entry in walkdir::WalkDir::new(root).follow_links(false) {
        let entry = entry.map_err(|error| PublishError::Io(error.into()))?;
        if !entry.file_type().is_file() {
            continue;
        }
        if entry
            .file_name()
            .to_string_lossy()
            .eq_ignore_ascii_case("SHA256SUMS.txt")
        {
            continue;
        }
        let relative = entry
            .path()
            .strip_prefix(root)
            .unwrap_or_else(|_| entry.path())
            .to_string_lossy()
            .replace('\\', "/");
        let hash = hash_file_blocking(entry.path(), HashAlgo::Sha256)?;
        entries.push((relative, hash));
    }
    // Sort by path, matching the conventional `sha256sum -c` friendly layout.
    entries.sort_by(|left, right| left.0.cmp(&right.0));
    let body = entries
        .into_iter()
        .map(|(path, hash)| format!("{hash}  {path}"))
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(root.join("SHA256SUMS.txt"), body + "\n")?;
    Ok(())
}

fn write_zip(dir: &Path, zip_path: &Path) -> Result<(), PublishError> {
    if zip_path.exists() {
        std::fs::remove_file(zip_path)?;
    }
    let file = std::fs::File::create(zip_path)?;
    let mut writer = zip::ZipWriter::new(file);
    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    for entry in walkdir::WalkDir::new(dir) {
        let entry = entry.map_err(|error| PublishError::Io(error.into()))?;
        if !entry.file_type().is_file() {
            continue;
        }
        let relative = entry
            .path()
            .strip_prefix(dir)
            .map_err(|_| PublishError::MissingFile(dir.display().to_string()))?
            .to_string_lossy()
            .replace('\\', "/");
        writer
            .start_file(relative, options)
            .map_err(|error| PublishError::Zip(error.to_string()))?;
        let mut source = std::fs::File::open(entry.path())?;
        std::io::copy(&mut source, &mut writer)?;
    }
    writer
        .finish()
        .map_err(|error| PublishError::Zip(error.to_string()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repo_parsing_accepts_common_forms() {
        assert_eq!(Repo::parse("name/repo").expect("plain").slug(), "name/repo");
        assert_eq!(
            Repo::parse("https://github.com/name/repo.git")
                .expect("url")
                .slug(),
            "name/repo"
        );
        assert!(Repo::parse("only-owner").is_err());
        assert!(Repo::parse("bad owner/repo").is_err());
    }

    #[test]
    fn retry_delay_has_jitter_bounds() {
        for attempt in 0..6 {
            let base = 500u64 << attempt;
            let spread = base / 5;
            for _ in 0..32 {
                let millis = retry_delay(attempt).as_millis() as u64;
                assert!(
                    (base - spread..=base + spread).contains(&millis),
                    "attempt {attempt}: {millis} ms outside ±20% of {base} ms"
                );
            }
        }
    }

    #[test]
    fn path_encoding_keeps_segments() {
        assert_eq!(encode_path("config/a b.json5"), "config/a%20b.json5");
        assert_eq!(
            encode_path("shaderpacks/[BTB] Back.zip"),
            "shaderpacks/%5BBTB%5D%20Back.zip"
        );
        assert_eq!(encode_path("mods/simple.jar"), "mods/simple.jar");
    }

    #[test]
    fn sanitizers_produce_valid_values() {
        assert_eq!(sanitize_tag(" release 1.0 "), "release-1.0");
        assert_eq!(sanitize_tag(""), "v1.0.0");
        assert_eq!(sanitize_branch("feature/new pack"), "feature/new-pack");
        assert_eq!(sanitize_branch(""), "main");
        assert_eq!(
            sanitize_asset_name("EblyaCraft Don's Edit 2.3.1.mrpack"),
            "EblyaCraft-Don-s-Edit-2.3.1.mrpack"
        );
        assert_eq!(sanitize_source_id("My_Pack 2"), "my_pack-2");
    }

    #[tokio::test]
    async fn manifest_rewrite_links_overrides() {
        let temp = tempfile::tempdir().expect("temp");
        let override_path = temp.path().join("config").join("example.toml");
        std::fs::create_dir_all(override_path.parent().expect("parent")).expect("dir");
        std::fs::write(&override_path, b"hello").expect("write");
        let overrides = BTreeMap::from([("config/example.toml".to_owned(), override_path)]);

        let mut manifest = json!({
            "schema_version": 1,
            "name": "Pack",
            "mc_version": "1.21.1",
            "loader": {"kind": "fabric", "version": "0.16.5"},
            "files": [
                {"path": "config/example.toml", "url": "https://old", "sha256": "a".repeat(64), "size": 1},
                {"path": "mods/mod.jar", "url": "https://cdn", "sha256": "b".repeat(64), "size": 2}
            ],
            "optional_files": []
        });
        let (rewritten, added) = rewrite_manifest(
            &mut manifest,
            "https://raw.githubusercontent.com/o/r/main",
            &overrides,
        )
        .await
        .expect("rewrite");
        assert_eq!(rewritten, 1);
        assert_eq!(added, 0);
        assert_eq!(
            manifest["files"][0]["url"],
            "https://raw.githubusercontent.com/o/r/main/overrides/config/example.toml"
        );
        assert_eq!(
            manifest["files"][0]["sha256"],
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
        assert_eq!(manifest["files"][0]["size"], 5);

        let mut second = json!({"files": []});
        let (_, added) = rewrite_manifest(
            &mut second,
            "https://raw.githubusercontent.com/o/r/main",
            &BTreeMap::from([(
                "config/example.toml".to_owned(),
                temp.path().join("config/example.toml"),
            )]),
        )
        .await
        .expect("rewrite");
        assert_eq!(added, 1);
        assert_eq!(second["files"][0]["path"], "config/example.toml");
    }

    #[test]
    fn dist_contains_config_readme_and_zip() {
        let temp = tempfile::tempdir().expect("temp");
        let fake_exe = temp.path().join("fake.exe");
        std::fs::write(&fake_exe, b"exe").expect("write");
        let output = temp.path().join("dist");
        std::fs::create_dir_all(&output).expect("old dist");
        std::fs::write(output.join(pml_config::PORTABLE_MARKER), b"old build").expect("marker");
        std::fs::write(output.join("stale-from-previous-build.txt"), b"leak").expect("stale");
        let result = build_dist(&DistRequest {
            output_dir: output.clone(),
            source_id: "test-pack".to_owned(),
            source_name: "Test pack 1.0.0".to_owned(),
            pack_url: "https://example.com/pack.mrpack".to_owned(),
            kind: SourceKind::Mrpack,
            server: ServerSpec {
                ip: "127.0.0.1".to_owned(),
                port: 25565,
            },
            xms: 2048,
            xmx: 6144,
            pack_label: "Minecraft 1.21.1 · NeoForge 21.1.248".to_owned(),
            exe: Some(fake_exe),
        })
        .expect("dist");
        assert!(result.exe_copied);
        assert!(output.join("OdiumLauncher.exe").is_file());
        assert!(output.join("README.txt").is_file());
        let config =
            std::fs::read_to_string(output.join("launcher_data/config.json")).expect("config");
        assert!(config.contains("\"mrpack\""));
        assert!(config.contains("https://example.com/pack.mrpack"));
        assert!(config.contains("\"auto_xmx\": true"));
        assert!(
            !output.join("stale-from-previous-build.txt").exists(),
            "files from the previous build must not leak into the new dist"
        );
        assert!(result.zip.is_file());
        let sums = std::fs::read_to_string(output.join("SHA256SUMS.txt")).expect("checksums");
        assert!(sums.contains("OdiumLauncher.exe"));
        assert!(sums.contains("README.txt"));
        assert!(temp
            .path()
            .join("OdiumLauncher-test-pack.zip.sha256")
            .is_file());
    }

    #[test]
    fn publisher_rejects_blank_or_header_breaking_tokens() {
        for token in ["", "   ", "\n", "ghp token", "ghp_token\r\nX-Evil: 1"] {
            assert!(
                matches!(Publisher::new(token), Err(PublishError::Token(_))),
                "token {token:?} must be rejected"
            );
        }
        assert!(Publisher::new("  ghp_example_token  ").is_ok());
    }

    #[test]
    fn build_dist_refuses_an_unrelated_folder() {
        let temp = tempfile::tempdir().expect("temp");
        let output = temp.path().join("Documents");
        std::fs::create_dir_all(&output).expect("dir");
        std::fs::write(output.join("important.txt"), b"keep").expect("file");
        let request = DistRequest {
            output_dir: output.clone(),
            source_id: "test-pack".to_owned(),
            source_name: "Test pack".to_owned(),
            pack_url: "https://example.com/pack.mrpack".to_owned(),
            kind: SourceKind::Mrpack,
            server: ServerSpec::default(),
            xms: 2048,
            xmx: 6144,
            pack_label: "test".to_owned(),
            exe: None,
        };
        let error = build_dist(&request).expect_err("must refuse");
        assert!(matches!(error, PublishError::MissingFile(_)));
        assert!(output.join("important.txt").is_file());
    }
}

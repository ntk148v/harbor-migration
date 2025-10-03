use anyhow::{Context, Result};
use base64::{Engine as _, engine::general_purpose};
use chrono::{DateTime, Utc};
use clap::Parser;
use futures::stream::{self, StreamExt};
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderValue};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs;
use std::path::Path;
use std::sync::Arc;
use tokio::sync::Semaphore;

#[derive(Parser, Debug)]
#[command(name = "harbor-migrator")]
#[command(about = "Migrate Harbor repositories between instances", long_about = None)]
struct Args {
    /// Path to config file
    #[arg(short, long)]
    config: Option<String>,

    /// Use environment variables for configuration
    #[arg(long)]
    env: bool,

    /// Generate example config file
    #[arg(long)]
    generate_config: Option<String>,

    /// Dry run mode - show what would be migrated without actually migrating
    #[arg(long)]
    dry_run: bool,

    /// Maximum number of concurrent image migrations
    #[arg(long, default_value = "3")]
    concurrency: usize,

    /// Path to state file for resume capability
    #[arg(long, default_value = "migration_state.json")]
    state_file: String,

    /// Resume from previous migration state
    #[arg(long)]
    resume: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct HarborConfig {
    url: String,
    username: String,
    password: String,
}

impl HarborConfig {
    fn new(url: String, username: String, password: String) -> Self {
        Self {
            url: url.trim_end_matches('/').to_string(),
            username,
            password,
        }
    }

    fn basic_auth(&self) -> String {
        let credentials = format!("{}:{}", self.username, self.password);
        let encoded = general_purpose::STANDARD.encode(credentials.as_bytes());
        format!("Basic {}", encoded)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ProjectConfig {
    source: String,
    destination: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct Config {
    source: HarborConfig,
    destination: HarborConfig,
    projects: Vec<ProjectConfig>,
}

impl Config {
    fn from_file(path: &str) -> Result<Self> {
        let content =
            fs::read_to_string(path).context(format!("Failed to read config file: {}", path))?;
        let config: Config =
            serde_json::from_str(&content).context("Failed to parse config file as JSON")?;
        Ok(config)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ImageRef {
    project: ProjectConfig,
    repository: String,
    tag: String,
}

impl ImageRef {
    fn to_string(&self) -> String {
        format!(
            "(source){}/{}:{}->(destination){}/{}:{}",
            self.project.source,
            self.repository,
            self.tag,
            self.project.destination,
            self.repository,
            self.tag
        )
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct MigrationState {
    started_at: DateTime<Utc>,
    last_updated: DateTime<Utc>,
    completed: HashSet<String>,
    failed: Vec<(String, String)>, // (image_ref, error_message)
    total_images: usize,
}

impl MigrationState {
    fn new() -> Self {
        let now = Utc::now();
        Self {
            started_at: now,
            last_updated: now,
            completed: HashSet::new(),
            failed: Vec::new(),
            total_images: 0,
        }
    }

    fn load(path: &Path) -> Result<Self> {
        let content = fs::read_to_string(path)?;
        let state: MigrationState = serde_json::from_str(&content)?;
        Ok(state)
    }

    fn save(&self, path: &Path) -> Result<()> {
        let json = serde_json::to_string_pretty(self)?;
        fs::write(path, json)?;
        Ok(())
    }

    fn mark_completed(&mut self, image_ref: &str) {
        self.completed.insert(image_ref.to_string());
        self.last_updated = Utc::now();
    }

    fn mark_failed(&mut self, image_ref: &str, error: &str) {
        self.failed.push((image_ref.to_string(), error.to_string()));
        self.last_updated = Utc::now();
    }

    fn is_completed(&self, image_ref: &str) -> bool {
        self.completed.contains(image_ref)
    }
}

#[derive(Debug, Deserialize)]
struct Repository {
    name: String,
}

#[derive(Debug, Deserialize)]
struct Artifact {
    tags: Option<Vec<Tag>>,
}

#[derive(Debug, Deserialize)]
struct Tag {
    name: String,
}

struct HarborClient {
    client: reqwest::Client,
    config: HarborConfig,
}

impl HarborClient {
    fn new(config: HarborConfig) -> Result<Self> {
        let mut headers = HeaderMap::new();
        headers.insert(AUTHORIZATION, HeaderValue::from_str(&config.basic_auth())?);
        let client = reqwest::Client::builder()
            .default_headers(headers)
            .build()?;
        Ok(Self { client, config })
    }

    async fn list_repositories(&self, project_name: &str) -> Result<Vec<Repository>> {
        let mut all_repos = Vec::new();
        let mut page = 1;
        let page_size = 100;

        loop {
            let url = format!(
                "{}/api/v2.0/projects/{}/repositories?page={}&page_size={}",
                self.config.url, project_name, page, page_size
            );
            let response = self.client.get(&url).send().await?;
            if !response.status().is_success() {
                anyhow::bail!("Failed to list repositories: {}", response.status());
            }

            let mut repos: Vec<Repository> = response.json().await?;
            // remove the project part
            repos.iter_mut().for_each(|r| {
                if let Some((_, rest)) = r.name.split_once('/') {
                    r.name = rest.to_string();
                }
            });
            let is_last_page = repos.len() < page_size;
            all_repos.extend(repos);

            if is_last_page {
                break;
            }
            page += 1;
        }

        Ok(all_repos)
    }

    async fn list_artifacts(
        &self,
        project_name: &str,
        repository_name: &str,
    ) -> Result<Vec<Artifact>> {
        // Double encode the repository name: / -> %2F -> %252F
        let encoded_repo = repository_name.split('/').collect::<Vec<_>>().join("%252F");

        let mut all_artifacts = Vec::new();
        let mut page = 1;
        let page_size = 100;

        loop {
            let url = format!(
                "{}/api/v2.0/projects/{}/repositories/{}/artifacts?page={}&page_size={}&with_tag=true",
                self.config.url, project_name, encoded_repo, page, page_size
            );
            let response = self.client.get(&url).send().await?;
            if !response.status().is_success() {
                anyhow::bail!("Failed to list artifacts: {}", response.status());
            }

            let artifacts: Vec<Artifact> = response.json().await?;
            let is_last_page = artifacts.len() < page_size;
            all_artifacts.extend(artifacts);

            if is_last_page {
                break;
            }
            page += 1;
        }

        Ok(all_artifacts)
    }
}

struct Migrator {
    source: Arc<HarborClient>,
    destination: Arc<HarborClient>,
    state: Arc<tokio::sync::Mutex<MigrationState>>,
    state_file: String,
    dry_run: bool,
    multi_progress: MultiProgress,
}

impl Migrator {
    fn new(
        source_config: HarborConfig,
        dest_config: HarborConfig,
        state_file: String,
        dry_run: bool,
    ) -> Result<Self> {
        Ok(Self {
            source: Arc::new(HarborClient::new(source_config)?),
            destination: Arc::new(HarborClient::new(dest_config)?),
            state: Arc::new(tokio::sync::Mutex::new(MigrationState::new())),
            state_file,
            dry_run,
            multi_progress: MultiProgress::new(),
        })
    }

    fn load_state(&self) -> Result<()> {
        let path = Path::new(&self.state_file);
        if path.exists() {
            let loaded_state = MigrationState::load(path)?;
            let mut state = futures::executor::block_on(self.state.lock());
            *state = loaded_state;
            Ok(())
        } else {
            Ok(())
        }
    }

    async fn save_state(&self) -> Result<()> {
        let state = self.state.lock().await;
        state.save(Path::new(&self.state_file))
    }

    async fn collect_all_images(&self, projects: &[ProjectConfig]) -> Result<Vec<ImageRef>> {
        let pb = self.multi_progress.add(ProgressBar::new_spinner());
        pb.set_style(
            ProgressStyle::default_spinner()
                .template("{spinner:.green} {msg}")
                .unwrap(),
        );
        pb.set_message("Discovering images...");

        let mut all_images = Vec::new();

        for project in projects {
            pb.set_message(format!("Scanning project: {}", project.source));
            let repositories = self.source.list_repositories(&project.source).await?;

            for repo in repositories {
                pb.set_message(format!("Scanning repository: {}", repo.name));
                let artifacts = self
                    .source
                    .list_artifacts(&project.source, &repo.name)
                    .await?;

                for artifact in artifacts {
                    if let Some(tags) = artifact.tags {
                        for tag in tags {
                            all_images.push(ImageRef {
                                project: ProjectConfig {
                                    source: project.source.clone(),
                                    destination: project.destination.clone(),
                                },
                                repository: repo.name.clone(),
                                tag: tag.name,
                            });
                        }
                    }
                }
            }
        }

        pb.finish_with_message(format!("Found {} images", all_images.len()));
        Ok(all_images)
    }

    async fn migrate_image(&self, image: ImageRef, pb: ProgressBar) -> Result<()> {
        let image_ref = image.to_string();

        // Check if already completed
        {
            let state = self.state.lock().await;
            if state.is_completed(&image_ref) {
                pb.set_message(format!("⏭️  Skipped (already completed): {}", image_ref));
                pb.inc(1);
                return Ok(());
            }
        }

        pb.set_message(format!("🔄 Migrating: {}:{}", image.repository, image.tag));

        if self.dry_run {
            pb.set_message(format!(
                "✓ [DRY RUN] Would migrate: {}:{}",
                image.repository, image.tag
            ));
            pb.inc(1);
            return Ok(());
        }

        let source_image = format!(
            "{}/{}:{}",
            self.source
                .config
                .url
                .replace("https://", "")
                .replace("http://", ""),
            format!("{}/{}", image.project.source, image.repository),
            image.tag
        );

        let dest_image = format!(
            "{}/{}:{}",
            self.destination
                .config
                .url
                .replace("https://", "")
                .replace("http://", ""),
            format!("{}/{}", image.project.destination, image.repository),
            image.tag
        );

        // Login to source
        let login_src = tokio::process::Command::new("docker")
            .args(&[
                "login",
                &self
                    .source
                    .config
                    .url
                    .replace("https://", "")
                    .replace("http://", ""),
                "-u",
                &self.source.config.username,
                "--password-stdin",
            ])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()?;

        if let Some(mut stdin) = login_src.stdin {
            use tokio::io::AsyncWriteExt;
            stdin
                .write_all(self.source.config.password.as_bytes())
                .await?;
        }

        // Pull image
        let pull = tokio::process::Command::new("docker")
            .args(&["pull", &source_image])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .output()
            .await?;

        if !pull.status.success() {
            anyhow::bail!(
                "Failed to pull image: {}",
                String::from_utf8_lossy(&pull.stderr)
            );
        }

        // Tag for destination
        tokio::process::Command::new("docker")
            .args(&["tag", &source_image, &dest_image])
            .status()
            .await?;

        // Login to destination
        let login_dst = tokio::process::Command::new("docker")
            .args(&[
                "login",
                &self
                    .destination
                    .config
                    .url
                    .replace("https://", "")
                    .replace("http://", ""),
                "-u",
                &self.destination.config.username,
                "--password-stdin",
            ])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()?;

        if let Some(mut stdin) = login_dst.stdin {
            use tokio::io::AsyncWriteExt;
            stdin
                .write_all(self.destination.config.password.as_bytes())
                .await?;
        }

        // Push image
        let push = tokio::process::Command::new("docker")
            .args(&["push", &dest_image])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .output()
            .await?;

        if !push.status.success() {
            anyhow::bail!(
                "Failed to push image: {}",
                String::from_utf8_lossy(&push.stderr)
            );
        }

        // Clean up
        tokio::process::Command::new("docker")
            .args(&["rmi", &source_image, &dest_image])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .await?;

        {
            let mut state = self.state.lock().await;
            state.mark_completed(&image_ref);
        }
        self.save_state().await?;

        pb.set_message(format!("✓ Completed: {}:{}", image.repository, image.tag));
        pb.inc(1);

        Ok(())
    }

    async fn migrate_projects(&self, projects: &[ProjectConfig], concurrency: usize) -> Result<()> {
        // Collect all images
        let all_images = self.collect_all_images(projects).await?;

        if all_images.is_empty() {
            println!("No images found to migrate.");
            return Ok(());
        }

        {
            let mut state = self.state.lock().await;
            state.total_images = all_images.len();
        }

        // Create progress bar
        let pb = self
            .multi_progress
            .add(ProgressBar::new(all_images.len() as u64));
        pb.set_style(
            ProgressStyle::default_bar()
                .template("{spinner:.green} [{elapsed_precise}] [{bar:40.cyan/blue}] {pos}/{len} ({eta}) {msg}")
                .unwrap()
                .progress_chars("#>-"),
        );

        // Create semaphore for concurrency control
        let semaphore = Arc::new(Semaphore::new(concurrency));

        // Migrate images concurrently
        let results: Vec<_> = stream::iter(all_images)
            .map(|image| {
                let pb = pb.clone();
                let semaphore = semaphore.clone();
                let self_clone = self.clone_for_task();
                async move {
                    let _permit = semaphore.acquire().await.unwrap();
                    let result = self_clone.migrate_image(image.clone(), pb).await;
                    (image, result)
                }
            })
            .buffer_unordered(concurrency)
            .collect()
            .await;

        pb.finish_with_message("Migration complete");

        // Handle errors
        let mut errors = Vec::new();
        for (image, result) in results {
            if let Err(e) = result {
                errors.push((image.to_string(), e.to_string()));
                let mut state = self.state.lock().await;
                state.mark_failed(&image.to_string(), &e.to_string());
            }
        }

        self.save_state().await?;

        // Print summary
        let state = self.state.lock().await;
        println!("\n=========================");
        println!("\n=   Migration Summary   =");
        println!("\n=========================");
        println!("Total images: {}", state.total_images);
        println!("Completed: {}", state.completed.len());
        println!("Failed: {}", state.failed.len());
        println!("\n=========================");
        println!("\n");

        if !state.failed.is_empty() {
            println!("\nFailed migrations:");
            for (img, err) in &state.failed {
                println!("  ❌ {}: {}", img, err);
            }
        }

        Ok(())
    }

    fn clone_for_task(&self) -> Self {
        Self {
            source: Arc::clone(&self.source),
            destination: Arc::clone(&self.destination),
            state: Arc::clone(&self.state),
            state_file: self.state_file.clone(),
            dry_run: self.dry_run,
            multi_progress: self.multi_progress.clone(),
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    // Load config
    let config = if let Some(config_path) = args.config {
        Config::from_file(&config_path)?
    } else {
        anyhow::bail!("Must provide --config <path>");
    };

    println!("🚀 Harbor Migration Tool");
    println!("Source: {}", config.source.url);
    println!("Destination: {}", config.destination.url);
    println!("Projects: {:?}", config.projects);
    println!("Concurrency: {}", args.concurrency);
    if args.dry_run {
        println!("⚠️  DRY RUN MODE - No actual migration will occur");
    }
    if args.resume {
        println!("📂 Resume mode enabled");
    }
    println!();

    let migrator = Migrator::new(
        config.source,
        config.destination,
        args.state_file,
        args.dry_run,
    )?;

    if args.resume {
        migrator.load_state()?;
        let state = migrator.state.lock().await;
        println!(
            "Loaded previous state: {} images already completed",
            state.completed.len()
        );
    }

    migrator
        .migrate_projects(&config.projects, args.concurrency)
        .await?;

    println!("\n✨ All done!");

    Ok(())
}
